//! Tests de bout en bout : on lance les vrais binaires, tels qu'un utilisateur
//! les lance.
//!
//! Les tests unitaires vérifient chaque brique isolément ; ceux-ci vérifient
//! l'assemblage — que `genlogs` écrit un fichier que `refrain` sait relire, qu'un
//! tube entre les deux marche aussi bien, que la sortie JSON est bien du JSON,
//! et que les codes de sortie sont ceux annoncés.
//!
//! `env!("CARGO_BIN_EXE_<nom>")` est fourni par Cargo : c'est le chemin du
//! binaire qu'il vient de compiler pour ce test.

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// Un dossier de travail propre, distinct par test.
fn dossier(nom: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("refrain-e2e-{}-{nom}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn genlogs(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_genlogs"))
        .args(args)
        .output()
        .expect("genlogs doit pouvoir démarrer")
}

fn refrain(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_refrain"))
        .args(args)
        .output()
        .expect("refrain doit pouvoir démarrer")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn de_la_generation_a_la_sortie_json() {
    let dir = dossier("json");
    // Volontairement niché : `genlogs` doit créer l'arborescence, comme il le
    // fait pour le `var/log/` canonique de Symfony sur un dépôt tout neuf.
    let log = dir.join("var").join("log").join("prod.log");
    let chemin = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "300", "--seed", "1", chemin]);
    assert!(out.status.success(), "genlogs a échoué : {}", stderr(&out));
    assert!(log.exists(), "le fichier de log doit avoir été créé");

    let out = refrain(&["--json", chemin]);
    assert!(out.status.success(), "refrain a échoué : {}", stderr(&out));

    let rapport: Value = serde_json::from_slice(&out.stdout).expect("la sortie doit être du JSON");

    let entrees = rapport["totals"]["entries"].as_u64().unwrap();
    assert!(entrees > 3000, "trop peu d'entrées analysées : {entrees}");
    assert_eq!(
        rapport["endpoints"].as_array().unwrap().len(),
        8,
        "les huit routes du générateur doivent ressortir"
    );
    assert_eq!(rapport["duration_source"]["kind"], "field");
    assert!(
        !rapport["nplus1"].as_array().unwrap().is_empty(),
        "les N+1 injectés par le générateur doivent être détectés"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn le_resume_texte_signale_les_n_plus_un() {
    let dir = dossier("resume");
    let log = dir.join("prod.log");
    let chemin = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "200", "--seed", "3", chemin]);
    assert!(out.status.success(), "genlogs a échoué : {}", stderr(&out));

    let out = refrain(&["--summary", chemin]);
    assert!(out.status.success(), "refrain a échoué : {}", stderr(&out));

    let resume = String::from_utf8_lossy(&out.stdout);
    assert!(resume.contains("Slowest endpoints"));
    assert!(resume.contains("N+1 patterns"));
    assert!(resume.contains("api_orders_list"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn un_tube_ferme_en_aval_n_est_pas_une_panne() {
    // `refrain --json --every 1 … | head -1`, ou un collecteur qui redémarre :
    // le lecteur s'en va, l'écriture suivante rend EPIPE. C'est la fin normale
    // d'un tube, pas une panne — et surtout pas le code 1, qui annonce « une
    // source n'a pas pu être lue » et ferait croire à un cron que les journaux
    // sont illisibles alors qu'ils viennent d'être lus.
    let dir = dossier("tube");
    let log = dir.join("prod.log");
    let chemin = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "50", "--seed", "7", chemin]);
    assert!(out.status.success(), "genlogs a échoué : {}", stderr(&out));

    let mut enfant = Command::new(env!("CARGO_BIN_EXE_refrain"))
        .args(["--json", "--every", "0.2", "--from-start", chemin])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("refrain doit pouvoir démarrer");

    let mut lecteur = BufReader::new(enfant.stdout.take().expect("refrain écrit sur sa sortie"));
    let mut premiere = String::new();
    lecteur
        .read_line(&mut premiere)
        .expect("le premier instantané doit arriver");
    assert!(premiere.starts_with('{'), "du NDJSON est attendu");

    // Le lecteur s'en va : c'est exactement ce que fait `head -1`.
    drop(lecteur);

    let out = enfant.wait_with_output().expect("refrain doit se terminer");
    assert!(
        out.status.success(),
        "un tube fermé doit rendre 0, pas {} — {}",
        out.status,
        stderr(&out)
    );
    assert_eq!(stderr(&out), "", "et ne rien dire sur la sortie d'erreur");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn l_entree_standard_est_analysable() {
    // `ssh prod tail -f … | refrain -` : le tube doit marcher comme un fichier. On
    // branche donc réellement les deux processus l'un sur l'autre — passer par
    // un fichier intermédiaire vérifierait tout sauf le chemin « - ».
    let mut source = Command::new(env!("CARGO_BIN_EXE_genlogs"))
        .args(["--rate", "0", "--count", "50", "--seed", "5"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("genlogs doit pouvoir démarrer");
    let tube = source.stdout.take().expect("genlogs écrit sur sa sortie");

    let out = Command::new(env!("CARGO_BIN_EXE_refrain"))
        .args(["--json", "-"])
        .stdin(Stdio::from(tube))
        .output()
        .expect("refrain doit pouvoir démarrer");

    let fin = source.wait().expect("genlogs doit se terminer");
    assert!(fin.success(), "genlogs a échoué");
    assert!(out.status.success(), "refrain a échoué : {}", stderr(&out));

    let rapport: Value = serde_json::from_slice(&out.stdout).expect("JSON valide");
    assert!(rapport["totals"]["entries"].as_u64().unwrap() > 200);
    assert_eq!(rapport["duration_source"]["kind"], "field");
    assert_eq!(
        rapport["endpoints"].as_array().unwrap().len(),
        8,
        "les huit routes du générateur doivent ressortir du tube"
    );
}

#[test]
fn n_restreint_le_rapport_a_la_fin_du_fichier() {
    // Sur un `prod.log` de plusieurs gigaoctets, « résume-moi la fin » doit
    // vraiment ne lire que la fin : `-n` était jusqu'ici ignoré en silence dans
    // les modes rapport, qui relisaient tout le fichier.
    let dir = dossier("dernieres-lignes");
    let log = dir.join("prod.log");
    let chemin = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "400", "--seed", "9", chemin]);
    assert!(out.status.success(), "genlogs a échoué : {}", stderr(&out));

    let entrees = |args: &[&str]| -> u64 {
        let out = refrain(args);
        assert!(out.status.success(), "refrain a échoué : {}", stderr(&out));
        let rapport: Value = serde_json::from_slice(&out.stdout).expect("JSON valide");
        rapport["totals"]["entries"].as_u64().unwrap()
    };

    let tout = entrees(&["--json", chemin]);
    let fin = entrees(&["--json", "-n", "500", chemin]);
    assert!(tout > 4000, "le fichier entier est bien plus gros : {tout}");
    // Une entrée par ligne, sauf les stack traces recollées à la précédente.
    assert!(fin <= 500, "seules les 500 dernières lignes : {fin}");
    assert!(fin > 400, "mais bien 500, pas une poignée : {fin}");
    // Plus grand que le fichier : on retombe sur son intégralité.
    assert_eq!(entrees(&["--json", "-n", "999999", chemin]), tout);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn une_source_illisible_fait_echouer_la_commande() {
    // Le piège du monitoring : sans ça, un cron sur un chemin fautif recevrait
    // un instantané à zéro et un code de sortie 0, donc « tout va bien ».
    let out = refrain(&["--json", "/introuvable/prod.log"]);
    assert!(
        !out.status.success(),
        "un fichier inexistant doit produire un code de sortie non nul"
    );
    assert!(
        stderr(&out).contains("introuvable"),
        "le message doit nommer le fichier fautif : {}",
        stderr(&out)
    );
}

#[test]
fn les_options_incompatibles_sont_refusees() {
    assert!(!refrain(&["--json", "--summary", "x.log"]).status.success());
    assert!(!refrain(&["--every", "5", "x.log"]).status.success());
    // « depuis le début » et « les N dernières lignes » se contredisent.
    assert!(!refrain(&["-a", "-n", "10", "x.log"]).status.success());
}

#[test]
fn la_fenetre_temporelle_restreint_le_rapport() {
    let dir = dossier("fenetre");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("prod.log");

    // Un fichier aux dates connues plutôt que du `genlogs` : on veut pouvoir
    // dire exactement ce qui doit tomber de part et d'autre des bornes.
    let mut contenu = String::new();
    for (heure, route) in [
        ("09:59:59", "avant_la_fenetre"),
        ("10:00:00", "dans_la_fenetre"),
        ("10:30:00", "dans_la_fenetre_aussi"),
        ("11:00:00", "apres_la_fenetre"),
    ] {
        contenu.push_str(&format!(
            "[2026-09-09T{heure}.000000+02:00] request.INFO: Matched route \"{route}\". \
             {{\"route\":\"{route}\"}} []\n"
        ));
    }
    std::fs::write(&log, contenu).unwrap();
    let chemin = log.to_str().unwrap();

    // Sans fenêtre : les quatre lignes.
    let out = refrain(&["--json", chemin]);
    let rapport: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rapport["totals"]["entries"], 4);
    assert_eq!(rapport["totals"]["out_of_window"], 0);

    // Avec fenêtre : les deux du milieu, et les deux autres comptées à part —
    // surtout pas dans `skipped`, qui signale un problème de format.
    let out = refrain(&[
        "--json",
        "--since",
        "2026-09-09T10:00:00+02:00",
        "--until",
        "2026-09-09T10:30:00+02:00",
        chemin,
    ]);
    assert!(out.status.success(), "refrain a échoué : {}", stderr(&out));
    let rapport: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        rapport["totals"]["entries"], 2,
        "seules les lignes du milieu"
    );
    assert_eq!(rapport["totals"]["out_of_window"], 2);
    assert_eq!(rapport["totals"]["skipped"], 0);

    // Et les endpoints hors fenêtre ont bel et bien disparu des agrégats.
    let endpoints = rapport["endpoints"].as_array().unwrap();
    let noms: Vec<&str> = endpoints
        .iter()
        .map(|e| e["endpoint"].as_str().unwrap())
        .collect();
    assert!(noms.contains(&"dans_la_fenetre"), "{noms:?}");
    assert!(!noms.contains(&"avant_la_fenetre"), "{noms:?}");
    assert!(!noms.contains(&"apres_la_fenetre"), "{noms:?}");

    // Le résumé texte dit ce qui a été écarté.
    let out = refrain(&["--summary", "--since", "2026-09-09T10:00:00+02:00", chemin]);
    let texte = String::from_utf8_lossy(&out.stdout);
    assert!(texte.contains("outside the bounds"), "{texte}");

    // Une borne mal écrite est refusée au lancement, pas après lecture.
    let out = refrain(&["--summary", "--since", "hier matin", chemin]);
    assert!(!out.status.success(), "une borne absurde doit être refusée");
    assert!(
        stderr(&out).contains("neither a duration"),
        "{}",
        stderr(&out)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn les_seuils_decident_du_code_de_sortie() {
    let dir = dossier("seuils");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("prod.log");

    // Quatre requêtes aux durées connues, dont une en erreur : taux d'erreur
    // de 25 %, pire durée à 3 s.
    let mut contenu = String::new();
    for (route, ms) in [("lent", 3000.0), ("rapide", 20.0), ("rapide", 30.0)] {
        contenu.push_str(&format!(
            "[2026-09-09T10:00:00.000000+02:00] request.INFO: Request finished \
             {{\"route\":\"{route}\",\"duration_ms\":{ms}}} []\n"
        ));
    }
    contenu.push_str(
        "[2026-09-09T10:00:01.000000+02:00] request.CRITICAL: Uncaught PHP Exception \
         App\\Exception\\Boom: \"nope\" at /var/www/src/X.php line 12 \
         {\"route\":\"lent\"} []\n",
    );
    std::fs::write(&log, contenu).unwrap();
    let chemin = log.to_str().unwrap();

    // Seuil respecté : 0.
    let out = refrain(&["--summary", "--fail-if", "error-rate>50%", chemin]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    // Seuil franchi : 3, distinct du 1 des sources illisibles comme du 2 que
    // clap rend pour une ligne de commande fautive.
    let out = refrain(&["--summary", "--fail-if", "error-rate>10%", chemin]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("threshold crossed"),
        "{}",
        stderr(&out)
    );
    // Le rapport reste sur la sortie standard : un tube en aval n'est pas pollué.
    assert!(String::from_utf8_lossy(&out.stdout).contains("summary"));

    // Plusieurs seuils, dont un sur le pire endpoint, qui doit être nommé.
    let out = refrain(&[
        "--summary",
        "--fail-if",
        "error-rate>10%",
        "--fail-if",
        "p95>1s",
        chemin,
    ]);
    assert_eq!(out.status.code(), Some(3));
    let erreurs = stderr(&out);
    assert_eq!(
        erreurs.lines().count(),
        2,
        "un seuil franchi par ligne : {erreurs}"
    );
    assert!(erreurs.contains("p95 (lent)"), "{erreurs}");

    // Une source illisible prime : les chiffres ne veulent rien dire.
    let out = refrain(&[
        "--summary",
        "--fail-if",
        "error-rate>10%",
        "/introuvable.log",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));

    // Un seuil mal formé est refusé au lancement, avant toute lecture — et
    // avec 2, ce qui le distingue d'un seuil réellement franchi.
    let out = refrain(&["--summary", "--fail-if", "p95 est trop grand", chemin]);
    assert_eq!(out.status.code(), Some(2), "une ligne de commande fautive");
    assert!(
        stderr(&out).contains("has no comparator"),
        "{}",
        stderr(&out)
    );

    // Et un seuil n'a pas de sens sans rapport qui se termine.
    let out = refrain(&["--fail-if", "error-rate>10%", chemin]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("one-shot report"), "{}", stderr(&out));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn un_journal_tourne_donne_le_meme_resultat_quen_clair() {
    let dir = dossier("gzip");
    std::fs::create_dir_all(&dir).unwrap();
    let clair = dir.join("prod.log");

    let out = genlogs(&[
        "--rate",
        "0",
        "--count",
        "200",
        "--seed",
        "3",
        clair.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "genlogs a échoué : {}", stderr(&out));

    // Compressé par le vrai `gzip`, comme le ferait logrotate — et non par la
    // bibliothèque qui sert à le relire : on veut savoir qu'on sait lire ce que
    // produit le système, pas seulement ce qu'on produit soi-même.
    let copie = dir.join("prod.log.1");
    std::fs::copy(&clair, &copie).unwrap();
    let gzip = Command::new("gzip")
        .arg(&copie)
        .status()
        .expect("gzip doit être installé");
    assert!(gzip.success(), "gzip a échoué");
    let compresse = dir.join("prod.log.1.gz");
    assert!(compresse.exists());
    assert!(
        std::fs::metadata(&compresse).unwrap().len() < std::fs::metadata(&clair).unwrap().len(),
        "le fichier compressé doit être plus petit"
    );

    let lire = |chemin: &std::path::Path| -> Value {
        let out = refrain(&["--json", "--top", "0", chemin.to_str().unwrap()]);
        assert!(out.status.success(), "refrain a échoué : {}", stderr(&out));
        serde_json::from_slice(&out.stdout).expect("du JSON")
    };

    let attendu = lire(&clair);
    let obtenu = lire(&compresse);

    assert!(
        attendu["totals"]["entries"].as_u64().unwrap() > 1000,
        "le fichier d'essai doit être conséquent"
    );
    assert_eq!(obtenu["totals"], attendu["totals"], "mêmes totaux");
    assert_eq!(obtenu["levels"], attendu["levels"], "mêmes niveaux");
    assert_eq!(obtenu["endpoints"], attendu["endpoints"], "mêmes endpoints");
    assert_eq!(obtenu["nplus1"], attendu["nplus1"], "mêmes motifs N+1");

    // Les deux ensemble : c'est le geste du post-mortem, la veille et le jour
    // même donnés d'un coup.
    let out = refrain(&[
        "--json",
        clair.to_str().unwrap(),
        compresse.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let ensemble: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        ensemble["totals"]["entries"].as_u64().unwrap(),
        attendu["totals"]["entries"].as_u64().unwrap() * 2,
        "les deux sources doivent être comptées"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn genlogs_etale_les_requetes_dans_le_temps() {
    let dir = dossier("spread");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("prod.log");
    let chemin = log.to_str().unwrap();

    let span = |args: &[&str]| -> f64 {
        let out = genlogs(args);
        assert!(out.status.success(), "genlogs a échoué : {}", stderr(&out));
        let out = refrain(&["--json", chemin]);
        let rapport: Value = serde_json::from_slice(&out.stdout).unwrap();
        let span = rapport["window"]["span_seconds"].as_f64().unwrap();
        std::fs::remove_file(&log).unwrap();
        span
    };

    // Sans étalement, tout est écrit en quelques millisecondes : les graphes de
    // refrain se réduiraient à une barre unique.
    let serre = span(&["--rate", "0", "--count", "200", "--seed", "5", chemin]);
    assert!(
        serre < 5.0,
        "sans --spread, la fenêtre doit être étroite : {serre}"
    );

    // Avec, les requêtes couvrent la fenêtre demandée.
    let etale = span(&[
        "--rate", "0", "--count", "200", "--spread", "60", "--seed", "5", chemin,
    ]);
    assert!(
        (55.0..=62.0).contains(&etale),
        "--spread 60 doit couvrir une minute, pas {etale} s"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
