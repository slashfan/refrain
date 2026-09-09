//! Agrégation : c'est ici qu'un flot de lignes devient des chiffres utiles.
//!
//! Toutes les structures de ce module sont conçues pour une **borne mémoire
//! fixe** : on peut avaler 40 Go de logs sans que la consommation bouge. Les
//! quantiles s'appuient sur un échantillon glissant, l'axe du temps sur un
//! tampon circulaire, et les tables de regroupement ont un plafond.

use crate::cli::{Cli, DurationUnit};
use crate::parser::{Level, LogEntry};
use chrono::{DateTime, FixedOffset, Local, Utc};
use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Plafonds : au-delà, on cesse d'ajouter de nouvelles clés (on continue de
/// compter celles déjà connues). Sans ça, un identifiant qui se faufile dans une
/// route ferait exploser la mémoire.
const MAX_ROUTES: usize = 4096;
const MAX_ERRORS: usize = 4096;
const MAX_CHANNELS: usize = 512;
const MAX_OPEN_REQUESTS: usize = 20_000;
/// Formes de requêtes SQL dont on retient le texte.
const MAX_SQL_SHAPES: usize = 2048;
/// Motifs N+1 distincts suivis (couples endpoint × requête SQL).
const MAX_NPLUS1: usize = 1024;
/// Formes SQL distinctes suivies au sein d'une même requête HTTP : au-delà,
/// on continue de compter le total sans mémoriser de nouvelles formes.
const MAX_SHAPES_PER_REQUEST: usize = 256;

/// Noms de champs où l'on va chercher une durée, par ordre de préférence.
const DURATION_KEYS: [&str; 9] = [
    "duration_ms",
    "duration",
    "elapsed_ms",
    "elapsed",
    "response_time",
    "execution_time",
    "exec_time",
    "runtime",
    "request_time",
];

/// Noms de champs identifiant une requête, pour la corrélation.
const CORRELATION_KEYS: [&str; 5] = ["token", "uid", "request_id", "x-request-id", "trace_id"];

// ---------------------------------------------------------------------------
// Axe du temps
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
pub struct Bucket {
    pub total: u64,
    pub errors: u64,
}

/// Un tampon circulaire d'un seau par seconde : c'est ce qui donne les
/// sparklines et permet de repérer les pics.
///
/// Le principe : `head` désigne le seau de la seconde la plus récente. Quand
/// une entrée plus récente arrive, on avance `head` en remettant à zéro les
/// seaux traversés. Rien n'est jamais alloué après la construction.
pub struct Timeline {
    buckets: Vec<Bucket>,
    head: usize,
    head_epoch: i64,
    started: bool,
}

impl Timeline {
    pub fn new(seconds: usize) -> Self {
        Self {
            buckets: vec![Bucket::default(); seconds],
            head: 0,
            head_epoch: 0,
            started: false,
        }
    }

    pub fn record(&mut self, epoch: i64, is_error: bool) {
        let len = self.buckets.len();
        if !self.started {
            self.started = true;
            self.head_epoch = epoch;
        }

        if epoch > self.head_epoch {
            // On avance d'autant de secondes que nécessaire, en nettoyant au
            // passage. `min(len)` évite une boucle d'un million de tours si on
            // enchaîne deux fichiers datés à des mois d'écart.
            let advance = (epoch - self.head_epoch).min(len as i64) as usize;
            for _ in 0..advance {
                self.head = (self.head + 1) % len;
                self.buckets[self.head] = Bucket::default();
            }
            self.head_epoch = epoch;
        }

        let back = self.head_epoch - epoch;
        if back < 0 || back >= len as i64 {
            return; // trop ancien pour la fenêtre observée
        }
        let index = (self.head + len - back as usize) % len;
        let bucket = &mut self.buckets[index];
        bucket.total += 1;
        if is_error {
            bucket.errors += 1;
        }
    }

    /// Les `n` dernières secondes, du plus ancien au plus récent.
    pub fn series(&self, n: usize, pick: impl Fn(&Bucket) -> u64) -> Vec<u64> {
        let len = self.buckets.len();
        let n = n.min(len);
        (0..n)
            .map(|i| {
                let back = n - 1 - i;
                pick(&self.buckets[(self.head + len - back) % len])
            })
            .collect()
    }

    /// Le pic sur toute la fenêtre : (lignes/s, seconde epoch).
    pub fn peak(&self) -> (u64, i64) {
        let len = self.buckets.len();
        let mut best = (0u64, self.head_epoch);
        for back in 0..len {
            let bucket = &self.buckets[(self.head + len - back) % len];
            if bucket.total > best.0 {
                best = (bucket.total, self.head_epoch - back as i64);
            }
        }
        best
    }

    /// Débit moyen sur les `secs` dernières secondes.
    pub fn rate(&self, secs: usize) -> f64 {
        let sum: u64 = self.series(secs, |b| b.total).iter().sum();
        sum as f64 / secs.max(1) as f64
    }
}

// ---------------------------------------------------------------------------
// Compteurs par clé
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct ChannelStat {
    pub count: u64,
    pub errors: u64,
}

#[derive(Clone)]
pub struct ErrorStat {
    pub count: u64,
    pub level: Level,
    pub channel: String,
    pub exception: Option<String>,
    pub message: String,
    pub context: Option<String>,
    pub first_seen: Option<DateTime<FixedOffset>>,
    pub last_seen: Option<DateTime<FixedOffset>>,
    pub endpoint: Option<String>,
}

/// Statistiques d'un endpoint. Les durées sont conservées dans un échantillon
/// glissant de taille fixe : les quantiles portent donc sur les 1024 dernières
/// requêtes, ce qui est exactement ce qu'on veut sur un flux vivant.
#[derive(Default, Clone)]
pub struct RouteStat {
    pub requests: u64,
    pub errors: u64,
    pub timed: u64,
    pub sum_ms: f64,
    pub max_ms: f32,
    /// Requêtes HTTP closes pour cet endpoint : dénominateur de la moyenne SQL.
    pub closed_requests: u64,
    pub queries_total: u64,
    pub queries_max: u32,
    samples: Vec<f32>,
    cursor: usize,
}

impl RouteStat {
    const SAMPLES: usize = 1024;

    fn add_duration(&mut self, ms: f64) {
        self.timed += 1;
        self.sum_ms += ms;
        let ms = ms as f32;
        if ms > self.max_ms {
            self.max_ms = ms;
        }
        if self.samples.len() < Self::SAMPLES {
            self.samples.push(ms);
        } else {
            self.samples[self.cursor] = ms;
            self.cursor = (self.cursor + 1) % Self::SAMPLES;
        }
    }

