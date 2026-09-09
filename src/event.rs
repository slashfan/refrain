//! Le canal unique par lequel arrive tout ce qui doit réveiller l'application.
//!
//! C'est le cœur de l'architecture : plutôt que d'aller *chercher* le clavier
//! puis les fichiers puis l'horloge à tour de rôle, plusieurs threads
//! **poussent** leurs événements dans un même `mpsc` (*multi-producer,
//! single-consumer*). La boucle principale n'a plus qu'à lire ce canal.
//!
//! C'est aussi ce qui rend l'outil rapide : la lecture et l'analyse des logs se
//! font sur un thread dédié pendant que le thread principal dessine l'écran.

use crate::parser::LogEntry;
use crossterm::event::KeyEvent;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub enum Event {
    /// Un lot d'entrées analysées, et l'indice de la source qui l'envoie. On
    /// envoie par paquets plutôt que ligne par ligne : la synchronisation
    /// coûterait bien plus cher que l'analyse.
    Batch {
        source: usize,
        entries: Vec<LogEntry>,
    },
    /// Nombre de lignes lues mais non reconnues comme du Monolog.
    Skipped(u64),
    /// Une source a rattrapé la fin de son fichier et attend la suite : elle
    /// est désormais à l'heure du mur, et non plus à celle de ses lignes.
    CaughtUp(usize),
    /// Une source de logs est arrivée à sa fin définitive.
    SourceDone(usize),
    /// Une touche a été pressée.
    Key(KeyEvent),
    /// Le terminal a changé de taille : il faut redessiner.
    Resize,
    /// Battement d'horloge : redessine et fait vieillir les fenêtres de stats.
    Tick,
    /// Une source a échoué (fichier illisible, disparu…).
    Failed(String),
}

/// Thread clavier : `read()` bloque jusqu'à la prochaine touche, ce qui rend
/// l'interface réactive sans consommer de CPU à interroger le terminal.
pub fn spawn_input(tx: Sender<Event>) {
    thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            let msg = match event {
                // `KeyEventKind` distingue appui, maintien et relâchement : sous
                // Windows on recevrait chaque touche deux fois sans ce filtre.
                crossterm::event::Event::Key(k)
                    if k.kind == crossterm::event::KeyEventKind::Press =>
                {
                    Event::Key(k)
                }
                crossterm::event::Event::Resize(_, _) => Event::Resize,
                _ => continue,
            };
            // `send` échoue quand le récepteur a été détruit : l'appli quitte,
            // donc ce thread n'a plus de raison de vivre.
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
}

/// Thread horloge : garantit un rafraîchissement régulier même quand ni le
/// clavier ni les logs ne bougent (l'axe du temps, lui, avance toujours).
pub fn spawn_ticker(tx: Sender<Event>, period: Duration) {
    thread::spawn(move || {
        // On dort d'abord : en sortie NDJSON, un battement immédiat produirait
        // un premier instantané vide, avant même d'avoir lu une ligne.
        thread::sleep(period);
        while tx.send(Event::Tick).is_ok() {
            thread::sleep(period);
        }
    });
}
