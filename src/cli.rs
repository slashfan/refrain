//! Définition des options en ligne de commande.
//!
//! `clap` avec la feature `derive` construit tout l'analyseur d'arguments à
//! partir de cette structure : les commentaires `///` deviennent l'aide affichée
//! par `refrain --help`.

use crate::parser::Level;
use crate::threshold::Threshold;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "refrain",
    version,
    about = "Analyseur de logs Symfony/Monolog en temps réel",
    long_about = "Suit des fichiers de log Monolog, les analyse à la volée et affiche \
                  un tableau de bord terminal : erreurs par type, endpoints les plus \
                  lents, pics de trafic."
)]
pub struct Cli {
    /// Fichiers de log à suivre. « - » lit l'entrée standard.
    #[arg(required = true, value_name = "FICHIER")]
    pub files: Vec<PathBuf>,

    /// Analyser tout le fichier depuis le début (par défaut : suivre depuis la fin).
    #[arg(short = 'a', long)]
    pub from_start: bool,

    /// Relire les N dernières lignes au démarrage, comme `tail -n`. Avec
    /// `--summary` ou `--json`, restreint le rapport à cette fin de fichier.
    #[arg(
        short = 'n',
        long,
        default_value_t = 0,
        value_name = "N",
        conflicts_with = "from_start"
    )]
    pub lines: usize,

    /// Ne compter que les entrées à partir de cet instant : une durée comptée
    /// depuis le lancement (`30s`, `15m`, `2h`, `3d`), ou une date
    /// (`2026-09-09T14:30:00`, `2026-09-09 14:30`, `14:30` pour aujourd'hui).
    ///
    /// Implique de lire le fichier depuis le début, sauf si `-n` en limite
    /// explicitement la relecture — sur un fichier de quarante gigaoctets,
    /// `--since 15m -n 100000` évite de tout relire pour n'en garder qu'un
    /// quart d'heure.
    #[arg(long, value_name = "QUAND", value_parser = parse_bound)]
    pub since: Option<Bound>,

    /// Ne compter que les entrées jusqu'à cet instant. Mêmes formes que
    /// `--since`.
    #[arg(long, value_name = "QUAND", value_parser = parse_bound)]
    pub until: Option<Bound>,

    /// Faire échouer la commande si un seuil est franchi, avec le code de
    /// sortie 3 : `error-rate>2%`, `p95>1s`, `p95:api_orders_list>800ms`,
    /// `entries<100`. Répétable.
    ///
    /// Métriques : `error-rate`, `errors`, `entries`, `p50`, `p95`, `p99`,
    /// `max`. Les quantiles portent sur le pire endpoint, ou sur celui qu'on
    /// nomme après « : ». Unités : `%`, `ms`, `s`.
    ///
    /// Ne vaut que pour un rapport ponctuel : `--summary` ou `--json` sans
    /// `--every`.
    #[arg(
        long = "fail-if",
        value_name = "SEUIL",
        value_parser = crate::threshold::Threshold::parse,
        conflicts_with = "every"
    )]
    pub fail_if: Vec<Threshold>,

    /// Clé de `context`/`extra` contenant la durée. Auto-détectée si absente.
    #[arg(long, value_name = "CLÉ")]
    pub duration_key: Option<String>,

    /// Unité de la valeur de durée trouvée.
    #[arg(long, value_enum, default_value_t = DurationUnit::Auto)]
    pub duration_unit: DurationUnit,

    /// Clé identifiant une requête (token, uid, request_id…). Permet de déduire
    /// la durée d'un endpoint quand aucun champ de durée n'est loggué.
    #[arg(long, value_name = "CLÉ")]
    pub correlate_key: Option<String>,

    /// Désactiver la corrélation même si une clé est détectée.
    #[arg(long)]
    pub no_correlate: bool,

    /// Délai d'inactivité (secondes) après lequel une requête corrélée est close.
    #[arg(long, default_value_t = 5.0, value_name = "SEC")]
    pub correlate_timeout: f64,

    /// Niveau minimum affiché au départ dans l'onglet Flux (ajustable avec +/-).
    /// Les statistiques, elles, comptent toujours tout.
    #[arg(short = 'l', long, value_enum, default_value_t = Level::Debug)]
    pub min_level: Level,

    /// Pas d'interface : analyse jusqu'à la fin du fichier puis affiche un
    /// résumé texte. Pratique en cron, en CI, ou au bout d'un `ssh`.
    #[arg(long)]
    pub summary: bool,

    /// Sortie JSON des statistiques au lieu du tableau de bord, pour du
    /// monitoring. Sans `--every`, lit les fichiers jusqu'au bout puis rend un
    /// objet unique.
    #[arg(long, conflicts_with = "summary")]
    pub json: bool,

    /// Avec `--json` : rester en suivi et émettre un objet JSON toutes les SEC
    /// secondes, un par ligne (NDJSON).
    #[arg(long, value_name = "SEC", requires = "json")]
    pub every: Option<f64>,

    /// Seuil de détection N+1 : nombre de fois qu'une même requête SQL doit
    /// être exécutée dans une seule requête HTTP pour être signalée. 0 désactive.
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub nplus1: u32,

    /// Nombre d'erreurs et d'endpoints détaillés en JSON. 0 = tous.
    #[arg(long, default_value_t = 25, value_name = "N")]
    pub top: usize,

    /// Intervalle de rafraîchissement de l'affichage, en millisecondes.
    #[arg(long, default_value_t = 250, value_name = "MS")]
    pub tick_ms: u64,

    /// Nombre d'entrées conservées dans l'onglet Flux.
    #[arg(long, default_value_t = 2000, value_name = "N")]
    pub scrollback: usize,
}