    /// Quantiles des durées observées. `scratch` est un tampon réutilisé d'un
    /// appel à l'autre pour ne pas allouer un vecteur par route.
    pub fn quantiles(&self, scratch: &mut Vec<f32>) -> Quantiles {
        if self.samples.is_empty() {
            return Quantiles::default();
        }
        scratch.clear();
        scratch.extend_from_slice(&self.samples);
        // `sort_unstable_by` avec `total_cmp` : les flottants n'ont pas d'ordre
        // total « gratuit » en Rust (à cause de NaN), il faut le demander.
        scratch.sort_unstable_by(f32::total_cmp);
        Quantiles {
            p50: percentile(scratch, 0.50),
            p95: percentile(scratch, 0.95),
            p99: percentile(scratch, 0.99),
        }
    }

    /// Comptabilise les requêtes SQL d'une requête HTTP qui vient de se clore.
    /// Les requêtes sans SQL comptent aussi : sinon la moyenne serait gonflée.
    fn add_queries(&mut self, count: u32) {
        self.closed_requests += 1;
        self.queries_total += u64::from(count);
        self.queries_max = self.queries_max.max(count);
    }

    /// Requêtes SQL par requête HTTP, en moyenne.
    pub fn avg_queries(&self) -> f32 {
        if self.closed_requests == 0 {
            0.0
        } else {
            self.queries_total as f32 / self.closed_requests as f32
        }
    }

    pub fn avg_ms(&self) -> f32 {
        if self.timed == 0 {
            0.0
        } else {
            (self.sum_ms / self.timed as f64) as f32
        }
    }

