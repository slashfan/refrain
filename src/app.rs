//! L'état de l'application et sa réaction aux événements.
//!
//! Séparation volontaire des rôles : ce module **décide**, `ui.rs` se contente
//! de **dessiner**. Les tableaux triés sont recalculés une fois par battement
//! d'horloge (et non à chaque image), ce qui garde le rendu quasi gratuit même
//! quand les logs défilent à 100 000 lignes par seconde.

use crate::cli::Cli;
use crate::event::Event;
use crate::export;
use crate::parser::Level;
use crate::stats::{ErrorStat, NPlusOne, Stats, StreamEntry};
use chrono::{DateTime, FixedOffset};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::cmp::Reverse;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

/// Durée d'affichage d'un message transitoire — « écrit dans … ». Assez long
/// pour être lu, assez court pour ne pas encombrer le bandeau.
const FLASH: Duration = Duration::from_secs(4);

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
            Tab::Overview => "Overview",
            Tab::Errors => "Errors",
            Tab::Endpoints => "Endpoints",
            Tab::Sql => "SQL",
            Tab::Stream => "Stream",
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
            RouteSort::Requests => "requests",
            RouteSort::Errors => "errors",
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
    /// L'endpoint suivi, s'il y en a un. Il filtre les erreurs, les motifs N+1
    /// et le flux — pas le tableau des endpoints, puisque c'est là qu'on le
    /// choisit.
    pub focus: Option<String>,
    /// Motif de recherche du flux. Vide : aucun filtre.
    pub search: String,
    /// La saisie du motif est en cours. Tant qu'elle l'est, les touches
    /// alimentent le motif au lieu de déclencher les raccourcis.
    pub searching: bool,
    pub route_sort: RouteSort,
    pub show_help: bool,
    pub should_quit: bool,
    pub sources_total: usize,
    pub sources_done: usize,
    pub failures: Vec<String>,
    pub started: Instant,
    /// Message transitoire du bandeau, avec l'instant où il s'efface.
    flash: Option<(String, Instant)>,
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
            focus: None,
            search: String::new(),
            searching: false,
            route_sort: RouteSort::P95,
            show_help: false,
            should_quit: false,
            sources_total,
            sources_done: 0,
            failures: Vec::new(),
            started: Instant::now(),
            flash: None,
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
            .filter(|(_, stat)| self.shows_endpoint(stat.endpoint.as_deref()))
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
            .filter(|(_, motif)| self.shows_endpoint(Some(motif.endpoint.as_str())))
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
        // Pendant la saisie d'un motif, tout caractère lui revient : sans ce
        // détour, chercher « queue » quitterait l'application dès le `q`.
        if self.searching {
            self.on_search_key(key);
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
            KeyCode::Char('q') => self.should_quit = true,
            // Échap défait ce qui est actif, du plus étroit au plus large, et
            // ne quitte que lorsqu'il ne reste rien à défaire. Sans cette
            // gradation, un filtre posé par mégarde ne se lèverait qu'en
            // relançant le programme.
            KeyCode::Esc => self.escape(),
            KeyCode::Enter => self.toggle_focus(),
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
            KeyCode::Char('/') => {
                // Le motif ne filtre que le flux : autant y emmener celui qui
                // le cherche, depuis n'importe quel onglet.
                self.tab = Tab::Stream;
                self.searching = true;
            }
            KeyCode::Char('w') => self.export_to_file(),
            KeyCode::Char('y') => self.export_to_clipboard(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.shift_min_level(1),
            KeyCode::Char('-') | KeyCode::Char('_') => self.shift_min_level(-1),
            _ => {}
        }
    }

    /// Écrit l'élément sélectionné dans un fichier du répertoire courant. Ne
    /// dépend de rien, et marche donc au bout d'un `ssh`, là où le
    /// presse-papier local est hors de portée.
    fn export_to_file(&mut self) {
        let message = match export::write_to(self, Path::new(".")) {
            Ok(path) => format!("written to {}", path.display()),
            Err(err) => format!("failed: {err}"),
        };
        self.set_flash(message);
    }

    /// Demande au terminal de mettre l'élément sélectionné dans le
    /// presse-papier — voir `export::clipboard_sequence` pour le pourquoi de
    /// cette façon de faire.
    fn export_to_clipboard(&mut self) {
        let report = export::report(self);
        let sequence = export::clipboard_sequence(&report.text);
        let mut out = std::io::stdout();
        let message = match out
            .write_all(sequence.as_bytes())
            .and_then(|()| out.flush())
        {
            // Le terminal ne répond rien : on ne peut pas savoir s'il a
            // vraiment honoré la demande, seulement qu'elle est partie.
            Ok(()) => format!("{} bytes sent to the clipboard", report.text.len()),
            Err(err) => format!("copy failed: {err}"),
        };
        self.set_flash(message);
    }

    fn set_flash(&mut self, message: String) {
        self.flash = Some((message, Instant::now() + FLASH));
    }

    /// Le message transitoire, tant qu'il n'a pas expiré. L'expiration se lit
    /// au moment de dessiner plutôt qu'elle ne se programme : un battement
    /// d'horloge passe toutes les 250 ms de toute façon.
    pub fn flash(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(message, _)| message.as_str())
    }

    /// Suit — ou cesse de suivre — l'endpoint sélectionné. Depuis l'onglet SQL,
    /// c'est l'endpoint du motif N+1 : c'est là qu'on découvre le coupable, et
    /// on veut voir dans la foulée ce qu'il fait d'autre.
    fn toggle_focus(&mut self) {
        let picked = match self.tab {
            Tab::Endpoints => self.route_rows.get(self.route_sel).map(|r| r.name.clone()),
            Tab::Sql => self
                .nplus1_rows
                .get(self.nplus1_sel)
                .map(|r| r.endpoint.clone()),
            _ => return,
        };
        let Some(picked) = picked else { return };
        // Deux fois la même touche sur la même ligne : on relâche.
        self.focus = if self.focus.as_deref() == Some(picked.as_str()) {
            None
        } else {
            Some(picked)
        };
        self.on_filter_changed();
    }

    fn escape(&mut self) {
        if !self.search.is_empty() {
            self.search.clear();
        } else if self.focus.is_some() {
            self.focus = None;
        } else {
            self.should_quit = true;
            return;
        }
        self.on_filter_changed();
    }

    /// Les listes visibles viennent de changer : on les reconstruit et on
    /// repart du haut. Garder la sélection viserait une ligne qui n'existe
    /// plus dans la liste réduite.
    ///
    /// La reconstruction est immédiate, et non repoussée au prochain battement
    /// d'horloge : une touche doit se voir tout de suite, pas un quart de
    /// seconde plus tard.
    fn on_filter_changed(&mut self) {
        self.error_sel = 0;
        self.nplus1_sel = 0;
        self.stream_offset = 0;
        self.refresh_views();
    }

    /// Cet endpoint passe-t-il le suivi en cours ? Une ligne sans endpoint
    /// connu ne passe pas : on ne peut pas affirmer qu'elle appartient à celui
    /// qu'on suit.
    fn shows_endpoint(&self, endpoint: Option<&str>) -> bool {
        match &self.focus {
            None => true,
            Some(focus) => endpoint == Some(focus.as_str()),
        }
    }

    /// Les touches pendant la saisie d'un motif. `Entrée` valide et rend la
    /// main aux raccourcis, `Échap` efface le motif — c'est le seul moyen de
    /// revenir au flux entier, et ça évite qu'un filtre oublié laisse croire
    /// que les logs se sont taris.
    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search.clear();
                self.searching = false;
            }
            KeyCode::Enter => self.searching = false,
            KeyCode::Backspace => {
                self.search.pop();
            }
            KeyCode::Char(c) => self.search.push(c),
            _ => return,
        }
        // Les lignes visibles viennent de changer : on recolle au présent,
        // sinon on défile dans un historique qui n'existe plus.
        self.stream_offset = 0;
    }

    /// Cette entrée a-t-elle sa place dans le flux ? Le niveau minimum, et le
    /// motif de recherche s'il y en a un.
    ///
    /// C'est ici que la décision se prend, pas dans le rendu : `ui.rs` dessine
    /// ce qu'on lui donne.
    pub fn stream_shows(&self, item: &StreamEntry) -> bool {
        let entry = &item.entry;
        if entry.level < self.min_level {
            return false;
        }
        if !self.shows_endpoint(item.endpoint.as_deref()) {
            return false;
        }
        if self.search.is_empty() {
            return true;
        }
        // Le canal et la route comptent autant que le message : on cherche
        // aussi bien « doctrine » que « app_login » ou « Connection refused ».
        contains_ignore_case(&entry.message, &self.search)
            || contains_ignore_case(&entry.channel, &self.search)
            || entry
                .route()
                .is_some_and(|route| contains_ignore_case(route, &self.search))
            || entry
                .request_uri()
                .is_some_and(|uri| contains_ignore_case(uri, &self.search))
            || item
                .endpoint
                .as_deref()
                .is_some_and(|name| contains_ignore_case(name, &self.search))
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

/// `contains`, insensible à la casse et sans allocation.
///
/// La solution évidente — `foin.to_lowercase().contains(&motif.to_lowercase())`
/// — recopierait chaque message à chaque image. On compare donc caractère par
/// caractère, en minuscules à la volée. `to_lowercase` rend un itérateur parce
/// qu'une minuscule peut compter plusieurs caractères (« İ » en donne deux) ;
/// `flat_map` les enchaîne des deux côtés, ce qui garde la comparaison juste.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.char_indices().any(|(start, _)| {
        let mut foin = haystack[start..].chars().flat_map(char::to_lowercase);
        let mut motif = needle.chars().flat_map(char::to_lowercase);
        loop {
            match (motif.next(), foin.next()) {
                // Le motif est épuisé : tout a correspondu.
                (None, _) => return true,
                // La ligne s'arrête avant le motif.
                (Some(_), None) => return false,
                (Some(m), Some(f)) if m == f => continue,
                _ => return false,
            }
        }
    })
}

fn step(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparaison_insensible_a_la_casse_et_aux_accents_composes() {
        assert!(contains_ignore_case("Connection refused", "REFUSED"));
        assert!(contains_ignore_case("ÉTAT de la file", "état"));
        assert!(contains_ignore_case("app_login", "_LOG"));
        assert!(!contains_ignore_case("app_login", "logout"));
        // Un motif vide ne filtre rien, et rien ne dépasse de la fin.
        assert!(contains_ignore_case("court", ""));
        assert!(!contains_ignore_case("ab", "abc"));
    }
}
