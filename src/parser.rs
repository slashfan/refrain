//! Transformation d'une ligne brute en une structure exploitable : [`LogEntry`].
//!
//! Monolog écrit deux formats très répandus, qu'on gère tous les deux :
//!
//! 1. **Le format « ligne »** (`LineFormatter`, celui par défaut de Symfony) :
//!    `[2026-09-09T10:23:45.123456+02:00] request.CRITICAL: Uncaught PHP Exception … {"exception":"…"} []`
//! 2. **Le format JSON** (`JsonFormatter`) : une ligne = un objet JSON.
//!
//! La détection se fait ligne par ligne plutôt que via une option : c'est plus
//! robuste, et ça permet de suivre plusieurs fichiers de formats différents.

use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone};
use serde_json::Value;

/// Les 8 niveaux de gravité de Monolog (norme PSR-3), du moins au plus grave.
///
/// Dériver `PartialOrd`/`Ord` sur un enum utilise l'**ordre de déclaration** :
/// `Level::Debug < Level::Error` est donc vrai gratuitement, ce qui rend les
/// filtres du genre `entry.level >= min_level` triviaux à écrire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, clap::ValueEnum)]
pub enum Level {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

impl Level {
    pub const ALL: [Level; 8] = [
        Level::Debug,
        Level::Info,
        Level::Notice,
        Level::Warning,
        Level::Error,
        Level::Critical,
        Level::Alert,
        Level::Emergency,
    ];

    /// Depuis le nom textuel du format ligne (`request.CRITICAL:` → `CRITICAL`).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "DEBUG" => Self::Debug,
            "INFO" => Self::Info,
            "NOTICE" => Self::Notice,
            "WARNING" => Self::Warning,
            "ERROR" => Self::Error,
            "CRITICAL" => Self::Critical,
            "ALERT" => Self::Alert,
            "EMERGENCY" => Self::Emergency,
            _ => return None,
        })
    }

    /// Depuis le code numérique PSR-3 du `JsonFormatter` (100 = DEBUG … 600 = EMERGENCY).
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            100 => Self::Debug,
            200 => Self::Info,
            250 => Self::Notice,
            300 => Self::Warning,
            400 => Self::Error,
            500 => Self::Critical,
            550 => Self::Alert,
            600 => Self::Emergency,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Notice => "NOTICE",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
            Self::Alert => "ALERT",
            Self::Emergency => "EMERGENCY",
        }
    }

    /// Nom en minuscules, tel qu'il apparaît dans la sortie JSON.
    pub fn lower(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Notice => "notice",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
            Self::Alert => "alert",
            Self::Emergency => "emergency",
        }
    }

    /// Version courte, pour tenir dans une colonne de tableau.
    pub fn short(self) -> &'static str {
        match self {
            Self::Debug => "DEBG",
            Self::Info => "INFO",
            Self::Notice => "NOTC",
            Self::Warning => "WARN",
            Self::Error => "ERRO",
            Self::Critical => "CRIT",
            Self::Alert => "ALRT",
            Self::Emergency => "EMER",
        }
    }

    /// `self as usize` : un enum sans données est représenté par son rang.
    /// Pratique pour indexer un `[u64; 8]` de compteurs.
    pub fn index(self) -> usize {
        self as usize
    }

    /// ERROR et au-dessus : ce qu'on compte comme « erreur » dans les stats.
    pub fn is_error(self) -> bool {
        self >= Level::Error
    }
}

/// Une entrée de log analysée.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts: Option<DateTime<FixedOffset>>,
    pub level: Level,
    pub channel: String,
    pub message: String,
    pub context: Option<Value>,
    pub extra: Option<Value>,
}

