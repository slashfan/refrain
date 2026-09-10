//! Les seuils de `--fail-if` : leur écriture, et leur verdict.
//!
//! Un rapport en cron ou en CI ne sert à rien s'il faut le lire pour savoir que
//! ça va mal. Un seuil franchi doit faire échouer le job — franchement, avec un
//! code de sortie distinct de celui d'une source illisible.

use crate::stats::{Stats, format_count, format_ms};
use std::fmt;

/// Ce qu'on mesure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Part des entrées en erreur, entre 0 et 1.
    ErrorRate,
    /// Nombre d'entrées en erreur.
    Errors,
    /// Nombre d'entrées analysées.
    Entries,
    /// Quantiles de durée, en millisecondes.
    P50,
    P95,
    P99,
    Max,
}

impl Metric {
    fn parse(texte: &str) -> Option<Self> {
        Some(match texte {
            "error-rate" => Metric::ErrorRate,
            "errors" => Metric::Errors,
            "entries" => Metric::Entries,
            "p50" => Metric::P50,
            "p95" => Metric::P95,
            "p99" => Metric::P99,
            "max" => Metric::Max,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Metric::ErrorRate => "error-rate",
            Metric::Errors => "errors",
            Metric::Entries => "entries",
            Metric::P50 => "p50",
            Metric::P95 => "p95",
            Metric::P99 => "p99",
            Metric::Max => "max",
        }
    }

    /// Une durée se lit en millisecondes, un taux en pourcentage, un compte en
    /// entier : c'est ce qui décide de l'unité par défaut et de l'affichage.
    fn is_duration(self) -> bool {
        matches!(self, Metric::P50 | Metric::P95 | Metric::P99 | Metric::Max)
    }

    fn format(self, valeur: f64) -> String {
        if self.is_duration() {
            format_ms(valeur as f32)
        } else if self == Metric::ErrorRate {
            format!("{:.2} %", valeur * 100.0)
        } else {
            format_count(valeur as u64)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Gt,
    Ge,
    Lt,
    Le,
}

impl Comparison {
    fn holds(self, mesure: f64, seuil: f64) -> bool {
        match self {
            Comparison::Gt => mesure > seuil,
            Comparison::Ge => mesure >= seuil,
            Comparison::Lt => mesure < seuil,
            Comparison::Le => mesure <= seuil,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Comparison::Gt => ">",
            Comparison::Ge => ">=",
            Comparison::Lt => "<",
            Comparison::Le => "<=",
        }
    }
}

/// Un seuil : `p95:api_orders_list>1s`.
#[derive(Debug, Clone, PartialEq)]
pub struct Threshold {
    metric: Metric,
    /// L'endpoint visé. Absent, un quantile porte sur le **pire** endpoint :
    /// « aucune route ne doit dépasser une seconde au p95 » est ce qu'on veut
    /// dire en CI, et le message nommera la coupable.
    endpoint: Option<String>,
    comparison: Comparison,
    value: f64,
}

impl fmt::Display for Threshold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.metric.name())?;
        if let Some(endpoint) = &self.endpoint {
            write!(f, ":{endpoint}")?;
        }
        write!(
            f,
            "{}{}",
            self.comparison.symbol(),
            self.metric.format(self.value)
        )
    }
}

/// Ce qu'on écrit quand un seuil est franchi.
pub struct Breach {
    pub threshold: Threshold,
    pub measured: f64,
    /// L'endpoint effectivement fautif, quand le seuil n'en désignait aucun.
    pub culprit: Option<String>,
}

impl fmt::Display for Breach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metric = self.threshold.metric;
        write!(f, "{}", metric.name())?;
        if let Some(endpoint) = self.threshold.endpoint.as_ref().or(self.culprit.as_ref()) {
            write!(f, " ({endpoint})")?;
        }
        write!(
            f,
            " = {} {} {}",
            metric.format(self.measured),
            self.threshold.comparison.symbol(),
            metric.format(self.threshold.value)
        )
    }
}

impl Threshold {
    /// `error-rate>2%`, `p95:api_orders_list>1s`, `entries<100`.
    ///
    /// Analysé à l'ouverture du programme et non à la fin : un seuil mal écrit
    /// doit échouer tout de suite, pas après avoir lu quarante gigaoctets.
    pub fn parse(texte: &str) -> Result<Self, String> {
        let texte = texte.trim();
        // Les deux caractères d'abord : sinon « >= » serait coupé sur son « > ».
        let (index, comparison) = [
            (">=", Comparison::Ge),
            ("<=", Comparison::Le),
            (">", Comparison::Gt),
            ("<", Comparison::Lt),
        ]
        .iter()
        .find_map(|(motif, comparison)| texte.find(motif).map(|i| (i, (*comparison, motif.len()))))
        .ok_or_else(|| format!("« {texte} » ne contient aucun comparateur (>, >=, <, <=)"))?;
        let (comparison, largeur) = comparison;

        let gauche = &texte[..index];
        let droite = texte[index + largeur..].trim();

        let (nom, endpoint) = match gauche.trim().split_once(':') {
            Some((nom, endpoint)) => (nom.trim(), Some(endpoint.trim().to_string())),
            None => (gauche.trim(), None),
        };
        let metric = Metric::parse(nom).ok_or_else(|| {
            format!(
                "« {nom} » n'est pas une métrique connue \
                 (error-rate, errors, entries, p50, p95, p99, max)"
            )
        })?;
        if endpoint.is_some() && !metric.is_duration() {
            return Err(format!(
                "« {} » porte sur l'ensemble des entrées : elle ne se restreint pas à un endpoint",
                metric.name()
            ));
        }

        let value = parse_value(droite, metric)?;
        Ok(Threshold {
            metric,
            endpoint,
            comparison,
            value,
        })
    }

