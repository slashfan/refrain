//! Following files the way `tail -f` does, but parsing on the way.
//!
//! Three difficulties `tail -f` handles and that we must handle too:
//!
//! - **The incomplete line.** A file can be read at the exact moment PHP is
//!   writing into it. We detect the missing trailing `\n`, "give back" the
//!   bytes read, and try again on the next round.
//! - **Rotation.** `logrotate` renames `prod.log` to `prod.log.1` and creates a
//!   new one. The open file descriptor keeps pointing at the old one. So we
//!   periodically compare the inode of the path with the one we hold.
//! - **Truncation.** `> prod.log` resets the size to zero: if the file is
//!   shorter than our position, it has been emptied, and we start over.
//!
//! And a case of its own: the **rotated log**, `prod.log.1.gz`. That is
//! precisely the one opened in a post-mortem. It is closed and complete:
//! nothing to follow, no rotation to watch for, but it must be decompressed on
//! the fly.
//!
//! Lines are read as **bytes** then converted without ever failing. A log is
//! not always valid UTF-8: a latin-1 byte from an old library, a binary blob in
//! an exception message, a character cut in two by a rotation. Reading into a
//! `String` would fail the read of **the whole file** over one faulty byte.

use crate::event::Event;
use crate::parser::{self, LogEntry};
use flate2::read::MultiGzDecoder;
use std::collections::VecDeque;
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

/// Past this, the batch is sent: bounds memory on a multi-gigabyte file.
const MAX_BATCH: usize = 4096;
/// A stack trace can run to thousands of lines; only its start is kept.
const MAX_MESSAGE: usize = 4000;
const READ_BUFFER: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Options {
    pub from_start: bool,
    pub lines: usize,
    pub follow: bool,
    pub poll: Duration,
}

/// Spawns one reading thread per file. All of them write to the same channel.
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

/// Assembles lines into entries and ships them in batches.
///
/// Its other role: gluing multi-line entries back together. A PHP stack trace
/// spreads over dozens of lines starting with neither `[` nor `{`; the parser
/// returns `None` for each, and we attach them to the entry in progress.
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

    /// Returns `false` once the receiver is gone: the thread must stop.
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
        // On a live stream we also want the screen to move: ship at least
        // every 100 ms. The length test avoids calling `Instant::now()` on
        // every line of a large file.
        if self.batch.len().is_multiple_of(64)
            && !self.batch.is_empty()
            && self.last_flush.elapsed() > Duration::from_millis(100)
        {
            return self.flush();
        }
        true
    }

    /// Closes the entry being built and pours it into the batch.
    fn close_pending(&mut self) {
        if let Some(mut entry) = self.pending.take() {
            parser::truncate_chars(&mut entry.message, MAX_MESSAGE);
            self.batch.push(entry);
        }
    }

    fn flush(&mut self) -> bool {
        self.last_flush = Instant::now();
        if !self.batch.is_empty() {
            // `std::mem::take` replaces the vector with an empty one and hands
            // back the old: ownership of the batch moves without a copy.
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
    if is_gzip(path)? {
        return run_gzip(source, path, opts, tx);
    }
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
    let mut line = Vec::new();

    loop {
        let mut read_any = false;

        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 {
                break; // no more data available
            }
            if !line.ends_with(b"\n") && opts.follow {
                // A write is in progress: put those bytes back and come again.
                // Only when following — in a one-shot report nothing more
                // will ever come, and a file cut by a crash or a rotation
                // ends exactly like this: its last line is complete as it is.
                reader.seek_relative(-(n as i64))?;
                break;
            }
            pos += n as u64;
            read_any = true;
            if !asm.feed(&decode(&line)) {
                return Ok(());
            }
        }

        if !opts.follow {
            asm.close_pending();
            asm.flush();
            return Ok(());
        }

        if !read_any {
            // Nothing left to read: the pending entry is necessarily complete.
            asm.close_pending();
        }
        if !asm.flush() {
            return Ok(());
        }

        if !read_any {
            // We hold the end of the file: this source is now on wall-clock
            // time, and must no longer hold back the others' sweep.
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

/// Reads standard input until it closes: `ssh prod cat prod.log | refrain -`.
fn run_stdin(source: usize, tx: &Sender<Event>) -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = BufReader::with_capacity(READ_BUFFER, stdin.lock());
    let mut asm = Assembler::new(source, tx);
    let mut line = Vec::new();

    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if !asm.feed(&decode(&line)) {
            return Ok(());
        }
    }
    asm.close_pending();
    asm.flush();
    Ok(())
}

/// From bytes to a line, without ever failing.
///
/// A log is not always valid UTF-8, and a faulty byte must cost only the
/// character it occupies: `read_line` would fail the read of the whole file,
/// and the batch already parsed would be lost with it. Invalid sequences become
/// "�"; `from_utf8_lossy` allocates nothing when the line is valid, which is
/// the general case.
fn decode(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

/// Places the cursor at the start of the last `n` lines, walking back in
/// blocks from the end — the file may be gigabytes, and reading it whole for
/// this is out of the question.
fn seek_back_lines(file: &mut File, n: usize) -> io::Result<u64> {
    let len = file.seek(SeekFrom::End(0))?;
    let mut pos = len;
    // A last line with no `\n` is a line all the same: its missing terminator
    // is counted as if it were there, otherwise `-n 1` would land one line
    // too early on a file cut by a crash.
    let mut newlines = usize::from(!ends_with_newline(file, len)?);
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
            // The file's trailing `\n` ends the last line: we must therefore
            // find n+1 of them to land at the start of the nth from the end.
            if newlines > n {
                return Ok(pos + i as u64 + 1);
            }
        }
    }
    Ok(0)
}