impl LogEntry {
    /// Cherche une clé d'abord dans `context`, puis dans `extra`.
    ///
    /// La durée de vie `'a` dit au compilateur : « la `Value` renvoyée vit aussi
    /// longtemps que l'entrée ». C'est ce qui permet de renvoyer une référence
    /// vers l'intérieur de `self` sans rien copier.
    pub fn lookup<'a>(&'a self, key: &str) -> Option<&'a Value> {
        self.context
            .as_ref()
            .and_then(|c| c.get(key))
            .or_else(|| self.extra.as_ref().and_then(|e| e.get(key)))
            .filter(|v| !v.is_null())
    }

    /// Nom de route Symfony, s'il est présent.
    ///
    /// Symfony le loggue via le canal `request` (« Matched route "app_x". »)
    /// avec `context.route`, et le duplique dans `context.route_parameters._route`.
    pub fn route(&self) -> Option<&str> {
        if let Some(v) = self.lookup("route").and_then(Value::as_str) {
            return Some(v);
        }
        self.context
            .as_ref()?
            .get("route_parameters")?
            .get("_route")?
            .as_str()
    }

    pub fn request_uri(&self) -> Option<&str> {
        self.lookup("request_uri")
            .or_else(|| self.lookup("uri"))
            .or_else(|| self.lookup("url"))
            .and_then(Value::as_str)
    }

    pub fn method(&self) -> Option<&str> {
        self.lookup("method").and_then(Value::as_str)
    }

    /// Code HTTP de la réponse, s'il est journalisé.
    ///
    /// Monolog n'en écrit aucun de lui-même : c'est le souscripteur de
    /// `kernel.terminate` qui le pose dans le contexte (voir le README). La clé
    /// varie d'une application à l'autre, et certains formatteurs rendent le
    /// code en chaîne — on accepte les deux plutôt que d'ajouter une option.
    pub fn status(&self) -> Option<u16> {
        let value = self
            .lookup("status")
            .or_else(|| self.lookup("status_code"))
            .or_else(|| self.lookup("http_status"))
            .or_else(|| self.lookup("response_code"))?;
        let code = match value {
            Value::Number(n) => n.as_i64()?,
            Value::String(s) => s.trim().parse().ok()?,
            _ => return None,
        };
        // Hors de la plage HTTP, ce n'est pas un statut : un « status » qui
        // vaut 0 ou 9999 vient d'un autre champ du même nom.
        (100..600).contains(&code).then_some(code as u16)
    }

    /// Le libellé sous lequel on regroupe les requêtes : la route si elle existe,
    /// sinon l'URI nettoyée de sa query string.
    pub fn endpoint(&self) -> Option<String> {
        if let Some(r) = self.route() {
            return Some(r.to_string());
        }
        let uri = self.request_uri()?;
        let path = uri.split('?').next().unwrap_or(uri);
        Some(path.to_string())
    }

    /// Classe d'exception, extraite du `context.exception` ou, à défaut, du message.
    ///
    /// Monolog sérialise l'exception de deux façons selon le formatter :
    /// - chaîne : `[object] (App\Exception\Foo(code: 0): msg at /src/X.php:88)`
    /// - objet  : `{"class":"App\\Exception\\Foo","message":"…"}`
    pub fn exception_class(&self) -> Option<&str> {
        let exc = self.lookup("exception")?;

        if let Some(class) = exc.get("class").and_then(Value::as_str) {
            return Some(class);
        }
        if let Some(s) = exc.as_str() {
            return class_from_object_string(s);
        }
        None
    }

    /// Une clé stable pour regrouper « la même erreur » vue N fois.
    ///
    /// On normalise le message (chiffres et chaînes entre guillemets remplacés)
    /// pour que « Product 42 not found » et « Product 1337 not found » comptent
    /// comme une seule et même erreur.
    pub fn signature(&self) -> String {
        let mut sig = String::with_capacity(96);
        if let Some(class) = self.exception_class() {
            sig.push_str(short_class(class));
            sig.push_str(": ");
        }
        // Uniquement la première ligne : la stack trace rattachée ferait deux
        // groupes distincts de la même erreur selon qu'elle est présente ou non.
        let head = self.message.lines().next().unwrap_or(&self.message);
        normalize_into(head, &mut sig);
        truncate_chars(&mut sig, 160);
        sig
    }
}

