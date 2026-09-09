//! L'état de l'application et sa réaction aux événements.
//!
//! Séparation volontaire des rôles : ce module **décide**, `ui.rs` se contente
//! de **dessiner**. Les tableaux triés sont recalculés une fois par battement
//! d'horloge (et non à chaque image), ce qui garde le rendu quasi gratuit même
//! quand les logs défilent à 100 000 lignes par seconde.

use crate::cli::Cli;
use crate::event::Event;
use crate::parser::Level;
use crate::stats::{ErrorStat, NPlusOne, Stats};
use chrono::{DateTime, FixedOffset};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::cmp::Reverse;
use std::time::Instant;

/// Nombre de lignes conservées dans les tableaux triés. Au-delà, personne ne
/// scrolle : autant ne pas payer le tri.
const MAX_ROWS: usize = 300;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Errors,
    Endpoints,
    Sql,
    Stream,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Overview,
        Tab::Errors,
        Tab::Endpoints,
        Tab::Sql,
        Tab::Stream,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Vue d'ensemble",
            Tab::Errors => "Erreurs",
            Tab::Endpoints => "Endpoints",
            Tab::Sql => "SQL",
            Tab::Stream => "Flux",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RouteSort {
    P95,
    Requests,
    Errors,
    Max,
}

impl RouteSort {
    pub fn label(self) -> &'static str {
        match self {
            RouteSort::P95 => "p95",
            RouteSort::Requests => "requêtes",
            RouteSort::Errors => "erreurs",
            RouteSort::Max => "max",
        }
    }

    fn next(self) -> Self {
        match self {
            RouteSort::P95 => RouteSort::Max,
            RouteSort::Max => RouteSort::Requests,
            RouteSort::Requests => RouteSort::Errors,
            RouteSort::Errors => RouteSort::P95,
        }
    }
}

/// Une ligne du tableau des erreurs. On n'y recopie que ce qu'affiche le
/// tableau ; le détail (message complet, contexte) est relu depuis `Stats` au
/// moment du rendu, pour ne pas dupliquer de gros blocs de texte à chaque tick.
pub struct ErrorRow {
    pub signature: String,
    pub count: u64,
    pub level: Level,
    pub channel: String,
    pub last_seen: Option<DateTime<FixedOffset>>,
}

/// Un motif N+1 tel qu'affiché dans le tableau.
pub struct NPlusOneRow {
    pub key: (String, u64),
    pub endpoint: String,
    pub max_count: u32,
    pub avg_count: f32,
    pub requests: u64,
    pub sql: String,
}

pub struct RouteRow {
    pub name: String,
    pub requests: u64,
    pub avg_queries: f32,
    pub errors: u64,
    pub timed: u64,
    pub p50: f32,
    pub p95: f32,
    pub max: f32,
    pub error_rate: f32,
}

pub struct App {
    pub cli: Cli,
    pub stats: Stats,
    pub tab: Tab,
    pub error_rows: Vec<ErrorRow>,
    pub route_rows: Vec<RouteRow>,
    pub nplus1_rows: Vec<NPlusOneRow>,
    pub error_sel: usize,
    pub route_sel: usize,
    pub nplus1_sel: usize,
    /// Décalage du flux par rapport au bas. 0 = collé aux dernières lignes.
    pub stream_offset: usize,
    pub frozen: bool,
    pub min_level: Level,
    pub route_sort: RouteSort,
    pub show_help: bool,
    pub should_quit: bool,
    pub sources_total: usize,
    pub sources_done: usize,
    pub failures: Vec<String>,
    pub started: Instant,
    /// Des entrées sont arrivées depuis le dernier recalcul des tableaux.
    dirty: bool,
}

impl App {
    pub fn new(cli: Cli, sources_total: usize) -> Self {
        let stats = Stats::new(&cli);
        let min_level = cli.min_level;
        Self {
            cli,
            stats,
            tab: Tab::Overview,
            error_rows: Vec::new(),
            route_rows: Vec::new(),
            nplus1_rows: Vec::new(),
            error_sel: 0,
            route_sel: 0,
            nplus1_sel: 0,
            stream_offset: 0,
            frozen: false,
            min_level,
            route_sort: RouteSort::P95,
            show_help: false,
            should_quit: false,
            sources_total,
            sources_done: 0,
            failures: Vec::new(),
            started: Instant::now(),
            dirty: true,
        }
    }

