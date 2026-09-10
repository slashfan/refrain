//! Suivi de fichiers façon `tail -f`, mais qui analyse au passage.
//!
//! Trois difficultés que `tail -f` gère et qu'on doit gérer aussi :
//!
//! - **La ligne incomplète.** On peut lire un fichier au moment exact où PHP
//!   écrit dedans. On détecte l'absence de `\n` final, on « rend » les octets
//!   lus, et on réessaiera au prochain tour.
//! - **La rotation.** `logrotate` renomme `prod.log` en `prod.log.1` et en crée
//!   un neuf. Le descripteur de fichier ouvert continue de pointer sur l'ancien.
//!   On compare donc périodiquement l'inode du chemin avec celui qu'on tient.
//! - **La troncature.** `> prod.log` remet la taille à zéro : si le fichier est
//!   plus court que notre position, c'est qu'il a été vidé, on repart de zéro.

use crate::event::Event;
use crate::parser::{self, LogEntry};
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

/// Au-delà, on envoie le lot : borne la mémoire sur un fichier de plusieurs Go.
const MAX_BATCH: usize = 4096;
/// Une stack trace peut faire des milliers de lignes ; on n'en garde que le début.
const MAX_MESSAGE: usize = 4000;
const READ_BUFFER: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Options {
    pub from_start: bool,
    pub lines: usize,
    pub follow: bool,
    pub poll: Duration,
}

/// Lance un thread de lecture par fichier. Tous écrivent dans le même canal.
pub fn spawn(source: usize, path: PathBuf, opts: Options, tx: Sender<Event>) {
    thread::spawn(move || {
        let result = if path.as_os_str() == "-" {
            run_stdin(source, &tx)
        } else {
            run_file(source, &path, &opts, &tx)
        };
        if let Err(err) = result {
            let _ = tx.send(Event::Failed(format!("{} : {err}", path.display())));
        }
        let _ = tx.send(Event::SourceDone(source));
    });
}

/// Assemble les lignes en entrées et les expédie par lots.
///
/// Son autre rôle : recoller les entrées multi-lignes. Une stack trace PHP
/// s'étale sur des dizaines de lignes qui ne commencent ni par `[` ni par `{` ;
/// le parseur renvoie `None` pour chacune, et on les rattache à l'entrée en cours.
struct Assembler<'a> {
    source: usize,
    tx: &'a Sender<Event>,
    pending: Option<LogEntry>,
    batch: Vec<LogEntry>,
    skipped: u64,
    last_flush: Instant,
}

impl<'a> Assembler<'a> {
    fn new(source: usize, tx: &'a Sender<Event>) -> Self {
        Self {
            source,
            tx,
            pending: None,
            batch: Vec::with_capacity(MAX_BATCH),
            skipped: 0,
            last_flush: Instant::now(),
        }
    }

    /// Renvoie `false` quand le récepteur a disparu : il faut arrêter le thread.
    fn feed(&mut self, line: &str) -> bool {
        match parser::parse_line(line) {
            Some(entry) => {
                self.close_pending();
                self.pending = Some(entry);
            }
            None => match self.pending.as_mut() {
                Some(open) if open.message.len() < MAX_MESSAGE => {
                    open.message.push('\n');
                    open.message.push_str(line.trim_end());
                }
                Some(_) => {}
                None if !line.trim().is_empty() => self.skipped += 1,
                None => {}
            },
        }

        if self.batch.len() >= MAX_BATCH {
            return self.flush();
        }
        // Sur un flux continu, on veut aussi que l'écran bouge : on expédie au
        // moins toutes les 100 ms. Le test de longueur évite d'appeler
        // `Instant::now()` à chaque ligne sur les gros fichiers.
        if self.batch.len().is_multiple_of(64)
            && !self.batch.is_empty()
            && self.last_flush.elapsed() > Duration::from_millis(100)
        {
            return self.flush();
        }
        true
    }