/// `App\Exception\ProductNotFound` → `ProductNotFound`
pub fn short_class(class: &str) -> &str {
    class.rsplit('\\').next().unwrap_or(class)
}

/// Extrait `App\Exception\Foo` de `[object] (App\Exception\Foo(code: 0): …)`.
fn class_from_object_string(s: &str) -> Option<&str> {
    let after_paren = &s[s.find('(')? + 1..];
    // La classe s'arrête à la parenthèse de `(code: 0)`, ou au `:` s'il n'y en a pas.
    let end = after_paren
        .find('(')
        .or_else(|| after_paren.find(':'))
        .unwrap_or(after_paren.len());
    let class = after_paren[..end].trim();
    (!class.is_empty()).then_some(class)
}

/// Écrit `src` dans `dst` en gommant tout ce qui varie d'une occurrence à l'autre.
fn normalize_into(src: &str, dst: &mut String) {
    let mut chars = src.chars().peekable();
    let mut last_was_digit = false;

    while let Some(c) = chars.next() {
        match c {
            // Une chaîne entre guillemets est presque toujours une valeur variable
            // (un id, un nom de fichier, une route) : on la remplace en bloc.
            '"' => {
                dst.push_str("\"…\"");
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                }
                last_was_digit = false;
            }
            // Un groupe de chiffres devient un seul `#`.
            '0'..='9' => {
                if !last_was_digit {
                    dst.push('#');
                    last_was_digit = true;
                }
            }
            _ => {
                dst.push(c);
                last_was_digit = false;
            }
        }
    }
}

/// Tronque en respectant les frontières de caractères (un `String` Rust est de
/// l'UTF-8 : couper à un index d'octet arbitraire ferait paniquer le programme).
pub fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() > max {
        let cut = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        s.truncate(cut);
        s.push('…');
    }
}

/// Point d'entrée : une ligne brute → une entrée, ou `None` si la ligne n'est
/// pas un début d'entrée (ligne vide, ou continuation d'une stack trace).
pub fn parse_line(line: &str) -> Option<LogEntry> {
    let line = line.trim_end_matches(['\n', '\r']);
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') {
        parse_json(trimmed)
    } else if trimmed.starts_with('[') {
        parse_text(trimmed)
    } else {
        None
    }
}

/// Format `JsonFormatter` : un objet JSON par ligne.
fn parse_json(line: &str) -> Option<LogEntry> {
    let mut v: Value = serde_json::from_str(line).ok()?;

    // Le niveau peut arriver sous deux formes : `level_name` ("ERROR") ou `level` (400).
    let level = v
        .get("level_name")
        .and_then(Value::as_str)
        .and_then(Level::from_name)
        .or_else(|| {
            v.get("level")
                .and_then(Value::as_i64)
                .and_then(Level::from_code)
        })?;

    let channel = v
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or("app")
        .to_string();

    let message = v
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let ts = v.get("datetime").and_then(|d| match d {
        Value::String(s) => parse_ts(s),
        // Certaines versions sérialisent l'objet DateTime de PHP en entier.
        Value::Object(_) => d.get("date").and_then(Value::as_str).and_then(parse_ts),
        _ => None,
    });

    // `take()` évite de cloner l'arbre JSON : on le déplace hors de `v`,
    // qui de toute façon meurt à la fin de la fonction.
    let context = v.get_mut("context").map(Value::take).filter(is_useful);
    let extra = v.get_mut("extra").map(Value::take).filter(is_useful);

    Some(LogEntry {
        ts,
        level,
        channel,
        message,
        context,
        extra,
    })
}