    pub fn all_sources_done(&self) -> bool {
        self.sources_done >= self.sources_total
    }

    /// Traite un événement. Renvoie `true` s'il faut redessiner.
    pub fn on_event(&mut self, event: Event) -> bool {
        match event {
            Event::Batch { source, entries } => {
                for entry in entries {
                    self.stats.ingest(source, entry);
                }
                self.dirty = true;
                // Pas de redessin : on attend le Tick. Sinon un flux rapide
                // passerait son temps à repeindre l'écran au lieu de compter.
                false
            }
            Event::Skipped(n) => {
                self.stats.skipped += n;
                false
            }
            Event::CaughtUp(source) => {
                self.stats.source_caught_up(source);
                false
            }
            Event::SourceDone(source) => {
                self.sources_done += 1;
                self.stats.source_done(source);
                // Surtout pas de `finalize` tant qu'une autre source lit encore :
                // il clôt toutes les requêtes ouvertes, y compris celles dont les
                // lignes dorment dans un fichier qu'on n'a pas fini de parcourir.
                if self.all_sources_done() {
                    self.stats.finalize();
                }
                self.dirty = true;
                true
            }
            Event::Failed(message) => {
                self.failures.push(message);
                true
            }
            Event::Key(key) => {
                self.on_key(key);
                true
            }
            Event::Resize => true,
            Event::Tick => {
                // Rien n'est arrivé depuis le dernier battement : les requêtes
                // encore ouvertes le resteraient indéfiniment, faute d'une
                // nouvelle ligne pour faire avancer l'horloge des logs.
                if !self.dirty && self.stats.sweep_idle() > 0 {
                    self.dirty = true;
                }
                if self.dirty {
                    self.refresh_views();
                    self.dirty = false;
                }
                true
            }
        }
    }

    /// Reconstruit les tableaux triés à partir des compteurs bruts.
    fn refresh_views(&mut self) {
        // -- erreurs, triées par fréquence --------------------------------
        let mut errors: Vec<(&String, &ErrorStat)> = self.stats.errors.iter().collect();
        errors.sort_unstable_by(|a, b| {
            b.1.count
                .cmp(&a.1.count)
                .then_with(|| b.1.level.cmp(&a.1.level))
        });
        self.error_rows = errors
            .into_iter()
            .take(MAX_ROWS)
            .map(|(signature, stat)| ErrorRow {
                signature: signature.clone(),
                count: stat.count,
                level: stat.level,
                channel: stat.channel.clone(),
                last_seen: stat.last_seen,
            })
            .collect();

        // -- endpoints -----------------------------------------------------
        // `scratch` est réutilisé par toutes les routes : une seule allocation
        // pour l'ensemble du calcul des quantiles.
        let mut scratch = Vec::with_capacity(1024);
        let mut rows: Vec<RouteRow> = self
            .stats
            .routes
            .iter()
            .map(|(name, route)| {
                let quantiles = route.quantiles(&mut scratch);
                RouteRow {
                    name: name.clone(),
                    requests: route.requests.max(route.timed),
                    avg_queries: route.avg_queries(),
                    errors: route.errors,
                    timed: route.timed,
                    p50: quantiles.p50,
                    p95: quantiles.p95,
                    max: route.max_ms,
                    error_rate: route.error_rate(),
                }
            })
            .collect();

        match self.route_sort {
            RouteSort::P95 => rows.sort_unstable_by(|a, b| b.p95.total_cmp(&a.p95)),
            RouteSort::Max => rows.sort_unstable_by(|a, b| b.max.total_cmp(&a.max)),
            RouteSort::Requests => rows.sort_unstable_by_key(|row| Reverse(row.requests)),
            RouteSort::Errors => rows.sort_unstable_by_key(|row| Reverse(row.errors)),
        }
        rows.truncate(MAX_ROWS);
        self.route_rows = rows;

        // -- motifs N+1, du plus grave au moins grave ----------------------
        let mut nplus1: Vec<(&(String, u64), &NPlusOne)> = self.stats.nplus1.iter().collect();
        nplus1.sort_unstable_by(|a, b| {
            b.1.max_count
                .cmp(&a.1.max_count)
                .then_with(|| b.1.requests.cmp(&a.1.requests))
        });
        self.nplus1_rows = nplus1
            .into_iter()
            .take(MAX_ROWS)
            .map(|(key, motif)| NPlusOneRow {
                key: key.clone(),
                endpoint: motif.endpoint.clone(),
                max_count: motif.max_count,
                avg_count: motif.avg_count(),
                requests: motif.requests,
                sql: motif.sql.clone(),
            })
            .collect();

        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        self.error_sel = self.error_sel.min(self.error_rows.len().saturating_sub(1));
        self.route_sel = self.route_sel.min(self.route_rows.len().saturating_sub(1));
        self.nplus1_sel = self
            .nplus1_sel
            .min(self.nplus1_rows.len().saturating_sub(1));
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Ctrl-C doit toujours sortir, quel que soit l'écran affiché.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.should_quit = true;
            return;
        }
        if self.show_help {
            // N'importe quelle touche referme l'aide.
            self.show_help = false;
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                return;
            }
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('?') | KeyCode::Char('h') => self.show_help = true,

