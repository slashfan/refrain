//! Définition des options en ligne de commande.
//!
//! `clap` avec la feature `derive` construit tout l'analyseur d'arguments à
//! partir de cette structure : les commentaires `///` deviennent l'aide affichée
//! par `refrain --help`.

use crate::parser::Level;
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