/// Does the file end with a line terminator? An empty file does, by
/// convention: it has no unterminated line.
fn ends_with_newline(file: &mut File, len: u64) -> io::Result<bool> {
    if len == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// A rotated log: closed, complete, compressed.
///
/// Nothing to follow — the file will not grow — no rotation to watch for, and
/// no position to seek: you cannot place yourself at the end of a compressed
/// stream without having decompressed it. So it is read whole, once.
///
/// `-n` is still honoured, and that is what sets this apart from a shortcut:
/// rather than ignoring the option silently, the last N lines are kept in a
/// ring buffer. Full decompression is unavoidable; memory stays bounded by N.
fn run_gzip(source: usize, path: &Path, opts: &Options, tx: &Sender<Event>) -> io::Result<()> {
    // `MultiGzDecoder` and not `GzDecoder`: `cat a.gz b.gz > c.gz` is a valid
    // gzip made of several members, and logrotate produces those.
    let decoder = MultiGzDecoder::new(File::open(path)?);
    let mut reader = BufReader::with_capacity(READ_BUFFER, decoder);
    let mut asm = Assembler::new(source, tx);
    let mut line = Vec::new();

    if opts.lines == 0 {
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if !asm.feed(&decode(&line)) {
                return Ok(());
            }
        }
    } else {
        let mut tail_lines: VecDeque<String> = VecDeque::with_capacity(opts.lines);
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if tail_lines.len() == opts.lines {
                tail_lines.pop_front();
            }
            tail_lines.push_back(decode(&line).into_owned());
        }
        for line in &tail_lines {
            if !asm.feed(line) {
                return Ok(());
            }
        }
    }

    asm.close_pending();
    asm.flush();
    Ok(())
}

/// Is this file compressed?
///
/// The header decides, not the extension: a gzipped `.log` is still a gzipped
/// file, and a `.gz` that is not one would be read wrong. Two bytes are
/// enough — `1f 8b`, the gzip signature (RFC 1952).
fn is_gzip(path: &Path) -> io::Result<bool> {
    let mut header = Vec::with_capacity(2);
    File::open(path)?.take(2).read_to_end(&mut header)?;
    Ok(header == [0x1f, 0x8b])
}