/// Format `LineFormatter` : `[date] canal.NIVEAU: message {context} {extra}`
fn parse_text(line: &str) -> Option<LogEntry> {
    let close = line.find(']')?;
    let ts = parse_ts(&line[1..close]);

    let rest = line[close + 1..].trim_start();

    // `canal.NIVEAU:` — le premier `:` termine l'en-tête. Le message qui suit
    // peut contenir des `:` en pagaille, d'où le fait de ne chercher que le premier.
    let colon = rest.find(':')?;
    let (channel, level) = rest[..colon].rsplit_once('.')?;
    let level = Level::from_name(level.trim())?;

    let body = rest[colon + 1..].trim_start();
    let (message, context, extra) = split_message_and_json(body);

    Some(LogEntry {
        ts,
        level,
        channel: channel.trim().to_string(),
        message: message.to_string(),
        context: context.filter(is_useful),
        extra: extra.filter(is_useful),
    })
}

/// Sépare `message {context} {extra}` en ses trois morceaux.
///
/// La difficulté : le message lui-même peut contenir des accolades. On cherche
/// donc le point de coupure **le plus à gauche** tel que tout ce qui suit soit
/// une suite de valeurs JSON valides consommant la fin de la ligne.
fn split_message_and_json(body: &str) -> (&str, Option<Value>, Option<Value>) {
    for (i, w) in body.as_bytes().windows(2).enumerate() {
        // On ne teste que les positions plausibles : un espace suivi de `{` ou `[`.
        if w[0] != b' ' || (w[1] != b'{' && w[1] != b'[') {
            continue;
        }
        // `i` pointe sur un espace ASCII : découper là est sûr en UTF-8.
        if let Some((context, extra)) = parse_trailing_values(&body[i + 1..]) {
            return (body[..i].trim_end(), context, extra);
        }
    }
    (body.trim_end(), None, None)
}

/// Vrai si `tail` est exactement 1 ou 2 valeurs JSON, et rien d'autre.
fn parse_trailing_values(tail: &str) -> Option<(Option<Value>, Option<Value>)> {
    let mut stream = serde_json::Deserializer::from_str(tail).into_iter::<Value>();
    let mut values = Vec::with_capacity(2);

    for value in stream.by_ref() {
        values.push(value.ok()?);
        if values.len() > 2 {
            return None;
        }
    }
    // Toute la fin de ligne doit avoir été consommée, sinon c'est que le `{`
    // trouvé faisait partie du message et pas du contexte.
    if values.is_empty() || !tail[stream.byte_offset()..].trim().is_empty() {
        return None;
    }

    let mut it = values.into_iter();
    Some((it.next(), it.next()))
}

