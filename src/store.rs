//! The files. Records are FIXED WIDTH, so a memory or a block is found by
//! seeking to its offset -- no scanning, no index file to keep in sync.
//! Position IS identity: memory i lives at i*LOG_REC of LOG.txt, and block
//! [k*s,(k+1)*s) lives at k*TREE_REC of TREE/<s>. Padding costs ~2x on disk
//! and buys O(1) everywhere. The format is byte-compatible with the original
//! Python tool, so an existing memory needs no migration.

use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::cover::Block;
use crate::{die, me, pretty, OrDie};

pub const LOG_REC: u64 = 320;
pub const TREE_REC: u64 = 288;
/// How long a command queues behind another session before it gives up.
const LOCK_WAIT: Duration = Duration::from_secs(30);

pub fn log_path(d: &Path) -> PathBuf {
    d.join("LOG.txt")
}

pub fn tree_path(d: &Path, size: u64) -> PathBuf {
    d.join("TREE").join(size.to_string())
}

pub fn pins_path(d: &Path) -> PathBuf {
    d.join("PINS.txt")
}

/// Records in a file; a missing file holds none. Any other failure is real.
pub fn count(p: &Path, rec: u64) -> u64 {
    match fs::metadata(p) {
        Ok(m) => m.len() / rec,
        Err(e) if e.kind() == ErrorKind::NotFound => 0,
        Err(e) => Err::<u64, _>(e).or_die(p),
    }
}

pub fn log_len(d: &Path) -> u64 {
    count(&log_path(d), LOG_REC)
}

/// Drop a partial trailing record left by a crash. It was never acknowledged.
/// Without this the next append lands at a wrong offset and every later
/// record is misaligned. Callers hold the lock.
fn repair(p: &Path, rec: u64) {
    let n = match fs::metadata(p) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == ErrorKind::NotFound => return,
        Err(e) => Err::<u64, _>(e).or_die(p),
    };
    if n % rec != 0 {
        let f = OpenOptions::new().write(true).open(p).or_die(p);
        f.set_len(n - n % rec).or_die(p);
    }
}

pub struct Entry {
    pub id: u64,
    pub date: String,
    pub text: String,
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "#{} {} {}", self.id, self.date, self.text)
    }
}

/// Records are sliced as BYTES and decoded one by one: slicing decoded text
/// would shift every boundary after the first multi-byte character.
fn parse(rec: &[u8], at: u64) -> Entry {
    let damaged = || -> ! {
        die(format!(
            "Memory #{at} is damaged: LOG.txt holds a record that is not a memory."
        ))
    };
    let line = std::str::from_utf8(rec)
        .unwrap_or_else(|_| damaged())
        .trim_end();
    let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
    let (date, text) = rest.split_once(' ').unwrap_or((rest, ""));
    let id = head
        .strip_prefix('#')
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| damaged());
    Entry {
        id,
        date: date.into(),
        text: text.into(),
    }
}

fn read_full(f: &mut File, buf: &mut [u8], p: &Path) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => Err::<(), _>(e).or_die(p),
        }
    }
    n
}

/// The log, opened once for a run of seeks.
pub struct Log {
    file: File,
    path: PathBuf,
}

impl Log {
    pub fn open(d: &Path) -> Log {
        let path = log_path(d);
        Log {
            file: File::open(&path).or_die(&path),
            path,
        }
    }

    /// Memories [lo,hi) in one read.
    pub fn slice(&mut self, lo: u64, hi: u64) -> Vec<Entry> {
        self.file
            .seek(SeekFrom::Start(lo * LOG_REC))
            .or_die(&self.path);
        let mut buf = vec![0u8; ((hi - lo) * LOG_REC) as usize];
        let n = read_full(&mut self.file, &mut buf, &self.path);
        buf[..n]
            .as_chunks::<{ LOG_REC as usize }>()
            .0
            .iter()
            .zip(lo..)
            .map(|(r, i)| parse(r, i))
            .collect()
    }

    pub fn get(&mut self, i: u64) -> Entry {
        self.slice(i, i + 1)
            .pop()
            .unwrap_or_else(|| die(format!("Memory #{i} does not exist.")))
    }

    /// "[2026-10-05..10-09]": the days block [lo,hi) spans, so an agent can
    /// place a summary in time.
    pub fn span(&mut self, lo: u64, hi: u64) -> String {
        let (a, b) = (self.get(lo).date, self.get(hi - 1).date);
        if a == b {
            format!("[{a}]")
        } else if a.len() == 10 && b.len() == 10 && a[..4] == b[..4] {
            format!("[{a}..{}]", &b[5..])
        } else {
            format!("[{a}..{b}]")
        }
    }
}

/// Every memory, streamed with its position. A search reads the whole log by
/// nature, but it must not HOLD it: at a million memories that is 300 MB.
pub fn log_scan(d: &Path, mut f: impl FnMut(u64, Entry)) {
    let p = log_path(d);
    let mut file = File::open(&p).or_die(&p);
    let mut buf = vec![0u8; (LOG_REC * 4096) as usize];
    let mut at = 0;
    loop {
        let n = read_full(&mut file, &mut buf, &p);
        for r in buf[..n].as_chunks::<{ LOG_REC as usize }>().0 {
            f(at, parse(r, at));
            at += 1;
        }
        if n < buf.len() {
            return;
        }
    }
}