    /// Le seuil est-il franchi ? Rend de quoi l'écrire, ou `None`.
    pub fn check(&self, stats: &Stats, scratch: &mut Vec<f32>) -> Option<Breach> {
        let (measured, culprit) = self.measure(stats, scratch)?;
        self.comparison.holds(measured, self.value).then(|| Breach {
            threshold: self.clone(),
            measured,
            culprit,
        })
    }

    fn measure(&self, stats: &Stats, scratch: &mut Vec<f32>) -> Option<(f64, Option<String>)> {
        let simple = match self.metric {
            Metric::ErrorRate => Some(match stats.total {
                0 => 0.0,
                total => stats.errors_total() as f64 / total as f64,
            }),
            Metric::Errors => Some(stats.errors_total() as f64),
            Metric::Entries => Some(stats.total as f64),
            _ => None,
        };
        if let Some(valeur) = simple {
            return Some((valeur, None));
        }

        // `mut` : la fermeture emprunte `scratch` en écriture, réutilisé
        // d'une route à l'autre pour ne pas allouer un vecteur par quantile.
        let mut quantile = |route: &crate::stats::RouteStat| -> f64 {
            match self.metric {
                Metric::Max => route.max_ms as f64,
                Metric::P50 => route.quantiles(scratch).p50 as f64,
                Metric::P95 => route.quantiles(scratch).p95 as f64,
                Metric::P99 => route.quantiles(scratch).p99 as f64,
                _ => unreachable!("les métriques simples sont traitées plus haut"),
            }
        };

        if let Some(nom) = &self.endpoint {
            // Un endpoint nommé mais absent des logs : on ne peut rien dire, et
            // inventer un zéro ferait passer le seuil pour respecté.
            let route = stats.routes.get(nom)?;
            return Some((quantile(route), None));
        }

        // Sans endpoint : le pire de tous. On ne retient que les routes dont on
        // a mesuré au moins une durée, sinon leur zéro tirerait le maximum vers
        // le bas et masquerait la seule route lente.
        stats
            .routes
            .iter()
            .filter(|(_, route)| route.timed > 0)
            .map(|(nom, route)| (quantile(route), Some(nom.clone())))
            .max_by(|a, b| a.0.total_cmp(&b.0))
    }
}

