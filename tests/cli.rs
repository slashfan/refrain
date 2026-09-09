//! Tests de bout en bout : on lance les vrais binaires, tels qu'un utilisateur
//! les lance.
//!
//! Les tests unitaires vérifient chaque brique isolément ; ceux-ci vérifient
//! l'assemblage — que `genlogs` écrit un fichier que `ruru` sait relire, que la
//! sortie JSON est bien du JSON, et que les codes de sortie sont ceux annoncés.
//!
//! `env!("CARGO_BIN_EXE_<nom>")` est fourni par Cargo : c'est le chemin du
//! binaire qu'il vient de compiler pour ce test.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};

/// Un dossier de travail propre, distinct par test.
fn dossier(nom: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ruru-e2e-{}-{nom}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn genlogs(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_genlogs"))
        .args(args)
        .output()
        .expect("genlogs doit pouvoir démarrer")
}

fn ruru(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ruru"))
        .args(args)
        .output()
        .expect("ruru doit pouvoir démarrer")
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

    let out = ruru(&["--json", chemin]);
    assert!(out.status.success(), "ruru a échoué : {}", stderr(&out));

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

    let out = ruru(&["--summary", chemin]);
    assert!(out.status.success(), "ruru a échoué : {}", stderr(&out));

    let resume = String::from_utf8_lossy(&out.stdout);
    assert!(resume.contains("Endpoints les plus lents"));
    assert!(resume.contains("Motifs N+1"));
    assert!(resume.contains("api_orders_list"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn l_entree_standard_est_analysable() {
    // `ssh prod tail -f … | ruru -` : le tube doit marcher comme un fichier.
    let out = genlogs(&["--rate", "0", "--count", "50", "--seed", "5"]);
    assert!(out.status.success(), "genlogs a échoué : {}", stderr(&out));

    let dir = dossier("stdin");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("depuis-stdin.log");
    std::fs::write(&log, &out.stdout).unwrap();

    let out = ruru(&["--json", log.to_str().unwrap()]);
    let rapport: Value = serde_json::from_slice(&out.stdout).expect("JSON valide");
    assert!(rapport["totals"]["entries"].as_u64().unwrap() > 200);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn une_source_illisible_fait_echouer_la_commande() {
    // Le piège du monitoring : sans ça, un cron sur un chemin fautif recevrait
    // un instantané à zéro et un code de sortie 0, donc « tout va bien ».
    let out = ruru(&["--json", "/introuvable/prod.log"]);
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
    assert!(!ruru(&["--json", "--summary", "x.log"]).status.success());
    assert!(!ruru(&["--every", "5", "x.log"]).status.success());
}
