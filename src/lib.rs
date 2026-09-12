//! refrain — real-time Symfony/Monolog log analyser.
//!
//! Overall shape:
//!
//! ```text
//!   tail thread(s) ──┐
//!   keyboard thread ─┼──► mpsc channel ──► main loop ──► ratatui
//!   clock thread ────┘                    (App: decides)  (ui: draws)
//! ```
//!
//! A single thread touches the application state, which avoids every lock:
//! concurrency goes through the channel and nowhere else.
//!
//! The whole is a library rather than a single binary, for a precise reason:
//! the benchmark (`src/bin/bench.rs`) must be able to call `parse_line` and
//! `Stats::ingest` directly, to say which of the two costs what. A binary
//! cannot import anything from another binary.

pub mod app;
pub mod cli;
pub mod event;
pub mod export;
pub mod parser;
pub mod stats;
pub mod tail;
pub mod threshold;
pub mod ui;
