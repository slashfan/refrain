//! refrain — analyseur de logs Symfony/Monolog en temps réel.
//!
//! Architecture générale :
//!
//! ```text
//!   thread(s) tail ─┐
//!   thread clavier ─┼──► canal mpsc ──► boucle principale ──► ratatui
//!   thread horloge ─┘                    (App: décide)        (ui: dessine)
//! ```
//!
//! Un seul thread touche à l'état de l'application, ce qui évite tout verrou :
//! la concurrence passe uniquement par le canal.
//!
//! Le tout est une bibliothèque, et non un unique binaire, pour une raison
//! précise : le banc de mesure (`src/bin/bench.rs`) doit pouvoir appeler
//! `parse_line` et `Stats::ingest` directement, afin de dire lequel des deux
//! coûte quoi. Un binaire ne peut rien importer d'un autre binaire.

pub mod app;
pub mod cli;
pub mod event;
pub mod export;
pub mod parser;
pub mod stats;
pub mod tail;
pub mod threshold;
pub mod ui;
