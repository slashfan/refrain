//! The single channel through which everything that must wake the application
//! arrives.
//!
//! This is the heart of the architecture: rather than going to *fetch* the
//! keyboard, then the files, then the clock in turn, several threads **push**
//! their events into one `mpsc` (*multi-producer, single-consumer*). The main
//! loop only has to read that channel.
//!
//! It is also what makes the tool fast: reading and parsing the logs happen on
//! a dedicated thread while the main thread draws the screen.

use crate::parser::LogEntry;
use crossterm::event::KeyEvent;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub enum Event {
    /// A batch of parsed entries, and the index of the source sending it. We
    /// send in batches rather than line by line: the synchronisation would
    /// cost far more than the parsing.
    Batch {
        source: usize,
        entries: Vec<LogEntry>,
    },
    /// Number of lines read but not recognised as Monolog.
    Skipped(u64),
    /// A source has caught up with the end of its file and awaits the rest: it
    /// is now on wall-clock time, no longer on the time of its lines.
    CaughtUp(usize),
    /// A log source has reached its definitive end.
    SourceDone(usize),
    /// A key was pressed.
    Key(KeyEvent),
    /// The terminal was resized: it must be redrawn.
    Resize,
    /// Clock tick: redraw, and age the statistics windows.
    Tick,
    /// A source failed (unreadable file, vanished file…).
    Failed(String),
}

/// Keyboard thread: `read()` blocks until the next key, which keeps the
/// interface responsive without spending CPU polling the terminal.
pub fn spawn_input(tx: Sender<Event>) {
    thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            let msg = match event {
                // `KeyEventKind` tells press, repeat and release apart: on
                // Windows we would receive every key twice without this filter.
                crossterm::event::Event::Key(k)
                    if k.kind == crossterm::event::KeyEventKind::Press =>
                {
                    Event::Key(k)
                }
                crossterm::event::Event::Resize(_, _) => Event::Resize,
                _ => continue,
            };
            // `send` fails once the receiver has been dropped: the application
            // is quitting, so this thread has no reason left to live.
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
}

/// Clock thread: guarantees a regular refresh even when neither the keyboard
/// nor the logs move (the time axis, itself, always advances).
pub fn spawn_ticker(tx: Sender<Event>, period: Duration) {
    thread::spawn(move || {
        // Sleep first: in NDJSON output, an immediate tick would produce an
        // empty first snapshot, before a single line had been read.
        thread::sleep(period);
        while tx.send(Event::Tick).is_ok() {
            thread::sleep(period);
        }
    });
}
