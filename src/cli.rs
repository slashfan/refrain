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
    about = "Real-time Symfony/Monolog log analyser for your terminal",
    long_about = "Follows Monolog log files, parses them on the fly and shows a \
                  terminal dashboard: errors grouped by type, slowest endpoints, \
                  traffic peaks."
)]
pub struct Cli {
    /// Log files to follow. "-" reads standard input.
    #[arg(required = true, value_name = "FILE")]
    pub files: Vec<PathBuf>,

    /// Read the whole file from the start (default: follow from the end).
    #[arg(short = 'a', long)]
    pub from_start: bool,

    /// Re-read the last N lines on start-up, like `tail -n`. With `--summary`
    /// or `--json`, limits the report to that tail of the file.
    #[arg(
        short = 'n',
        long,
        default_value_t = 0,
        value_name = "N",
        conflicts_with = "from_start"
    )]
    pub lines: usize,

    /// Only count entries from this point on: a duration back from start-up
    /// (`30s`, `15m`, `2h`, `3d`), or a date (`2026-09-09T14:30:00`,
    /// `2026-09-09 14:30`, or `14:30` for today).
    ///
    /// Implies reading the file from the start, unless `-n` explicitly caps how
    /// much is re-read — on a forty-gigabyte file, `--since 15m -n 100000`
    /// avoids reading it all to keep a quarter of an hour.
    #[arg(long, value_name = "WHEN", value_parser = parse_bound)]
    pub since: Option<Bound>,

    /// Only count entries up to this point. Same forms as `--since`.
    #[arg(long, value_name = "WHEN", value_parser = parse_bound)]
    pub until: Option<Bound>,

    /// Fail with exit code 3 if a threshold is crossed: `error-rate>2%`,
    /// `p95>1s`, `p95:api_orders_list>800ms`, `entries<100`. Repeatable.
    ///
    /// Metrics: `error-rate`, `errors`, `entries`, `p50`, `p95`, `p99`, `max`.
    /// Quantiles apply to the worst endpoint, or to the one named after `:`.
    /// Units: `%`, `ms`, `s`.
    ///
    /// Only for one-shot reports: `--summary`, or `--json` without `--every`.
    #[arg(
        long = "fail-if",
        value_name = "THRESHOLD",
        value_parser = crate::threshold::Threshold::parse,
        conflicts_with = "every"
    )]
    pub fail_if: Vec<Threshold>,

    /// Key in `context`/`extra` holding the duration. Auto-detected if absent.
    #[arg(long, value_name = "KEY")]
    pub duration_key: Option<String>,

    /// Unit of the duration value found.
    #[arg(long, value_enum, default_value_t = DurationUnit::Auto)]
    pub duration_unit: DurationUnit,

    /// Key identifying one request (token, uid, request_id…). Lets refrain
    /// derive endpoint durations when no duration field is logged.
    #[arg(long, value_name = "KEY")]
    pub correlate_key: Option<String>,

    /// Disable correlation even when a key is detected.
    #[arg(long)]
    pub no_correlate: bool,

    /// Idle time (seconds) after which a correlated request is closed.
    #[arg(long, default_value_t = 5.0, value_name = "SEC")]
    pub correlate_timeout: f64,

    /// Minimum level shown in the Stream tab at start (adjust with +/-).
    /// Statistics always count everything, whatever this is set to.
    #[arg(short = 'l', long, value_enum, default_value_t = Level::Debug)]
    pub min_level: Level,

    /// No dashboard: read to the end of the file, then print a text summary.
    /// Handy from cron, from CI, or at the end of an `ssh`.
    #[arg(long)]
    pub summary: bool,

    /// JSON statistics instead of the dashboard, for monitoring. Without
    /// `--every`, reads the files to the end and prints a single object.
    #[arg(long, conflicts_with = "summary")]
    pub json: bool,

    /// With `--json`: keep following and emit one JSON object every SEC
    /// seconds, one per line (NDJSON).
    #[arg(long, value_name = "SEC", requires = "json")]
    pub every: Option<f64>,

    /// N+1 detection threshold: how many times the same SQL query must run
    /// within a single HTTP request to be reported. 0 disables it.
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub nplus1: u32,

    /// How many errors and endpoints to detail in JSON. 0 means all of them.
    #[arg(long, default_value_t = 25, value_name = "N")]
    pub top: usize,

    /// Refresh interval of the display, in milliseconds.
    #[arg(long, default_value_t = 250, value_name = "MS")]
    pub tick_ms: u64,

    /// How many entries the Stream tab keeps.
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
    /// Infer from the key name (`_ms`, `_s`, `_us`), then from magnitude: a
    /// float below 30 is almost always seconds.
    Auto,
    /// Milliseconds.
    Ms,
    /// Seconds.
    S,
    /// Microseconds.
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
        "'{texte}' is neither a duration (30s, 15m, 2h, 3d) nor a date \
         (2026-09-09T14:30:00, '2026-09-09 14:30', 14:30)"
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