/// The summary of block [lo,hi), in one seek. None if not built yet.
pub fn tree_get(d: &Path, lo: u64, hi: u64) -> Option<String> {
    let (size, p) = (hi - lo, tree_path(d, hi - lo));
    let mut f = match File::open(&p) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => Err::<File, _>(e).or_die(&p),
    };
    f.seek(SeekFrom::Start(lo / size * TREE_REC)).or_die(&p);
    let mut rec = vec![0u8; TREE_REC as usize];
    let n = read_full(&mut f, &mut rec, &p);
    match std::str::from_utf8(&rec[..n]) {
        Ok(s) if s.trim_end().is_empty() => None,
        Ok(s) => Some(s.trim_end().to_string()),
        Err(_) => die(format!(
            "The summary of #{lo}-{} is corrupt. Run: {} forget {lo}-{}",
            hi - 1,
            me(),
            hi - 1
        )),
    }
}

fn pad(text: &str, rec: u64) -> Vec<u8> {
    let mut b = text.as_bytes().to_vec();
    if b.len() as u64 > rec - 1 {
        die(format!(
            "Too long: {} bytes. The record holds {}.",
            b.len(),
            rec - 1
        ));
    }
    b.resize((rec - 1) as usize, b' ');
    b.push(b'\n');
    b
}

/// The store's one lock: held while a file changes. Released on drop, and by
/// the OS if the process dies. Parallel sessions queue behind it.
pub struct Lock(#[allow(dead_code)] File);

pub fn locked(d: &Path) -> Lock {
    let p = d.join(".lock");
    // Never truncate: the lock file is shared by every session.
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&p)
        .or_die(&p);
    let (start, mut nap) = (Instant::now(), Duration::from_millis(5));
    loop {
        match f.try_lock() {
            Ok(()) => return Lock(f),
            Err(TryLockError::WouldBlock) => {
                if start.elapsed() >= LOCK_WAIT {
                    die(format!(
                        "{} has been locked by another memo for {}s. If no other session is \
                         running memo, a stuck process holds it: end it, then retry.",
                        pretty(d),
                        LOCK_WAIT.as_secs()
                    ));
                }
                sleep(nap);
                nap = (nap * 2).min(Duration::from_millis(100));
            }
            Err(TryLockError::Error(e)) => Err::<(), _>(e).or_die(&p),
        }
    }
}

fn append_synced(p: &Path, bytes: &[u8]) {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .or_die(p);
    f.write_all(bytes).or_die(p);
    f.sync_all().or_die(p);
}

/// Append memories, items = [(date, text)]. The only way LOG.txt ever
/// changes. Ids are assigned INSIDE the lock: two sessions noting at the same
/// moment must not be handed the same id. Returns the first id used.
pub fn log_append(d: &Path, items: &[(String, String)]) -> u64 {
    let _lock = locked(d);
    let p = log_path(d);
    repair(&p, LOG_REC);
    let base = log_len(d);
    let mut buf = Vec::with_capacity(items.len() * LOG_REC as usize);
    for (k, (date, text)) in items.iter().enumerate() {
        buf.extend(pad(&format!("#{} {date} {text}", base + k as u64), LOG_REC));
    }
    append_synced(&p, &buf);
    base
}

/// Write block [lo,hi). Blocks are built in order, so this only ever appends
/// one record to one level file. False if another session got there first.
pub fn tree_put(d: &Path, (lo, hi): Block, text: &str) -> bool {
    let size = hi - lo;
    let _lock = locked(d);
    let p = tree_path(d, size);
    repair(&p, TREE_REC);
    if count(&p, TREE_REC) != lo / size {
        return false;
    }
    append_synced(&p, &pad(text, TREE_REC));
    true
}

/// Forget block [lo,hi) and every block built from it, by truncating each
/// level back to that point. Later blocks at those levels go too and are
/// rebuilt; the log is never touched, so nothing is lost.
pub fn tree_drop(d: &Path, (lo, hi): Block) -> Vec<Block> {
    let (mut gone, mut size) = (Vec::new(), hi - lo);
    let _lock = locked(d);
    while size <= log_len(d) {
        let (p, k) = (tree_path(d, size), lo / size);
        let n = count(&p, TREE_REC);
        if n > k {
            gone.extend((k..n).map(|i| (i * size, (i + 1) * size)));
            let f = OpenOptions::new().write(true).open(&p).or_die(&p);
            f.set_len(k * TREE_REC).or_die(&p);
        }
        size *= 2;
    }
    gone
}

pub fn read_pins(d: &Path) -> Vec<String> {
    let p = pins_path(d);
    match fs::read_to_string(&p) {
        Ok(s) => s
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => Err::<Vec<String>, _>(e).or_die(&p),
    }
}

/// Replace the pins whole: write a sibling, then rename over, so a crash
/// leaves the old list or the new one, never half of one. Callers hold the lock.
pub fn write_pins(d: &Path, pins: &[String]) {
    let (p, tmp) = (pins_path(d), d.join("PINS.txt.new"));
    let body: String = pins.iter().map(|l| format!("{l}\n")).collect();
    let write = || -> io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()
    };
    write().or_die(&tmp);
    fs::rename(&tmp, &p).or_die(&p);
}