    pub fn error_rate(&self) -> f32 {
        if self.requests == 0 {
            0.0
        } else {
            self.errors as f32 / self.requests as f32
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Quantiles {
    pub p50: f32,
    pub p95: f32,
    pub p99: f32,
}

fn percentile(sorted: &[f32], p: f64) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

// ---------------------------------------------------------------------------
// D'où vient la durée d'une requête
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum DurationSource {
    /// Aucune durée exploitable trouvée pour l'instant.
    Unknown,
    /// Un champ de `context`/`extra` porte directement la durée.
    Field { key: String, unit: DurationUnit },
    /// Déduite en corrélant les lignes d'une même requête par un identifiant.
    Correlated { key: String },
}

impl DurationSource {
    pub fn label(&self) -> String {
        match self {
            Self::Unknown => "aucune".into(),
            Self::Field { key, .. } => format!("champ « {key} »"),
            Self::Correlated { key } => format!("corrélation « {key} »"),
        }
    }
}

/// Suit les requêtes « ouvertes » pour en déduire une durée.
///
/// Sans champ de durée, on peut quand même mesurer : toutes les lignes d'une
/// même requête partagent un identifiant (le `token` de Symfony, ou l'`uid` du
/// `UidProcessor` de Monolog). La durée est alors l'écart entre la première et
/// la dernière ligne portant cet identifiant.
pub struct RequestTracker {
    pub key: Option<String>,
    pub enabled: bool,
    timeout_ms: f64,
    open: HashMap<String, OpenRequest>,
}

struct OpenRequest {
    first_ms: i64,
    last_ms: i64,
    endpoint: Option<String>,
    /// Empreinte de requête SQL → nombre d'exécutions dans cette requête HTTP.
    queries: HashMap<u64, u32>,
    /// Total, y compris les formes non mémorisées faute de place.
    query_count: u32,
}

pub struct FinishedRequest {
    pub endpoint: String,
    pub ms: f64,
    pub queries: Vec<(u64, u32)>,
    pub query_count: u32,
}

/// Un motif N+1 : une même requête SQL répétée au sein d'une seule requête HTTP.
#[derive(Clone)]
pub struct NPlusOne {
    pub endpoint: String,
    pub sql: String,
    /// Nombre de requêtes HTTP où le motif a été observé.
    pub requests: u64,
    /// Pire répétition vue sur une seule requête HTTP.
    pub max_count: u32,
    total_count: u64,
    pub last_seen: Option<DateTime<FixedOffset>>,
}

impl NPlusOne {
    /// Répétitions par requête HTTP, en moyenne.
    pub fn avg_count(&self) -> f32 {
        if self.requests == 0 {
            0.0
        } else {
            self.total_count as f32 / self.requests as f32
        }
    }
}

impl RequestTracker {
    fn new(key: Option<String>, enabled: bool, timeout_secs: f64) -> Self {
        Self {
            key,
            enabled,
            timeout_ms: timeout_secs * 1000.0,
            open: HashMap::new(),
        }
    }

    /// Cherche l'identifiant de requête dans l'entrée, en verrouillant la clé
    /// dès qu'on en a trouvé une qui marche.
    fn token_of<'a>(&mut self, entry: &'a LogEntry) -> Option<&'a str> {
        if !self.enabled {
            return None;
        }
        if let Some(key) = &self.key {
            return entry.lookup(key).and_then(value_as_token);
        }
        for candidate in CORRELATION_KEYS {
            if let Some(token) = entry.lookup(candidate).and_then(value_as_token) {
                self.key = Some(candidate.to_string());
                return Some(token);
            }
        }
        None
    }

    /// Enregistre la ligne dans sa requête et renvoie l'endpoint connu pour elle
    /// — ce qui permet d'attribuer une erreur sans contexte de route.
    fn observe(
        &mut self,
        entry: &LogEntry,
        endpoint: Option<&str>,
        ms: i64,
        sql: Option<u64>,
    ) -> Option<String> {
        let token = self.token_of(entry)?.to_string();

        if self.open.len() >= MAX_OPEN_REQUESTS && !self.open.contains_key(&token) {
            return None;
        }
        let open = self.open.entry(token).or_insert_with(|| OpenRequest {
            first_ms: ms,
            last_ms: ms,
            endpoint: None,
            queries: HashMap::new(),
            query_count: 0,
        });
        open.last_ms = open.last_ms.max(ms);
        open.first_ms = open.first_ms.min(ms);
        if let Some(endpoint) = endpoint {
            open.endpoint = Some(endpoint.to_string());
        }
        if let Some(fingerprint) = sql {
            open.query_count = open.query_count.saturating_add(1);
            let connue = open.queries.contains_key(&fingerprint);
            if connue || open.queries.len() < MAX_SHAPES_PER_REQUEST {
                *open.queries.entry(fingerprint).or_insert(0) += 1;
            }
        }
        open.endpoint.clone()
    }

    /// Clôt les requêtes sans nouvelle ligne depuis `timeout_ms`.
    fn sweep(&mut self, now_ms: i64) -> Vec<FinishedRequest> {
        let mut done = Vec::new();
        // `retain` parcourt la table une fois et supprime au passage : bien plus
        // efficace que collecter les clés puis les retirer une par une.
        self.open.retain(|_, open| {
            if ((now_ms - open.last_ms) as f64) < self.timeout_ms {
                return true;
            }
            if let Some(endpoint) = &open.endpoint {
                done.push(FinishedRequest {
                    endpoint: endpoint.clone(),
                    ms: (open.last_ms - open.first_ms) as f64,
                    // `take` récupère la table sans la copier : la requête est
                    // détruite juste après, de toute façon.
                    queries: std::mem::take(&mut open.queries).into_iter().collect(),
                    query_count: open.query_count,
                });
            }
            false
        });
        done
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }
}

fn value_as_token(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// L'agrégat complet
// ---------------------------------------------------------------------------

pub struct Stats {
    pub total: u64,
    pub skipped: u64,
    pub by_level: [u64; 8],
    pub channels: HashMap<String, ChannelStat>,
    pub errors: HashMap<String, ErrorStat>,
    pub routes: HashMap<String, RouteStat>,
    /// Motifs N+1, indexés par (endpoint, empreinte de la requête SQL).
    pub nplus1: HashMap<(String, u64), NPlusOne>,
    /// Dictionnaire empreinte → texte SQL : le texte n'est stocké qu'une seule
    /// fois, et non dans chacune des requêtes ouvertes.
    sql_texts: HashMap<u64, String>,
    nplus1_threshold: u32,
    pub timeline: Timeline,
    pub recent: VecDeque<LogEntry>,
    pub tracker: RequestTracker,
    pub duration: DurationSource,
    pub first_ts: Option<DateTime<FixedOffset>>,
    pub last_ts: Option<DateTime<FixedOffset>>,
    forced_unit: DurationUnit,
    forced_key: Option<String>,
    scrollback: usize,
    /// Horloge de chaque source : la date de la dernière ligne qu'elle a
    /// livrée. Le balayage des requêtes corrélées se cale sur la plus en
    /// retard d'entre elles — sinon un fichier lu plus vite que les autres
    /// clôturerait des requêtes dont les lignes attendent encore d'être lues.
    clocks: Vec<i64>,
    last_sweep_ms: i64,
    saw_matched_route: bool,
    wall_guard_ms: i64,
}

impl Stats {
    pub fn new(cli: &Cli) -> Self {
        Self {
            total: 0,
            skipped: 0,
            by_level: [0; 8],
            channels: HashMap::new(),
            errors: HashMap::new(),
            routes: HashMap::new(),
            nplus1: HashMap::new(),
            sql_texts: HashMap::new(),
            nplus1_threshold: cli.nplus1,
            timeline: Timeline::new(600),
            recent: VecDeque::with_capacity(cli.scrollback.min(1024)),
            tracker: RequestTracker::new(
                cli.correlate_key.clone(),
                !cli.no_correlate,
                cli.correlate_timeout,
            ),
            duration: DurationSource::Unknown,
            first_ts: None,
            last_ts: None,
            forced_unit: cli.duration_unit,
            forced_key: cli.duration_key.clone(),
            scrollback: cli.scrollback,
            clocks: vec![i64::MIN; cli.files.len().max(1)],
            last_sweep_ms: i64::MIN,
            saw_matched_route: false,
            wall_guard_ms: i64::MIN,
        }
    }

    pub fn reset(&mut self, cli: &Cli) {
        *self = Self::new(cli);
    }

    pub fn ingest(&mut self, source: usize, entry: LogEntry) {
        self.total += 1;
        self.by_level[entry.level.index()] += 1;
        let is_error = entry.level.is_error();

        // Horloge de référence : celle des logs quand elle existe (ça permet de
        // rejouer un vieux fichier avec des pics au bon endroit), sinon la nôtre.
        let now_ms = match entry.ts {
            Some(ts) => {
                self.last_ts = Some(ts);
                self.first_ts.get_or_insert(ts);
                ts.timestamp_millis()
            }
            None => Utc::now().timestamp_millis(),
        };
        self.set_clock(source, now_ms);
        // Le garde-fou ne vaut que pour l'axe du temps : les durées, elles,
        // doivent rester calculées sur les vraies dates des lignes.
        let bucket_ms = self.clamp_future(now_ms);
        self.timeline.record(bucket_ms.div_euclid(1000), is_error);

        // -- canaux -------------------------------------------------------
        if self.channels.len() < MAX_CHANNELS || self.channels.contains_key(&entry.channel) {
            let channel = self.channels.entry(entry.channel.clone()).or_default();
            channel.count += 1;
            if is_error {
                channel.errors += 1;
            }
        }

        // -- durée ---------------------------------------------------------
        let field_ms = self.duration_of(&entry);
        // Doctrine journalise chaque requête dans `context.sql`. On la réduit à
        // une empreinte 64 bits, en mémorisant son texte une seule fois.
        let sql = entry
            .lookup("sql")
            .and_then(Value::as_str)
            .map(|sql| self.intern_sql(sql));
        let own_endpoint = entry.endpoint();
        let known_endpoint = self
            .tracker
            .observe(&entry, own_endpoint.as_deref(), now_ms, sql);
        let endpoint = own_endpoint.or(known_endpoint);

        // Une « requête » = une ligne « Matched route » : Symfony en écrit
        // exactement une par requête HTTP, c'est le marqueur le plus fiable. Si
        // le flux n'en contient pas, on se rabat sur les lignes portant une durée.
        let matched = is_matched_route(&entry);
        self.saw_matched_route |= matched;
        let counts_as_request = matched || (!self.saw_matched_route && field_ms.is_some());

        if let Some(name) = &endpoint
            && (counts_as_request || is_error || field_ms.is_some())
            && (self.routes.len() < MAX_ROUTES || self.routes.contains_key(name))
        {
            let route = self.routes.entry(name.clone()).or_default();
            if counts_as_request {
                route.requests += 1;
            }
            if is_error {
                route.errors += 1;
            }
            if let Some(ms) = field_ms {
                route.add_duration(ms);
            }
        }

        // -- erreurs -------------------------------------------------------
        if is_error {
            self.record_error(&entry, endpoint);
        }

        // -- flux ----------------------------------------------------------
        if self.recent.len() >= self.scrollback {
            self.recent.pop_front();
        }
        self.recent.push_back(entry);

        // -- clôture des requêtes corrélées --------------------------------
        // Une fois par seconde suffit : `sweep` parcourt toute la table.
        // `saturating_sub` : au tout premier appel `last_sweep_ms` vaut i64::MIN,
        // et une soustraction normale déborderait.
        let watermark = self.watermark(now_ms);
        if watermark.saturating_sub(self.last_sweep_ms) > 1000 {
            self.last_sweep_ms = watermark;
            self.close_finished(watermark);
        }
    }

    /// Le point de synchronisation entre sources : la date jusqu'à laquelle
    /// *toutes* ont livré leurs lignes.
    ///
    /// Chaque fichier est lu par son propre thread, aussi vite qu'il le peut.
    /// Deux fichiers couvrant la même période n'ont donc aucune raison d'y
    /// progresser à la même vitesse : `prod.log` peut être arrivé à midi quand
    /// `doctrine.log` en est encore à dix heures. Balayer à l'horloge du plus
    /// rapide clôturerait les requêtes du plus lent avant même d'avoir lu leurs
    /// lignes. On se cale donc sur la source la plus en retard.
    fn watermark(&self, fallback: i64) -> i64 {
        match self.clocks.iter().copied().min() {
            // `i64::MAX` : toutes les sources sont taries, plus rien à attendre.
            Some(ms) if ms != i64::MAX => ms,
            _ => fallback,
        }
    }

    /// L'horloge suit la dernière ligne livrée, sans jamais la majorer : c'est
    /// là qu'en est le lecteur, et une seule source retrouve ainsi exactement le
    /// comportement d'avant — se caler sur le maximum vu clôturerait plus tôt.
    fn set_clock(&mut self, source: usize, ms: i64) {
        if let Some(clock) = self.clocks.get_mut(source) {
            *clock = ms;
        }
    }

    /// Une source a rattrapé la fin de son fichier : ses prochaines lignes
    /// arriveront en direct, son horloge est donc celle du mur.
    pub fn source_caught_up(&mut self, source: usize) {
        self.set_clock(source, Utc::now().timestamp_millis());
    }

    /// Une source est close pour de bon : elle ne retient plus le balayage.
    pub fn source_done(&mut self, source: usize) {
        if let Some(clock) = self.clocks.get_mut(source) {
            *clock = i64::MAX;
        }
    }

    /// Empêche une ligne datée dans le futur de propulser l'axe du temps.
    ///
    /// Une seule ligne en avance — dérive d'horloge, log recopié d'une autre
    /// machine, requête très longue journalisée à son ouverture — suffirait
    /// sinon à vider tout le tampon circulaire et à afficher un débit nul.
    /// On ne lit l'horloge que quand une date dépasse le dernier garde-fou,
    /// soit environ une fois par seconde en suivi live.
    fn clamp_future(&mut self, ts_ms: i64) -> i64 {
        if ts_ms <= self.wall_guard_ms {
            return ts_ms;
        }
        self.wall_guard_ms = Utc::now().timestamp_millis() + 1_000;
        ts_ms.min(self.wall_guard_ms)
    }

    fn record_error(&mut self, entry: &LogEntry, endpoint: Option<String>) {
        let signature = entry.signature();
        if self.errors.len() >= MAX_ERRORS && !self.errors.contains_key(&signature) {
            return;
        }
        let stat = self.errors.entry(signature).or_insert_with(|| ErrorStat {
            count: 0,
            level: entry.level,
            channel: entry.channel.clone(),
            exception: entry.exception_class().map(|c| c.to_string()),
            message: String::new(),
            context: None,
            first_seen: entry.ts,
            last_seen: entry.ts,
            endpoint: None,
        });

        stat.count += 1;
        stat.last_seen = entry.ts.or(stat.last_seen);
        // On garde toujours le niveau le plus grave vu pour cette signature.
        stat.level = stat.level.max(entry.level);
        stat.message = entry.message.clone();
        stat.context = entry
            .context
            .as_ref()
            .and_then(|c| serde_json::to_string_pretty(c).ok());
        if let Some(endpoint) = endpoint {
            stat.endpoint = Some(match entry.method() {
                Some(method) => format!("{method} {endpoint}"),
                None => endpoint,
            });
        }
    }

    fn close_finished(&mut self, now_ms: i64) -> usize {
        // Quand un champ de durée existe, il fait foi. On continue de suivre les
        // requêtes (ça sert à rattacher une erreur à son endpoint), mais on
        // n'injecte pas une seconde mesure pour la même requête.
        let field_mode = matches!(self.duration, DurationSource::Field { .. });

        let threshold = self.nplus1_threshold;
        let seen_at = self.last_ts;
        let mut closed = 0;

        for finished in self.tracker.sweep(now_ms) {
            closed += 1;
            // Un N+1, c'est la même requête SQL répétée au sein d'une seule
            // requête HTTP. Doctrine journalisant des requêtes *préparées*
            // (`WHERE id = ?`), deux exécutions d'un même motif produisent
            // exactement la même chaîne : l'égalité suffit, aucune
            // normalisation à écrire.
            if threshold > 0 {
                for (fingerprint, count) in &finished.queries {
                    if *count >= threshold {
                        self.record_nplus1(&finished.endpoint, *fingerprint, *count, seen_at);
                    }
                }
            }

            let is_new = !self.routes.contains_key(&finished.endpoint);
            if is_new && self.routes.len() >= MAX_ROUTES {
                continue;
            }
            let route = self.routes.entry(finished.endpoint).or_default();
            route.add_queries(finished.query_count);
            if !field_mode {
                route.add_duration(finished.ms);
            }
        }
        // On n'annonce la corrélation comme source qu'une fois qu'elle produit
        // vraiment des mesures : afficher une promesse vide serait trompeur.
        if matches!(self.duration, DurationSource::Unknown)
            && let Some(key) = &self.tracker.key
            && self.routes.values().any(|r| r.timed > 0)
        {
            self.duration = DurationSource::Correlated { key: key.clone() };
        }
        closed
    }

    fn record_nplus1(
        &mut self,
        endpoint: &str,
        fingerprint: u64,
        count: u32,
        seen_at: Option<DateTime<FixedOffset>>,
    ) {
        let key = (endpoint.to_string(), fingerprint);
        if self.nplus1.len() >= MAX_NPLUS1 && !self.nplus1.contains_key(&key) {
            return;
        }
        let sql = self
            .sql_texts
            .get(&fingerprint)
            .cloned()
            .unwrap_or_default();
        let motif = self.nplus1.entry(key).or_insert_with(|| NPlusOne {
            endpoint: endpoint.to_string(),
            sql,
            requests: 0,
            max_count: 0,
            total_count: 0,
            last_seen: None,
        });
        motif.requests += 1;
        motif.max_count = motif.max_count.max(count);
        motif.total_count += u64::from(count);
        motif.last_seen = seen_at.or(motif.last_seen);
    }

    /// Empreinte 64 bits d'une requête SQL, dont le texte est mémorisé au passage.
    fn intern_sql(&mut self, sql: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        sql.hash(&mut hasher);
        let fingerprint = hasher.finish();

        if self.sql_texts.len() < MAX_SQL_SHAPES {
            self.sql_texts.entry(fingerprint).or_insert_with(|| {
                // Espaces normalisés : le SQL journalisé est parfois indenté sur
                // plusieurs lignes, ce qui le rend illisible dans un tableau.
                let mut text = sql.split_whitespace().collect::<Vec<_>>().join(" ");
                crate::parser::truncate_chars(&mut text, 400);
                text
            });
        }
        fingerprint
    }

    /// Clôt les requêtes en attente quand plus rien n'arrive.
    ///
    /// En suivi live, la dernière requête reste ouverte tant qu'aucune nouvelle
    /// ligne ne fait avancer l'horloge des logs. On se rabat alors sur l'horloge
    /// murale — mais **seulement à l'arrêt**, pour ne pas couper une requête en
    /// deux au milieu de l'analyse d'un gros fichier.
    /// Renvoie le nombre de requêtes effectivement closes.
    pub fn sweep_idle(&mut self) -> usize {
        self.close_finished(Utc::now().timestamp_millis())
    }

    /// Vide les requêtes encore ouvertes : appelé en fin de fichier, sinon la
    /// dernière poignée de requêtes ne serait jamais comptabilisée.
    pub fn finalize(&mut self) {
        let now_ms = self
            .last_ts
            .map(|t| t.timestamp_millis())
            .unwrap_or_else(|| Utc::now().timestamp_millis());
        // Un balayage très loin dans le futur ferme tout.
        self.close_finished(now_ms.saturating_add(1_000_000_000));
    }

    /// Extrait une durée en millisecondes de l'entrée, en mémorisant le champ
    /// utilisé la première fois qu'on en trouve un.
    fn duration_of(&mut self, entry: &LogEntry) -> Option<f64> {
        if let Some(key) = &self.forced_key {
            let value = entry.lookup(key)?;
            return to_millis(value, key, self.forced_unit);
        }
        if let DurationSource::Field { key, unit } = &self.duration {
            let value = entry.lookup(key)?;
            return to_millis(value, key, *unit);
        }
        if entry.context.is_none() && entry.extra.is_none() {
            return None;
        }
        for candidate in DURATION_KEYS {
            if let Some(value) = entry.lookup(candidate)
                && let Some(ms) = to_millis(value, candidate, self.forced_unit)
            {
                self.duration = DurationSource::Field {
                    key: candidate.to_string(),
                    unit: self.forced_unit,
                };
                return Some(ms);
            }
        }
        None
    }

    /// Nombre de formes de requêtes SQL distinctes rencontrées.
    pub fn sql_shapes(&self) -> usize {
        self.sql_texts.len()
    }

    pub fn errors_total(&self) -> u64 {
        Level::ALL
            .iter()
            .filter(|l| l.is_error())
            .map(|l| self.by_level[l.index()])
            .sum()
    }

    /// Durée couverte par les logs analysés, en secondes.
    pub fn span_secs(&self) -> f64 {
        match (self.first_ts, self.last_ts) {
            (Some(a), Some(b)) => (b - a).num_milliseconds() as f64 / 1000.0,
            _ => 0.0,
        }
    }
}

/// Le marqueur que Symfony écrit exactement une fois par requête HTTP.
fn is_matched_route(entry: &LogEntry) -> bool {
    entry.channel == "request" && entry.message.starts_with("Matched route")
}

/// Convertit la valeur d'un champ de durée en millisecondes.
///
/// Accepte les nombres (`123.5`) comme les chaînes (`"123ms"`, `"1.5s"`).
fn to_millis(value: &Value, key: &str, forced: DurationUnit) -> Option<f64> {
    let (raw, unit_from_value) = match value {
        Value::Number(n) => (n.as_f64()?, None),
        Value::String(s) => parse_number_with_unit(s)?,
        _ => return None,
    };
    if raw < 0.0 || !raw.is_finite() {
        return None;
    }

    let unit = match (forced, unit_from_value) {
        (DurationUnit::Auto, Some(unit)) => unit,
        (DurationUnit::Auto, None) => infer_unit(key, raw),
        (forced, _) => forced,
    };

    Some(match unit {
        DurationUnit::S => raw * 1000.0,
        DurationUnit::Us => raw / 1000.0,
        _ => raw,
    })
}

/// `"1.5s"` → `(1.5, Some(S))`, `"120"` → `(120.0, None)`
fn parse_number_with_unit(s: &str) -> Option<(f64, Option<DurationUnit>)> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(s.len());
    let value: f64 = s[..split].parse().ok()?;
    let unit = match s[split..].trim().to_ascii_lowercase().as_str() {
        "" => None,
        "ms" | "msec" => Some(DurationUnit::Ms),
        "s" | "sec" | "secs" => Some(DurationUnit::S),
        "us" | "µs" | "μs" => Some(DurationUnit::Us),
        _ => return None,
    };
    Some((value, unit))
}

/// Devine l'unité : d'abord par le suffixe du nom de clé, ce qui est fiable ;
/// à défaut par l'ordre de grandeur, car PHP mesure traditionnellement en
/// secondes flottantes (`microtime(true)` en donne 0.0123 pour 12 ms).
fn infer_unit(key: &str, value: f64) -> DurationUnit {
    let key = key.to_ascii_lowercase();
    if key.ends_with("_ms") || key.ends_with("ms") {
        DurationUnit::Ms
    } else if key.ends_with("_us") || key.ends_with("micro") {
        DurationUnit::Us
    } else if key.ends_with("_s")
        || key.ends_with("_sec")
        || key.ends_with("seconds")
        // Pas de suffixe parlant : un flottant sous 30 est un `microtime(true)`,
        // donc des secondes. Un entier, lui, est presque toujours des ms.
        || (value > 0.0 && value < 30.0 && value.fract() != 0.0)
    {
        DurationUnit::S
    } else {
        DurationUnit::Ms
    }
}

/// Formate une durée pour l'affichage : « 12.3 ms », « 1.24 s ».
pub fn format_ms(ms: f32) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}