/// Monolog écrit `[]` / `{}` quand context ou extra sont vides : autant les oublier.
fn is_useful(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Object(m) => !m.is_empty(),
        Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// Accepte les deux dates qu'on croise en pratique dans les logs Symfony :
/// RFC 3339 (`2026-09-09T10:23:45.123456+02:00`) et l'ancien format sans zone.
fn parse_ts(s: &str) -> Option<DateTime<FixedOffset>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt);
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            // Pas de fuseau dans la ligne : on suppose celui de la machine.
            if let Some(dt) = Local.from_local_datetime(&naive).single() {
                return Some(dt.fixed_offset());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ligne_symfony_avec_contexte() {
        let line = r#"[2026-09-09T10:23:45.123456+02:00] request.INFO: Matched route "app_product_show". {"route":"app_product_show","route_parameters":{"_route":"app_product_show","id":"42"},"request_uri":"https://ex.test/product/42","method":"GET"} []"#;
        let e = parse_line(line).expect("la ligne doit être reconnue");

        assert_eq!(e.level, Level::Info);
        assert_eq!(e.channel, "request");
        assert_eq!(e.message, r#"Matched route "app_product_show"."#);
        assert_eq!(e.route(), Some("app_product_show"));
        assert_eq!(e.method(), Some("GET"));
        assert!(e.ts.is_some());
        // `[]` en fin de ligne est un extra vide : on ne le garde pas.
        assert!(e.extra.is_none());
    }

    #[test]
    fn le_statut_se_lit_sous_ses_noms_usuels() {
        let avec = |contexte: &str| {
            let ligne = format!(
                r#"[2026-09-09T10:23:45+02:00] request.INFO: Request finished {contexte} []"#
            );
            parse_line(&ligne).expect("ligne valide").status()
        };

        assert_eq!(avec(r#"{"status":500}"#), Some(500));
        assert_eq!(avec(r#"{"status_code":404}"#), Some(404));
        assert_eq!(avec(r#"{"http_status":201}"#), Some(201));
        assert_eq!(avec(r#"{"response_code":302}"#), Some(302));
        // Certains formatteurs rendent le code en chaîne.
        assert_eq!(avec(r#"{"status":"200"}"#), Some(200));

        // Hors de la plage HTTP, c'est un autre champ qui porte le même nom :
        // un statut applicatif, un drapeau, un code d'erreur maison.
        assert_eq!(avec(r#"{"status":0}"#), None);
        assert_eq!(avec(r#"{"status":9999}"#), None);
        assert_eq!(avec(r#"{"status":"ok"}"#), None);
        assert_eq!(avec("{}"), None);
    }

    #[test]
    fn parse_exception_et_signature() {
        let line = r#"[2026-09-09T10:23:46+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\ProductNotFound: "Product 42 not found" at /var/www/src/X.php line 88 {"exception":"[object] (App\\Exception\\ProductNotFound(code: 0): Product 42 not found at /var/www/src/X.php:88)"} []"#;
        let e = parse_line(line).unwrap();

        assert_eq!(e.level, Level::Critical);
        assert_eq!(e.exception_class(), Some(r"App\Exception\ProductNotFound"));

        // Deux ids différents doivent produire la même signature.
        let other = line.replace("42", "1337");
        let e2 = parse_line(&other).unwrap();
        assert_eq!(e.signature(), e2.signature());
        assert!(e.signature().starts_with("ProductNotFound: "));
    }

    #[test]
    fn message_contenant_des_accolades() {
        // Le `{` du message ne doit pas être pris pour le début du contexte.
        let line = r#"[2026-09-09T10:23:45+02:00] app.WARNING: Template {name} is deprecated {"name":"old.twig"} []"#;
        let e = parse_line(line).unwrap();
        assert_eq!(e.message, "Template {name} is deprecated");
        assert_eq!(e.lookup("name").and_then(|v| v.as_str()), Some("old.twig"));
    }

    #[test]
    fn parse_json_formatter() {
        let line = r#"{"message":"Boom","context":{"duration_ms":123.5},"level":500,"level_name":"CRITICAL","channel":"app","datetime":"2026-09-09T10:23:45.000000+02:00","extra":{}}"#;
        let e = parse_line(line).unwrap();

        assert_eq!(e.level, Level::Critical);
        assert_eq!(e.channel, "app");
        assert_eq!(
            e.lookup("duration_ms").and_then(|v| v.as_f64()),
            Some(123.5)
        );
        assert!(e.extra.is_none());
    }

    #[test]
    fn ligne_de_continuation_ignoree() {
        // Une ligne de stack trace ne commence ni par `[` ni par `{`.
        assert!(parse_line("  #0 /var/www/src/X.php(88): App\\Foo->bar()").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn ordre_des_niveaux() {
        assert!(Level::Debug < Level::Error);
        assert!(Level::Critical.is_error());
        assert!(!Level::Warning.is_error());
    }
}

#[cfg(test)]
mod robustesse {
    use super::*;

    /// Un générateur pseudo-aléatoire minuscule et déterministe : la même
    /// graine rejoue exactement les mêmes lignes tordues, sans dépendance et
    /// sans test qui clignote.
    struct Xorshift(u64);

    impl Xorshift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const MODELES: [&str; 6] = [
        r#"[2026-09-09T10:23:45.123456+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boom: "nope" at /var/www/src/X.php line 12 {"exception":"[object] (App\Exception\Boom(code: 0): nope)","route":"app_home"} {"token":"aaa"}"#,
        r#"{"message":"Matched route","context":{"route":"app_home","duration_ms":12.5},"level":200,"channel":"request","datetime":"2026-09-09T10:23:45.123456+02:00"}"#,
        r#"[2026-09-09T10:23:45.123456+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT t0.id FROM produit t0 WHERE t0.id = ?","params":{"1":42}} []"#,
        "#0 /var/www/src/Controller/ProductController.php(88): App\\Repository->find(42)",
        "",
        "{",
    ];

    /// Le parseur avale du texte qu'on ne maîtrise pas : lignes tronquées par
    /// une rotation, JSON coupé au milieu, accolades déséquilibrées. Il peut
    /// rendre `None` — la ligne sera comptée comme ignorée — mais il ne doit
    /// pas faire tomber un tableau de bord qui tourne depuis trois jours.
    ///
    /// On n'exerce pas l'UTF-8 invalide ici : `tail.rs` le convertit avant,
    /// et le parseur ne voit jamais que des `&str` valides.
    #[test]
    fn aucune_ligne_tordue_ne_fait_paniquer_le_parseur() {
        // 1. Toutes les troncatures possibles, à chaque frontière de caractère.
        for modele in MODELES {
            for (index, _) in modele.char_indices() {
                exercer(&modele[..index]);
            }
            exercer(modele);
        }

        // 2. Vingt mille mutations à graine fixe, avec les caractères qui font
        //    justement la structure d'une ligne Monolog.
        const POISON: [char; 14] = [
            '"', '{', '}', '[', ']', '\\', ':', ',', '\0', '\n', '\t', 'é', '日', '🙂',
        ];
        let mut rng = Xorshift(0x5eed_1234_abcd);
        for _ in 0..20_000 {
            let modele = MODELES[rng.below(MODELES.len())];
            let mut chars: Vec<char> = modele.chars().collect();
            if chars.is_empty() {
                continue;
            }
            for _ in 0..1 + rng.below(3) {
                let position = rng.below(chars.len());
                chars[position] = POISON[rng.below(POISON.len())];
            }
            exercer(&chars.into_iter().collect::<String>());
        }

        // 3. Les absurdités qu'on écrirait à la main.
        for texte in [
            "[",
            "]",
            "{}",
            "[]",
            "[2026",
            "{\"",
            "{\"a\":",
            "[] {} []",
            "[2026-09-09T10:23:45.123456+02:00]",
            "[2026-09-09T10:23:45+02:00] a.B:",
            "[9999999999999-99-99T99:99:99.999999+99:99] a.INFO: x {} []",
            "{\"level\":999999999999999999999}",
            "{\"datetime\":[]}",
        ] {
            exercer(texte);
        }
    }

    /// Analyse la ligne et, si elle donne une entrée, exerce tout ce qu'on en
    /// tire ensuite : c'est là que se cachent les découpages de chaînes.
    fn exercer(ligne: &str) {
        let Some(entry) = parse_line(ligne) else {
            return;
        };
        let _ = entry.signature();
        let _ = entry.endpoint();
        let _ = entry.exception_class();
        let _ = entry.route();
        let _ = entry.request_uri();
        let _ = entry.method();
        let mut copie = entry.message.clone();
        truncate_chars(&mut copie, 7);
        assert!(copie.chars().count() <= 8, "la troncature reste bornée");
    }

    #[test]
    fn la_troncature_respecte_les_caracteres_multi_octets() {
        // Couper à l'octet ferait paniquer au milieu d'un caractère ; le point
        // de suspension ajouté compte pour un caractère de plus.
        for texte in ["ééééééééé", "日本語日本語日本語", "🙂🙂🙂🙂🙂", "abc"]
        {
            for limite in 0..12 {
                let mut copie = texte.to_string();
                truncate_chars(&mut copie, limite);
                assert!(copie.chars().count() <= limite + 1, "{texte} à {limite}");
            }
        }
    }
}