    /// Clôt l'entrée en cours de constitution et la verse au lot.
    fn close_pending(&mut self) {
        if let Some(mut entry) = self.pending.take() {
            parser::truncate_chars(&mut entry.message, MAX_MESSAGE);
            self.batch.push(entry);
        }
    }

    fn flush(&mut self) -> bool {
        self.last_flush = Instant::now();
        if !self.batch.is_empty() {
            // `std::mem::take` remplace le vecteur par un vide et nous rend
            // l'ancien : on transfère la propriété du lot sans le copier.
            let entries = std::mem::take(&mut self.batch);
            self.batch = Vec::with_capacity(MAX_BATCH);
            let batch = Event::Batch {
                source: self.source,
                entries,
            };
            if self.tx.send(batch).is_err() {
                return false;
            }
        }
        if self.skipped > 0 {
            let skipped = std::mem::take(&mut self.skipped);
            if self.tx.send(Event::Skipped(skipped)).is_err() {
                return false;
            }
        }
        true
    }
}

fn run_file(source: usize, path: &Path, opts: &Options, tx: &Sender<Event>) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut id = file_id(&file.metadata()?);

    let start = if opts.from_start {
        0
    } else if opts.lines > 0 {
        seek_back_lines(&mut file, opts.lines)?
    } else {
        file.seek(SeekFrom::End(0))?
    };
    file.seek(SeekFrom::Start(start))?;

    let mut reader = BufReader::with_capacity(READ_BUFFER, file);
    let mut pos = start;
    let mut asm = Assembler::new(source, tx);
    let mut line = String::new();

    loop {
        let mut read_any = false;

        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break; // fin des données disponibles
            }
            if !line.ends_with('\n') {
                // Écriture en cours : on repose ces octets et on repassera.
                reader.seek_relative(-(n as i64))?;
                break;
            }
            pos += n as u64;
            read_any = true;
            if !asm.feed(&line) {
                return Ok(());
            }
        }

        if !opts.follow {
            asm.close_pending();
            asm.flush();
            return Ok(());
        }

        if !read_any {
            // Plus rien à lire : l'entrée en attente est forcément complète.
            asm.close_pending();
        }
        if !asm.flush() {
            return Ok(());
        }

        if !read_any {
            // On tient la fin du fichier : cette source est désormais à l'heure
            // du mur, et ne doit plus retenir le balayage des autres.
            if tx.send(Event::CaughtUp(source)).is_err() {
                return Ok(());
            }
            thread::sleep(opts.poll);
            if let Ok(meta) = fs::metadata(path) {
                let rotated = file_id(&meta) != id;
                let truncated = meta.len() < pos;
                if rotated || truncated {
                    let file = File::open(path)?;
                    id = file_id(&file.metadata()?);
                    reader = BufReader::with_capacity(READ_BUFFER, file);
                    pos = 0;
                }
            }
        }
    }
}

/// Lit l'entrée standard jusqu'à sa fermeture : `ssh prod cat prod.log | refrain -`.
fn run_stdin(source: usize, tx: &Sender<Event>) -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = BufReader::with_capacity(READ_BUFFER, stdin.lock());
    let mut asm = Assembler::new(source, tx);
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if !asm.feed(&line) {
            return Ok(());
        }
    }
    asm.close_pending();
    asm.flush();
    Ok(())
}

/// Positionne le curseur au début des `n` dernières lignes, en remontant par
/// blocs depuis la fin — le fichier peut faire des gigaoctets, hors de question
/// de le lire en entier pour ça.
fn seek_back_lines(file: &mut File, n: usize) -> io::Result<u64> {
    let len = file.seek(SeekFrom::End(0))?;
    let mut pos = len;
    let mut newlines = 0usize;
    let mut buf = vec![0u8; 8192];

    while pos > 0 {
        let chunk = (buf.len() as u64).min(pos) as usize;
        pos -= chunk as u64;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut buf[..chunk])?;

        for i in (0..chunk).rev() {
            if buf[i] != b'\n' {
                continue;
            }
            newlines += 1;
            // Le `\n` final du fichier termine la dernière ligne : il faut donc
            // en trouver n+1 pour se placer au début de la n-ième avant la fin.
            if newlines > n {
                return Ok(pos + i as u64 + 1);
            }
        }
    }
    Ok(0)
}