/// Formate un grand nombre avec des espaces fines : « 1 234 567 ».
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

pub fn format_time(ts: Option<DateTime<FixedOffset>>) -> String {
    match ts {
        Some(ts) => ts.with_timezone(&Local).format("%H:%M:%S").to_string(),
        None => "--:--:--".into(),
    }
}

/// Le résumé texte du mode `--summary`.
pub fn render_summary(stats: &Stats) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut scratch = Vec::new();

    let _ = writeln!(out, "── ruru ─ résumé ───────────────────────────────");
    let _ = writeln!(
        out,
        "{} entrées analysées ({} ignorées), {} erreurs",
        format_count(stats.total),
        format_count(stats.skipped),
        format_count(stats.errors_total())
    );
    if stats.span_secs() > 0.0 {
        let _ = writeln!(
            out,
            "période : {} → {} ({:.0} s)",
            format_time(stats.first_ts),
            format_time(stats.last_ts),
            stats.span_secs()
        );
    }
    let (peak, _) = stats.timeline.peak();
    let _ = writeln!(out, "pic : {} lignes/s", format_count(peak));
    let _ = writeln!(out, "durées : {}", stats.duration.label());

    let _ = writeln!(out, "\nNiveaux");
    for level in Level::ALL.iter().rev() {
        let count = stats.by_level[level.index()];
        if count > 0 {
            let _ = writeln!(out, "  {:<10} {:>10}", level.as_str(), format_count(count));
        }
    }

    let mut errors: Vec<_> = stats.errors.iter().collect();
    errors.sort_unstable_by_key(|(_, stat)| Reverse(stat.count));
    if !errors.is_empty() {
        let _ = writeln!(out, "\nTop erreurs");
        for (signature, stat) in errors.iter().take(10) {
            let _ = writeln!(
                out,
                "  {:>7} × [{}] {}",
                format_count(stat.count),
                stat.channel,
                signature
            );
        }
    }

    // On calcule les quantiles une seule fois par route, puis on trie : les
    // recalculer dans le comparateur les referait O(n log n) fois.
    let mut routes: Vec<_> = stats
        .routes
        .iter()
        .filter(|(_, route)| route.timed > 0)
        .map(|(name, route)| (name, route, route.quantiles(&mut scratch)))
        .collect();
    routes.sort_unstable_by(|a, b| b.2.p95.total_cmp(&a.2.p95));

    if !routes.is_empty() {
        let _ = writeln!(out, "\nEndpoints les plus lents (p95)");
        for (name, route, quantiles) in routes.iter().take(10) {
            let _ = writeln!(
                out,
                "  {:<40} n={:<6} p50={:<10} p95={:<10} max={}",
                truncate(name, 40),
                route.requests.max(route.timed),
                format_ms(quantiles.p50),
                format_ms(quantiles.p95),
                format_ms(route.max_ms)
            );
        }
    } else if !stats.routes.is_empty() {
        let _ = writeln!(
            out,
            "\nAucune durée mesurable. Voir la section « Mesurer les durées » du README."
        );
    }

    let mut motifs: Vec<&NPlusOne> = stats.nplus1.values().collect();
    motifs.sort_unstable_by(|a, b| {
        b.max_count
            .cmp(&a.max_count)
            .then_with(|| b.requests.cmp(&a.requests))
    });
    if !motifs.is_empty() {
        let _ = writeln!(
            out,
            "\nMotifs N+1 (même requête SQL répétée dans une requête HTTP)"
        );
        for motif in motifs.iter().take(10) {
            let _ = writeln!(
                out,
                "  {:<22} {:>4} × au pire, {:>5.1} × en moyenne sur {} requêtes",
                truncate(&motif.endpoint, 22),
                motif.max_count,
                motif.avg_count(),
                format_count(motif.requests)
            );
            let _ = writeln!(out, "      {}", truncate(&motif.sql, 90));
        }
    }
    out
}