/// Ce que le programme doit produire. Déduit des drapeaux plutôt que stocké :
/// une seule source de vérité, impossible de la désynchroniser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Le tableau de bord interactif (défaut).
    Tui,
    /// Un résumé texte, une fois, en fin de lecture.
    Summary,
    /// Un objet JSON, une fois, en fin de lecture.
    JsonOnce,
    /// Un objet JSON par intervalle, en suivi continu (NDJSON).
    JsonStream,
}

impl Cli {
    pub fn mode(&self) -> Mode {
        match (self.json, self.every, self.summary) {
            (true, Some(_), _) => Mode::JsonStream,
            (true, None, _) => Mode::JsonOnce,
            (false, _, true) => Mode::Summary,
            _ => Mode::Tui,
        }
    }

    /// Les modes « une fois » lisent jusqu'au bout puis rendent la main ; les
    /// autres restent accrochés au fichier.
    pub fn follow(&self) -> bool {
        matches!(self.mode(), Mode::Tui | Mode::JsonStream)
    }

    /// Un rapport ponctuel porte sur l'intégralité du fichier… sauf si `-n` en
    /// désigne explicitement la fin : sur un `prod.log` de quarante gigaoctets,
    /// « résume-moi les cent mille dernières lignes » est une demande courante,
    /// et l'ignorer en silence relirait tout le fichier.
    pub fn read_from_start(&self) -> bool {
        self.from_start
            || (self.lines == 0 && matches!(self.mode(), Mode::Summary | Mode::JsonOnce))
            // Une fenêtre qui commence dans le passé n'a de sens qu'à partir du
            // début du fichier : suivre depuis la fin ne montrerait rien tant
            // qu'une nouvelle ligne n'arrive pas.
            || (self.since.is_some() && self.lines == 0)
    }

    /// Période entre deux instantanés NDJSON, bornée pour éviter de noyer la
    /// sortie ou de lire l'horloge en boucle.
    pub fn snapshot_period(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.every.unwrap_or(10.0).clamp(0.1, 3600.0))
    }
}

/// Comment interpréter la valeur numérique trouvée dans le champ de durée.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DurationUnit {
    /// Déduit l'unité du nom de la clé (`_ms`, `_s`, `_us`), puis de l'ordre de
    /// grandeur : un flottant sous 30 est presque toujours des secondes.
    Auto,
    /// Millisecondes.
    Ms,
    /// Secondes.
    S,
    /// Microsecondes.
    Us,
}

/// Une borne temporelle, telle qu'écrite sur la ligne de commande.
///
/// Elle n'est pas résolue ici mais au démarrage de l'agrégation : `--since 15m`
/// désigne un instant fixe, pris une fois pour toutes, et non une fenêtre qui
/// glisserait sous les pieds des compteurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Un recul, en millisecondes, depuis le lancement.
    Ago(i64),
    /// Un instant, en millisecondes depuis l'époque.
    At(i64),
}

impl Bound {
    pub fn epoch_ms(self, launched_ms: i64) -> i64 {
        match self {
            Bound::Ago(ms) => launched_ms - ms,
            Bound::At(ms) => ms,
        }
    }
}

/// Accepte une durée relative ou une date, sous les formes qu'on écrit sans y
/// penser quand on cherche « depuis quatorze heures trente ».
fn parse_bound(texte: &str) -> Result<Bound, String> {
    let texte = texte.trim();
    if let Some(ms) = parse_duree(texte) {
        return Ok(Bound::Ago(ms));
    }
    if let Some(ms) = parse_instant(texte) {
        return Ok(Bound::At(ms));
    }
    Err(format!(
        "« {texte} » n'est ni une durée (30s, 15m, 2h, 3d) ni une date \
         (2026-09-09T14:30:00, « 2026-09-09 14:30 », 14:30)"
    ))
}

/// `15m` → 900 000 ms. Sans unité, on refuse : « --since 15 » ne veut rien dire.
fn parse_duree(texte: &str) -> Option<i64> {
    let (nombre, unite) = texte.split_at(texte.len().checked_sub(1)?);
    let quantite: i64 = nombre.parse().ok()?;
    let facteur = match unite {
        "s" => 1_000,
        "m" => 60 * 1_000,
        "h" => 60 * 60 * 1_000,
        "d" => 24 * 60 * 60 * 1_000,
        _ => return None,
    };
    quantite.checked_mul(facteur)
}