/// `2%` → 0.02, `1s` → 1000 ms, `500ms` → 500, `100` → 100.
fn parse_value(texte: &str, metric: Metric) -> Result<f64, String> {
    let invalide = || format!("« {texte} » n'est pas une valeur valide");

    if let Some(nombre) = texte.strip_suffix('%') {
        let valeur: f64 = nombre.trim().parse().map_err(|_| invalide())?;
        if metric != Metric::ErrorRate {
            return Err(format!(
                "un pourcentage n'a pas de sens pour « {} »",
                metric.name()
            ));
        }
        return Ok(valeur / 100.0);
    }

    // L'ordre compte : « ms » avant « s », sinon « 500ms » se lirait « 500m ».
    for (suffixe, facteur) in [("ms", 1.0), ("s", 1000.0)] {
        if let Some(nombre) = texte.strip_suffix(suffixe) {
            if !metric.is_duration() {
                return Err(format!(
                    "une durée n'a pas de sens pour « {} »",
                    metric.name()
                ));
            }
            let valeur: f64 = nombre.trim().parse().map_err(|_| invalide())?;
            return Ok(valeur * facteur);
        }
    }

    // Sans unité : la milliseconde pour une durée, la valeur brute sinon. Un
    // taux s'écrit alors en fraction — « error-rate>0.02 » vaut « >2% ».
    texte.parse().map_err(|_| invalide())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::parser::parse_line;
    use clap::Parser;

    fn seuil(texte: &str) -> Threshold {
        Threshold::parse(texte).unwrap_or_else(|e| panic!("« {texte} » : {e}"))
    }

    /// Deux endpoints, l'un lent et fautif, l'autre rapide et sain.
    fn stats_de_test() -> Stats {
        let mut stats = Stats::new(&Cli::parse_from(["refrain", "prod.log"]));
        let lignes = [
            (r#"{"route":"lent","duration_ms":2000.0}"#, "INFO"),
            (r#"{"route":"lent","duration_ms":3000.0}"#, "INFO"),
            (r#"{"route":"rapide","duration_ms":10.0}"#, "INFO"),
            (r#"{"route":"rapide","duration_ms":20.0}"#, "INFO"),
            (r#"{"route":"lent"}"#, "CRITICAL"),
        ];
        for (contexte, niveau) in lignes {
            let ligne = format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.{niveau}: Fini {contexte} []"#
            );
            stats.ingest(0, parse_line(&ligne).expect("ligne valide"));
        }
        stats.finalize();
        stats
    }

    #[test]
    fn la_grammaire_accepte_ce_qu_elle_annonce() {
        // Les unités se ramènent toutes à la même échelle interne.
        assert_eq!(seuil("error-rate>2%"), seuil("error-rate>0.02"));
        assert_eq!(seuil("p95>1s"), seuil("p95>1000"));
        assert_eq!(seuil("p95>1s"), seuil("p95>1000ms"));

        // Les comparateurs de deux caractères ne doivent pas être coupés sur
        // leur premier : « >= » n'est pas « > » suivi de « =2% ».
        assert_eq!(seuil("errors>=10").comparison, Comparison::Ge);
        assert_eq!(seuil("entries<=10").comparison, Comparison::Le);
        assert_eq!(seuil("entries<100").comparison, Comparison::Lt);

        // Les espaces autour ne gênent pas : la valeur vient souvent d'un
        // fichier de configuration ou d'une variable d'environnement.
        assert_eq!(
            seuil(" p95 : app_home > 800 ms "),
            seuil("p95:app_home>800ms")
        );

        // Et le seuil se réécrit tel qu'on l'a compris.
        assert_eq!(
            seuil("p95:app_home>800ms").to_string(),
            "p95:app_home>800 ms"
        );
    }

    #[test]
    fn la_grammaire_refuse_ce_qui_n_a_pas_de_sens() {
        assert!(
            Threshold::parse("p95 est trop grand").is_err(),
            "pas de comparateur"
        );
        assert!(
            Threshold::parse("tps_reponse>1s").is_err(),
            "métrique inconnue"
        );
        assert!(
            Threshold::parse("p95>vite").is_err(),
            "valeur non numérique"
        );
        // Un pourcentage de millisecondes, une durée d'entrées : non.
        assert!(Threshold::parse("p95>2%").is_err());
        assert!(Threshold::parse("entries>2s").is_err());
        // Un taux global ne se restreint pas à un endpoint.
        assert!(Threshold::parse("error-rate:app_home>2%").is_err());
    }

    #[test]
    fn les_seuils_globaux_se_mesurent_sur_l_ensemble() {
        let stats = stats_de_test();
        let mut scratch = Vec::new();

        // Cinq entrées, une seule en erreur : 20 %.
        assert!(
            seuil("error-rate>10%")
                .check(&stats, &mut scratch)
                .is_some()
        );
        assert!(
            seuil("error-rate>50%")
                .check(&stats, &mut scratch)
                .is_none()
        );
        assert!(seuil("errors>=1").check(&stats, &mut scratch).is_some());
        assert!(seuil("entries<3").check(&stats, &mut scratch).is_none());

        let breach = seuil("error-rate>10%").check(&stats, &mut scratch).unwrap();
        assert_eq!(breach.to_string(), "error-rate = 20.00 % > 10.00 %");
    }

    #[test]
    fn sans_endpoint_un_quantile_vise_le_pire() {
        let stats = stats_de_test();
        let mut scratch = Vec::new();

        // « lent » culmine à 3 s, « rapide » à 20 ms : c'est le pire qui
        // décide, et le message doit le nommer.
        let breach = seuil("max>1s")
            .check(&stats, &mut scratch)
            .expect("franchi");
        assert_eq!(breach.culprit.as_deref(), Some("lent"));
        assert!(breach.to_string().contains("max (lent)"), "{breach}");

        assert!(seuil("max>10s").check(&stats, &mut scratch).is_none());

        // Nommer l'endpoint sain rend le seuil respecté, alors que le pire le
        // franchissait : c'est bien la route demandée qui est mesurée.
        assert!(seuil("max:rapide>1s").check(&stats, &mut scratch).is_none());
        assert!(seuil("max:lent>1s").check(&stats, &mut scratch).is_some());
    }

    #[test]
    fn un_endpoint_absent_ne_declare_rien() {
        let stats = stats_de_test();
        let mut scratch = Vec::new();
        // Inventer un zéro ferait passer le seuil pour respecté, ce qui est un
        // mensonge : on ne se prononce pas.
        assert!(
            seuil("p95:jamais_vu>1ms")
                .check(&stats, &mut scratch)
                .is_none()
        );
        assert!(
            seuil("p95:jamais_vu<1ms")
                .check(&stats, &mut scratch)
                .is_none()
        );
    }
}