/// Instantané des compteurs au format JSON, pour du monitoring.
///
/// Les totaux sont **cumulés** depuis le démarrage, à la façon d'un compteur
/// Prometheus : c'est au collecteur de faire les différences d'un relevé à
/// l'autre. `throughput` fournit en plus des débits sur fenêtre glissante,
/// directement exploitables sans état côté collecteur.
pub fn render_json(stats: &Stats, top: usize, pretty: bool) -> String {
    let mut scratch = Vec::with_capacity(RouteStat::SAMPLES);

    let levels: serde_json::Map<String, Value> = Level::ALL
        .iter()
        .map(|level| {
            (
                level.lower().to_string(),
                json!(stats.by_level[level.index()]),
            )
        })
        .collect();

    let mut channels: Vec<_> = stats.channels.iter().collect();
    channels.sort_unstable_by_key(|(_, channel)| Reverse(channel.count));
    let channels: Vec<Value> = channels
        .iter()
        .map(|(name, channel)| {
            json!({ "channel": name, "count": channel.count, "errors": channel.errors })
        })
        .collect();

    let mut errors: Vec<_> = stats.errors.iter().collect();
    errors.sort_unstable_by_key(|(_, error)| Reverse(error.count));
    keep_top(&mut errors, top);
    let errors: Vec<Value> = errors
        .iter()
        .map(|(signature, error)| {
            json!({
                "signature": signature,
                "count": error.count,
                "level": error.level.lower(),
                "channel": error.channel,
                "exception": error.exception,
                "endpoint": error.endpoint,
                "first_seen": error.first_seen.map(|ts| ts.to_rfc3339()),
                "last_seen": error.last_seen.map(|ts| ts.to_rfc3339()),
                "message": error.message.lines().next().unwrap_or_default(),
            })
        })
        .collect();

    let mut endpoints: Vec<_> = stats
        .routes
        .iter()
        .map(|(name, route)| (name, route, route.quantiles(&mut scratch)))
        .collect();
    endpoints.sort_unstable_by(|a, b| b.2.p95.total_cmp(&a.2.p95));
    keep_top(&mut endpoints, top);
    let endpoints: Vec<Value> = endpoints
        .iter()
        .map(|(name, route, quantiles)| {
            json!({
                "endpoint": name,
                "requests": route.requests.max(route.timed),
                "errors": route.errors,
                "error_rate": round(f64::from(route.error_rate()), 4),
                "timed": route.timed,
                "p50_ms": round(f64::from(quantiles.p50), 2),
                "p95_ms": round(f64::from(quantiles.p95), 2),
                "p99_ms": round(f64::from(quantiles.p99), 2),
                "max_ms": round(f64::from(route.max_ms), 2),
                "avg_ms": round(f64::from(route.avg_ms()), 2),
                "queries_avg": round(f64::from(route.avg_queries()), 1),
                "queries_max": route.queries_max,
            })
        })
        .collect();

    let mut motifs: Vec<&NPlusOne> = stats.nplus1.values().collect();
    motifs.sort_unstable_by(|a, b| {
        b.max_count
            .cmp(&a.max_count)
            .then_with(|| b.requests.cmp(&a.requests))
    });
    keep_top(&mut motifs, top);
    let nplus1: Vec<Value> = motifs
        .iter()
        .map(|motif| {
            json!({
                "endpoint": motif.endpoint,
                "sql": motif.sql,
                "requests_affected": motif.requests,
                "max_per_request": motif.max_count,
                "avg_per_request": round(f64::from(motif.avg_count()), 1),
                "last_seen": motif.last_seen.map(|ts| ts.to_rfc3339()),
            })
        })
        .collect();

    let (peak, peak_epoch) = stats.timeline.peak();
    let errors_total = stats.errors_total();

    let snapshot = json!({
        "generated_at": Local::now().to_rfc3339(),
        "window": {
            "first_seen": stats.first_ts.map(|ts| ts.to_rfc3339()),
            "last_seen": stats.last_ts.map(|ts| ts.to_rfc3339()),
            "span_seconds": round(stats.span_secs(), 3),
        },
        "totals": {
            "entries": stats.total,
            "skipped": stats.skipped,
            "errors": errors_total,
            "error_rate": round(ratio(errors_total, stats.total), 4),
        },
        "levels": levels,
        "throughput": {
            "peak_per_second": peak,
            "peak_at": (peak > 0).then(|| epoch_to_rfc3339(peak_epoch)).flatten(),
            "last_5s_per_second": round(stats.timeline.rate(5), 2),
            "last_60s_per_second": round(stats.timeline.rate(60), 2),
        },
        "duration_source": match &stats.duration {
            DurationSource::Unknown => json!({ "kind": "none" }),
            DurationSource::Field { key, .. } => json!({ "kind": "field", "key": key }),
            DurationSource::Correlated { key } => json!({ "kind": "correlation", "key": key }),
        },
        "open_requests": stats.tracker.open_count(),
        "sql": {
            "shapes": stats.sql_shapes(),
            "nplus1_threshold": stats.nplus1_threshold,
        },
        "channels": channels,
        "errors": errors,
        "endpoints": endpoints,
        "nplus1": nplus1,
    });

    if pretty {
        serde_json::to_string_pretty(&snapshot).unwrap_or_default()
    } else {
        snapshot.to_string()
    }
}