fn parse_instant(texte: &str) -> Option<i64> {
    // Avec fuseau : la date porte elle-même son décalage, rien à deviner.
    if let Ok(ts) = DateTime::parse_from_rfc3339(texte) {
        return Some(ts.timestamp_millis());
    }

    // Sans fuseau : on prend celui de la machine, qui est aussi celui des logs
    // dans l'immense majorité des cas.
    const DATES: [&str; 5] = [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d",
    ];
    for format in DATES {
        let naive = if format == "%Y-%m-%d" {
            NaiveDate::parse_from_str(texte, format)
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        } else {
            NaiveDateTime::parse_from_str(texte, format).ok()
        };
        if let Some(naive) = naive {
            return local_ms(naive);
        }
    }

    // Une heure seule désigne aujourd'hui : « --since 14:30 » est ce qu'on tape
    // le jour même, en plein incident.
    for format in ["%H:%M:%S", "%H:%M"] {
        if let Ok(heure) = NaiveTime::parse_from_str(texte, format) {
            return local_ms(Local::now().date_naive().and_time(heure));
        }
    }
    None
}

/// Interprète une date sans fuseau dans celui de la machine.
///
/// `earliest()` tranche les deux cas tordus du changement d'heure : une heure
/// qui n'existe pas au printemps, une heure qui existe deux fois à l'automne.
fn local_ms(naive: NaiveDateTime) -> Option<i64> {
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_durees_relatives_se_comptent_depuis_le_lancement() {
        assert_eq!(parse_bound("30s"), Ok(Bound::Ago(30_000)));
        assert_eq!(parse_bound("15m"), Ok(Bound::Ago(900_000)));
        assert_eq!(parse_bound("2h"), Ok(Bound::Ago(7_200_000)));
        assert_eq!(parse_bound("3d"), Ok(Bound::Ago(259_200_000)));

        // Un lancement à midi pile, « depuis 15 minutes » : 11 h 45.
        let midi = 1_757_412_000_000;
        assert_eq!(Bound::Ago(900_000).epoch_ms(midi), midi - 900_000);
        assert_eq!(Bound::At(42).epoch_ms(midi), 42);
    }

    #[test]
    fn une_duree_sans_unite_est_refusee() {
        // « --since 15 » ne veut rien dire : minutes ? secondes ? On refuse
        // plutôt que de deviner.
        assert!(parse_bound("15").is_err());
        assert!(parse_bound("15x").is_err());
        assert!(parse_bound("").is_err());
        assert!(parse_bound("hier matin").is_err());
    }

    #[test]
    fn les_dates_absolues_sont_comprises_sous_leurs_formes_usuelles() {
        let reference = DateTime::parse_from_rfc3339("2026-09-09T14:30:00+02:00")
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            parse_bound("2026-09-09T14:30:00+02:00"),
            Ok(Bound::At(reference))
        );

        // Sans fuseau : celui de la machine. On ne compare donc pas à une
        // valeur en dur, mais à la même date passée par le même chemin.
        for texte in [
            "2026-09-09T14:30:00",
            "2026-09-09 14:30:00",
            "2026-09-09T14:30",
            "2026-09-09 14:30",
        ] {
            let attendu = local_ms(
                NaiveDate::from_ymd_opt(2026, 9, 9)
                    .unwrap()
                    .and_hms_opt(14, 30, 0)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(parse_bound(texte), Ok(Bound::At(attendu)), "{texte}");
        }

        // Une date seule commence à minuit.
        let minuit = local_ms(
            NaiveDate::from_ymd_opt(2026, 9, 9)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(parse_bound("2026-09-09"), Ok(Bound::At(minuit)));
    }

    #[test]
    fn une_heure_seule_designe_aujourd_hui() {
        let attendu = local_ms(Local::now().date_naive().and_hms_opt(14, 30, 0).unwrap()).unwrap();
        assert_eq!(parse_bound("14:30"), Ok(Bound::At(attendu)));
        assert_eq!(parse_bound("14:30:00"), Ok(Bound::At(attendu)));
    }

    #[test]
    fn since_impose_de_lire_depuis_le_debut_sauf_si_n_plafonne() {
        // En suivi, sans fenêtre : on part de la fin, comme `tail -f`.
        let cli = Cli::parse_from(["refrain", "prod.log"]);
        assert!(!cli.read_from_start());

        // Avec `--since`, partir de la fin ne montrerait rien.
        let cli = Cli::parse_from(["refrain", "--since", "15m", "prod.log"]);
        assert!(cli.read_from_start());

        // Sauf si `-n` borne explicitement la relecture : c'est le garde-fou
        // de coût sur un fichier de quarante gigaoctets.
        let cli = Cli::parse_from(["refrain", "--since", "15m", "-n", "1000", "prod.log"]);
        assert!(!cli.read_from_start());
    }
}