/// (device, inode) designate the same file; a different inode after a rotation
/// signals that it must be reopened.
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

    fn log_line(n: usize) -> String {
        format!("[2026-09-09T10:00:0{n}.000000+02:00] app.INFO: message {n} {{}} []\n")
    }

    /// Compresses with gzip, the way `logrotate` would.
    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn an_invalid_byte_costs_only_its_own_character() {
        let dir = std::env::temp_dir().join(format!("refrain-utf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prod.log");

        // A log is not always valid UTF-8: a latin-1 byte from an old
        // library, a binary blob inside an exception, a character cut in two by
        // a rotation. Reading into a `String` used to fail the read of the
        // whole file — and the batch already parsed was lost with it: three
        // valid lines returned zero entries and exit code 1.
        let mut content = Vec::new();
        content.extend(log_line(1).as_bytes());
        content.extend(b"[2026-09-09T10:00:02.000000+02:00] app.INFO: casse\xff\xfe {} []\n");
        content.extend(log_line(3).as_bytes());
        std::fs::write(&path, &content).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn(
            0,
            path,
            Options {
                from_start: true,
                lines: 0,
                follow: false,
                poll: Duration::from_millis(10),
            },
            tx.clone(),
        );
        drop(tx);

        let got = collect(&rx, Duration::from_secs(3));
        assert_eq!(got.len(), 3, "all three lines must arrive");
        assert!(
            got[1].message.contains('\u{fffd}'),
            "the faulty byte becomes the replacement character: {:?}",
            got[1].message
        );
        assert!(got[2].message.contains("message 3"), "the rest is read");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gzipped_rotated_log_is_read_whole() {
        let dir = std::env::temp_dir().join(format!("refrain-gz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let content: String = (1..=4).map(log_line).collect();
        let path = dir.join("prod.log.1.gz");
        std::fs::write(&path, gzip(content.as_bytes())).unwrap();

        // Following was asked for, but a closed file is not followed: it is
        // read whole and then the source ends by itself.
        let (tx, rx) = mpsc::channel();
        spawn(
            0,
            path.clone(),
            Options {
                from_start: false,
                lines: 0,
                follow: true,
                poll: Duration::from_millis(10),
            },
            tx,
        );
        assert_eq!(collect(&rx, Duration::from_secs(3)).len(), 4);

        // `-n` is honoured rather than silently ignored: full decompression
        // is unavoidable, memory stays bounded by N.
        let (tx, rx) = mpsc::channel();
        spawn(
            0,
            path.clone(),
            Options {
                from_start: false,
                lines: 2,
                follow: false,
                poll: Duration::from_millis(10),
            },
            tx,
        );
        let got = collect(&rx, Duration::from_secs(3));
        assert_eq!(got.len(), 2, "the last two lines");
        assert!(got[1].message.contains("message 4"), "{:?}", got[1].message);

        // `cat a.gz b.gz > c.gz` is a valid multi-member gzip, and that is
        // what a concatenating logrotate produces.
        let mut multi = gzip(log_line(1).as_bytes());
        multi.extend(gzip(log_line(2).as_bytes()));
        let path = dir.join("multi.log.gz");
        std::fs::write(&path, multi).unwrap();
        let (tx, rx) = mpsc::channel();
        spawn(
            0,
            path,
            Options {
                from_start: true,
                lines: 0,
                follow: false,
                poll: Duration::from_millis(10),
            },
            tx,
        );
        assert_eq!(
            collect(&rx, Duration::from_secs(3)).len(),
            2,
            "both members must be read"
        );

        // A plain file named ".gz" must not mislead: the header decides, not
        // the extension.
        let path = dir.join("liar.gz");
        std::fs::write(&path, log_line(9)).unwrap();
        assert!(!is_gzip(&path).unwrap());
        let (tx, rx) = mpsc::channel();
        spawn(
            0,
            path,
            Options {
                from_start: true,
                lines: 0,
                follow: false,
                poll: Duration::from_millis(10),
            },
            tx,
        );
        assert_eq!(collect(&rx, Duration::from_secs(3)).len(), 1);

        // And a file too short to carry a signature does not blow up.
        let path = dir.join("empty.log");
        std::fs::write(&path, b"x").unwrap();
        assert!(!is_gzip(&path).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Collects the entries received, giving up after `budget`.
    fn collect(rx: &mpsc::Receiver<Event>, budget: Duration) -> Vec<LogEntry> {
        let mut out = Vec::new();
        let tail_lines = Instant::now() + budget;
        while Instant::now() < tail_lines {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Event::Batch { entries, .. }) => out.extend(entries),
                // The source announces it holds the end of the file: what it
                // had to deliver is delivered, no need to wait out the budget.
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
    fn appends_rotation_and_incomplete_lines_are_followed() {
        let dir = std::env::temp_dir().join(format!("refrain-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prod.log");
        std::fs::write(&path, format!("{}{}", log_line(1), log_line(2))).unwrap();

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

        let got = collect(&rx, Duration::from_secs(3));
        assert_eq!(got.len(), 2, "the two lines already there");

        // A live append, the way PHP would.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(log_line(3).as_bytes()).unwrap();
        file.flush().unwrap();
        assert_eq!(
            collect(&rx, Duration::from_secs(3)).len(),
            1,
            "line appended"
        );

        // A line being written: without its trailing `\n`, it must not come out.
        file.write_all(b"[2026-09-09T10:00:06.000000+02:00] app.INFO: incomplet")
            .unwrap();
        file.flush().unwrap();
        assert!(
            collect(&rx, Duration::from_millis(400)).is_empty(),
            "a line with no line ending must wait"
        );

        file.write_all(b" {} []\n").unwrap();
        file.flush().unwrap();
        let got = collect(&rx, Duration::from_secs(3));
        assert_eq!(got.len(), 1, "once complete, it is emitted");
        assert_eq!(got[0].message, "incomplet");

        // logrotate-style rotation: the file is renamed, a new one replaces it.
        drop(file);
        std::fs::rename(&path, dir.join("prod.log.1")).unwrap();
        std::fs::write(&path, log_line(7)).unwrap();
        let got = collect(&rx, Duration::from_secs(3));
        assert_eq!(got.len(), 1, "the new file is picked up");
        assert_eq!(got[0].message, "message 7");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_last_line_without_newline_is_read_in_a_one_shot_report() {
        let dir = std::env::temp_dir().join(format!("refrain-nonl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prod.log");
        // A file cut by a crash or a rotation ends without `\n`. The gzip and
        // stdin paths already counted that line; the plain-file path put its
        // bytes back to wait for a newline that would never come, and the
        // report lost the last line — often the one that matters.
        let mut content = log_line(1);
        content.push_str(log_line(2).trim_end());
        std::fs::write(&path, &content).unwrap();

        let read = |lines: usize| {
            let (tx, rx) = mpsc::channel();
            spawn(
                0,
                path.clone(),
                Options {
                    from_start: lines == 0,
                    lines,
                    follow: false,
                    poll: Duration::from_millis(10),
                },
                tx,
            );
            collect(&rx, Duration::from_secs(3))
        };

        let got = read(0);
        assert_eq!(got.len(), 2, "both lines, the unterminated one included");
        assert_eq!(got[1].message, "message 2");

        // And `-n` counts that line as one: `-n 1` is the last line, not the
        // one before it.
        let got = read(1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].message, "message 2");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_last_lines_asked_for_are_reread() {
        let dir = std::env::temp_dir().join(format!("refrain-back-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prod.log");
        let content: String = (1..=9).map(log_line).collect();
        std::fs::write(&path, content).unwrap();

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

        let got = collect(&rx, Duration::from_secs(3));
        assert_eq!(got.len(), 3, "only the last 3");
        assert_eq!(got[0].message, "message 7");
        assert_eq!(got[2].message, "message 9");

        std::fs::remove_dir_all(&dir).ok();
    }
}