/// Ne garde que les `top` premiers éléments. `0` veut dire « tout garder ».
fn keep_top<T>(items: &mut Vec<T>, top: usize) {
    if top > 0 {
        items.truncate(top);
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// Arrondit, pour ne pas noyer la sortie sous quinze décimales de bruit flottant.
fn round(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

fn epoch_to_rfc3339(epoch: i64) -> Option<String> {
    DateTime::from_timestamp(epoch, 0).map(|ts| ts.with_timezone(&Local).to_rfc3339())
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_line;
    use clap::Parser;
    use serde_json::json;

    fn stats() -> Stats {
        Stats::new(&Cli::parse_from(["ruru", "prod.log"]))
    }

    /// Une requête Symfony typique. `duration` place ou non le champ de durée
    /// sur la ligne finale, pour exercer les deux modes de mesure.
    fn requete(token: &str, duration: bool) -> Vec<LogEntry> {
        let fin = if duration {
            r#"{"route":"app_home","method":"GET","status":200,"duration_ms":120.0}"#
        } else {
            r#"{"route":"app_home","method":"GET","status":200}"#
        };
        [
            format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {{"route":"app_home","request_uri":"https://x.test/","method":"GET"}} {{"token":"{token}"}}"#
            ),
            format!(
                r#"[2026-09-09T10:00:00.050000+02:00] doctrine.DEBUG: Executing statement {{"sql":"SELECT 1"}} {{"token":"{token}"}}"#
            ),
            format!(
                r#"[2026-09-09T10:00:00.120000+02:00] request.INFO: Request finished {fin} {{"token":"{token}"}}"#
            ),
        ]
        .iter()
        .map(|line| parse_line(line).expect("ligne valide"))
        .collect()
    }

    #[test]
    fn timeline_compte_par_seconde_et_repere_le_pic() {
        let mut timeline = Timeline::new(10);
        timeline.record(1000, false);
        timeline.record(1000, true);
        timeline.record(1002, false);

        assert_eq!(timeline.series(3, |b| b.total), vec![2, 0, 1]);
        assert_eq!(timeline.series(3, |b| b.errors), vec![1, 0, 0]);
        assert_eq!(timeline.peak(), (2, 1000));

        // Une entrée antérieure à la fenêtre est ignorée, sans panique.
        timeline.record(1, false);
        assert_eq!(timeline.peak().0, 2);
    }

    #[test]
    fn unites_de_duree_deduites() {
        let ms = |v, key| to_millis(&v, key, DurationUnit::Auto);
        assert_eq!(ms(json!(150), "duration_ms"), Some(150.0));
        // PHP mesure en secondes flottantes : 0.25 s = 250 ms.
        assert_eq!(ms(json!(0.25), "duration"), Some(250.0));
        assert_eq!(ms(json!("1.5s"), "elapsed"), Some(1500.0));
        assert_eq!(ms(json!(2000), "elapsed_us"), Some(2.0));
        // Un entier sans suffixe reste des millisecondes.
        assert_eq!(ms(json!(430), "duration"), Some(430.0));
        assert_eq!(ms(json!("bonjour"), "duration"), None);
    }

    #[test]
    fn le_champ_de_duree_prime_sur_la_correlation() {
        let mut stats = stats();
        for entry in requete("aaa", true) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert_eq!(stats.total, 3);
        let route = &stats.routes["app_home"];
        assert_eq!(route.requests, 1, "une seule requête comptée");
        assert_eq!(route.timed, 1, "pas de double mesure champ + corrélation");
        assert!((route.max_ms - 120.0).abs() < 0.01);
        assert!(matches!(stats.duration, DurationSource::Field { .. }));
    }

    #[test]
    fn sans_champ_de_duree_la_correlation_prend_le_relais() {
        let mut stats = stats();
        for entry in requete("bbb", false) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        let route = &stats.routes["app_home"];
        assert_eq!(route.requests, 1);
        assert_eq!(route.timed, 1, "durée déduite du token");
        // Première ligne à .000, dernière à .120 : 120 ms.
        assert!(
            (route.max_ms - 120.0).abs() < 1.0,
            "durée = {}",
            route.max_ms
        );
        assert!(matches!(stats.duration, DurationSource::Correlated { .. }));
    }

    #[test]
    fn une_erreur_sans_contexte_de_route_est_rattachee_par_son_token() {
        let mut stats = stats();
        let mut entries = requete("ccc", false);
        // La ligne d'exception ne porte que l'exception : pas de route.
        let erreur = parse_line(
            r#"[2026-09-09T10:00:00.100000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boom: "nope" at /var/www/src/X.php line 12 {"exception":"[object] (App\\Exception\\Boom(code: 0): nope at /var/www/src/X.php:12)"} {"token":"ccc"}"#,
        )
        .unwrap();
        entries.insert(2, erreur);
        for entry in entries {
            stats.ingest(0, entry);
        }

        assert_eq!(stats.errors_total(), 1);
        let (_, erreur) = stats.errors.iter().next().unwrap();
        assert_eq!(erreur.exception.as_deref(), Some(r"App\Exception\Boom"));
        assert_eq!(erreur.endpoint.as_deref(), Some("app_home"));
        assert_eq!(stats.routes["app_home"].errors, 1);
    }

    /// Une ligne Doctrine : requête préparée, paramètres à part.
    fn ligne_sql(token: &str, sql: &str) -> LogEntry {
        let ligne = format!(
            r#"[2026-09-09T10:00:00.060000+02:00] doctrine.DEBUG: Executing statement {{"sql":"{sql}","params":{{"1":1}}}} {{"token":"{token}"}}"#
        );
        parse_line(&ligne).expect("ligne SQL valide")
    }

    /// Insère `n` requêtes SQL au milieu d'une requête HTTP type.
    fn requete_avec_sql(token: &str, sql: impl Fn(usize) -> String, n: usize) -> Vec<LogEntry> {
        let mut entries = requete(token, true);
        for i in 0..n {
            entries.insert(2, ligne_sql(token, &sql(i)));
        }
        entries
    }

    #[test]
    fn detecte_un_n_plus_un() {
        let mut stats = stats();
        // La boucle fautive : douze fois exactement la même requête préparée.
        let sql = "SELECT t0.id, t0.name FROM product t0 WHERE t0.id = ?";
        for entry in requete_avec_sql("nnn", |_| sql.to_string(), 12) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert_eq!(stats.nplus1.len(), 1, "un seul motif attendu");
        let motif = stats.nplus1.values().next().unwrap();
        assert_eq!(motif.endpoint, "app_home");
        assert_eq!(motif.max_count, 12);
        assert_eq!(motif.requests, 1);
        assert!(motif.sql.contains("FROM product"));
        // Douze répétitions plus le « SELECT 1 » de la requête type.
        assert_eq!(stats.routes["app_home"].queries_max, 13);
    }

    #[test]
    fn des_requetes_variees_ne_declenchent_rien() {
        let mut stats = stats();
        for entry in requete_avec_sql("vvv", |i| format!("SELECT id FROM table_{i}"), 30) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert!(
            stats.nplus1.is_empty(),
            "trente requêtes distinctes ne forment pas un N+1"
        );
        assert_eq!(stats.routes["app_home"].queries_max, 31);
        assert_eq!(stats.sql_shapes(), 31);
    }

    #[test]
    fn le_motif_se_cumule_sur_plusieurs_requetes() {
        let mut stats = stats();
        let sql = "SELECT t0.id FROM address t0 WHERE t0.customer_id = ?";
        for (token, n) in [("a", 15), ("b", 40)] {
            for entry in requete_avec_sql(token, |_| sql.to_string(), n) {
                stats.ingest(0, entry);
            }
        }
        stats.finalize();

        let motif = stats.nplus1.values().next().expect("motif détecté");
        assert_eq!(motif.requests, 2, "vu sur deux requêtes HTTP");
        assert_eq!(motif.max_count, 40, "le pire cas est retenu");
        assert!((motif.avg_count() - 27.5).abs() < 0.01);
    }

    #[test]
    fn le_seuil_zero_desactive_la_detection() {
        let mut cli = Cli::parse_from(["ruru", "prod.log"]);
        cli.nplus1 = 0;
        let mut stats = Stats::new(&cli);

        for entry in requete_avec_sql("zzz", |_| "SELECT 42".to_string(), 50) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert!(stats.nplus1.is_empty());
        // Le comptage SQL, lui, continue.
        assert_eq!(stats.routes["app_home"].queries_max, 51);
    }

    /// Deux fichiers lus en parallèle n'avancent pas à la même vitesse dans le
    /// temps : `prod.log` est court, son lecteur file donc bien plus loin que
    /// celui de `doctrine.log`. Le balayage ne doit pas se caler sur le plus
    /// rapide, sinon il découpe en morceaux les requêtes du plus lent — et un
    /// N+1 réparti sur deux morceaux ne franchit plus jamais le seuil.
    #[test]
    fn l_avance_d_une_source_ne_decoupe_pas_les_requetes_d_une_autre() {
        let cli = Cli::parse_from(["ruru", "prod.log", "doctrine.log"]);
        let mut stats = Stats::new(&cli);
        let sql = "SELECT t0.id FROM address t0 WHERE t0.customer_id = ?";

        // prod.log (source 0) ouvre la requête.
        stats.ingest(0, requete("xyz", true).remove(0));
        // doctrine.log (source 1) en livre la moitié des requêtes SQL.
        for _ in 0..6 {
            stats.ingest(1, ligne_sql("xyz", sql));
        }
        // prod.log file cinq minutes plus loin : son lecteur a de l'avance.
        let plus_loin =
            parse_line(r#"[2026-09-09T10:05:00.000000+02:00] app.INFO: autre chose [] []"#)
                .expect("ligne valide");
        stats.ingest(0, plus_loin);
        // doctrine.log, resté en arrière, livre le reste de la même requête.
        for _ in 0..6 {
            stats.ingest(1, ligne_sql("xyz", sql));
        }
        stats.finalize();

        let motif = stats
            .nplus1
            .values()
            .next()
            .expect("les douze exécutions forment un seul N+1");
        assert_eq!(
            motif.max_count, 12,
            "une seule requête HTTP, douze fois la même requête SQL"
        );
        assert_eq!(motif.requests, 1);
        // Les douze exécutions comptées sur une seule requête HTTP, pas deux
        // moitiés de six.
        assert_eq!(stats.routes["app_home"].queries_max, 12);
    }

    #[test]
    fn la_sortie_json_expose_les_metriques_attendues() {
        let mut stats = stats();
        for entry in requete("aaa", true) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        let doc: Value =
            serde_json::from_str(&render_json(&stats, 25, false)).expect("JSON bien formé");

        assert_eq!(doc["totals"]["entries"], 3);
        assert_eq!(doc["totals"]["errors"], 0);
        assert_eq!(doc["levels"]["info"], 2);
        assert_eq!(doc["levels"]["debug"], 1);
        assert_eq!(doc["duration_source"]["kind"], "field");
        assert_eq!(doc["duration_source"]["key"], "duration_ms");

        let endpoint = &doc["endpoints"][0];
        assert_eq!(endpoint["endpoint"], "app_home");
        assert_eq!(endpoint["requests"], 1);
        assert_eq!(endpoint["p95_ms"], 120.0);
        assert_eq!(endpoint["max_ms"], 120.0);
    }

    #[test]
    fn le_plafond_top_limite_les_listes() {
        let mut stats = stats();
        for route in ["a", "b", "c"] {
            let ligne = format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "{route}". {{"route":"{route}","duration_ms":10}} []"#
            );
            stats.ingest(0, parse_line(&ligne).unwrap());
        }

        let combien = |top| {
            let doc: Value = serde_json::from_str(&render_json(&stats, top, false)).unwrap();
            doc["endpoints"].as_array().unwrap().len()
        };
        assert_eq!(combien(2), 2);
        assert_eq!(combien(0), 3, "0 signifie « tous »");
    }

    #[test]
    fn les_grands_nombres_sont_lisibles() {
        assert_eq!(format_count(1234567), "1 234 567");
        assert_eq!(format_count(42), "42");
        assert_eq!(format_ms(1500.0), "1.50 s");
        assert_eq!(format_ms(12.34), "12 ms");
    }
}