            KeyCode::Tab | KeyCode::Right => self.cycle_tab(1),
            KeyCode::BackTab | KeyCode::Left => self.cycle_tab(-1),
            KeyCode::Char(c @ '1'..='5') => {
                self.tab = Tab::ALL[c as usize - '1' as usize];
            }

            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home | KeyCode::Char('g') => self.jump_start(),
            KeyCode::End | KeyCode::Char('G') => self.jump_end(),

            KeyCode::Char(' ') => self.frozen = !self.frozen,
            KeyCode::Char('s') => {
                self.route_sort = self.route_sort.next();
                self.dirty = true;
            }
            KeyCode::Char('r') => {
                let cli = self.cli.clone();
                self.stats.reset(&cli);
                self.error_rows.clear();
                self.route_rows.clear();
                self.nplus1_rows.clear();
                self.started = Instant::now();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.shift_min_level(1),
            KeyCode::Char('-') | KeyCode::Char('_') => self.shift_min_level(-1),
            _ => {}
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        let count = Tab::ALL.len() as isize;
        let index = (self.tab.index() as isize + delta).rem_euclid(count);
        self.tab = Tab::ALL[index as usize];
    }

    fn move_selection(&mut self, delta: isize) {
        match self.tab {
            Tab::Errors => {
                self.error_sel = step(self.error_sel, delta, self.error_rows.len());
            }
            Tab::Endpoints => {
                self.route_sel = step(self.route_sel, delta, self.route_rows.len());
            }
            Tab::Sql => {
                self.nplus1_sel = step(self.nplus1_sel, delta, self.nplus1_rows.len());
            }
            Tab::Stream => {
                // Dans le flux, « descendre » rapproche du présent : l'offset
                // se compte depuis le bas, il diminue donc.
                let max = self.stats.recent.len();
                let next = self.stream_offset as isize - delta;
                self.stream_offset = next.clamp(0, max as isize) as usize;
                // Remonter dans l'historique fige naturellement l'affichage.
                self.frozen = self.stream_offset > 0;
            }
            Tab::Overview => {}
        }
    }

    fn jump_start(&mut self) {
        match self.tab {
            Tab::Errors => self.error_sel = 0,
            Tab::Endpoints => self.route_sel = 0,
            Tab::Sql => self.nplus1_sel = 0,
            Tab::Stream => {
                self.stream_offset = self.stats.recent.len();
                self.frozen = true;
            }
            Tab::Overview => {}
        }
    }

    fn jump_end(&mut self) {
        match self.tab {
            Tab::Errors => self.error_sel = self.error_rows.len().saturating_sub(1),
            Tab::Endpoints => self.route_sel = self.route_rows.len().saturating_sub(1),
            Tab::Sql => self.nplus1_sel = self.nplus1_rows.len().saturating_sub(1),
            Tab::Stream => {
                self.stream_offset = 0;
                self.frozen = false;
            }
            Tab::Overview => {}
        }
    }

    fn shift_min_level(&mut self, delta: isize) {
        let index = (self.min_level.index() as isize + delta).clamp(0, 7) as usize;
        self.min_level = Level::ALL[index];
    }

    /// Débit instantané, en lignes par seconde, moyenné sur 5 s.
    pub fn rate(&self) -> f64 {
        self.stats.timeline.rate(5)
    }
}

fn step(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
}