/// Identité d'un fichier, indépendante de son nom. Deux chemins de même
/// (device, inode) désignent le même fichier ; un inode différent après une
/// rotation signale qu'il faut rouvrir.
#[cfg(unix)]
fn file_id(meta: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_id(_meta: &Metadata) -> (u64, u64) {
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::sync::mpsc;

    fn ligne(n: usize) -> String {
        format!("[2026-09-09T10:00:0{n}.000000+02:00] app.INFO: message {n} {{}} []\n")
    }

    /// Collecte les entrées reçues, en abandonnant au bout de `budget`.
    fn recolte(rx: &mpsc::Receiver<Event>, budget: Duration) -> Vec<LogEntry> {
        let mut out = Vec::new();
        let fin = Instant::now() + budget;
        while Instant::now() < fin {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Event::Batch { entries, .. }) => out.extend(entries),
                // La source annonce qu'elle tient la fin du fichier : ce qu'elle
                // avait à livrer est livré, inutile d'attendre le budget entier.
                Ok(Event::CaughtUp(_)) if !out.is_empty() => break,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) if !out.is_empty() => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        out
    }

    #[test]
    fn suit_les_ajouts_la_rotation_et_les_lignes_incompletes() {
        let dir = std::env::temp_dir().join(format!("refrain-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prod.log");
        std::fs::write(&path, format!("{}{}", ligne(1), ligne(2))).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn(
            0,
            path.clone(),
            Options {
                from_start: true,
                lines: 0,
                follow: true,
                poll: Duration::from_millis(10),
            },
            tx,
        );

        let recu = recolte(&rx, Duration::from_secs(3));
        assert_eq!(recu.len(), 2, "les deux lignes déjà présentes");

        // Ajout à chaud, comme le ferait PHP.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(ligne(3).as_bytes()).unwrap();
        file.flush().unwrap();
        assert_eq!(
            recolte(&rx, Duration::from_secs(3)).len(),
            1,
            "ligne ajoutée"
        );

        // Ligne en cours d'écriture : sans `\n` final, elle ne doit pas sortir.
        file.write_all(b"[2026-09-09T10:00:06.000000+02:00] app.INFO: incomplet")
            .unwrap();
        file.flush().unwrap();
        assert!(
            recolte(&rx, Duration::from_millis(400)).is_empty(),
            "une ligne sans fin de ligne doit attendre"
        );

        file.write_all(b" {} []\n").unwrap();
        file.flush().unwrap();
        let recu = recolte(&rx, Duration::from_secs(3));
        assert_eq!(recu.len(), 1, "une fois complète, elle est émise");
        assert_eq!(recu[0].message, "incomplet");

        // Rotation façon logrotate : le fichier est renommé, un neuf le remplace.
        drop(file);
        std::fs::rename(&path, dir.join("prod.log.1")).unwrap();
        std::fs::write(&path, ligne(7)).unwrap();
        let recu = recolte(&rx, Duration::from_secs(3));
        assert_eq!(recu.len(), 1, "le nouveau fichier est repris");
        assert_eq!(recu[0].message, "message 7");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn relit_les_dernieres_lignes_demandees() {
        let dir = std::env::temp_dir().join(format!("refrain-back-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prod.log");
        let contenu: String = (1..=9).map(ligne).collect();
        std::fs::write(&path, contenu).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn(
            0,
            path.clone(),
            Options {
                from_start: false,
                lines: 3,
                follow: false,
                poll: Duration::from_millis(10),
            },
            tx,
        );

        let recu = recolte(&rx, Duration::from_secs(3));
        assert_eq!(recu.len(), 3, "seulement les 3 dernières");
        assert_eq!(recu[0].message, "message 7");
        assert_eq!(recu[2].message, "message 9");

        std::fs::remove_dir_all(&dir).ok();
    }
}
