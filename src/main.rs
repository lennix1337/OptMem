//! OptMem: a permanent, append-only memory for AI agents.

mod cover;
mod store;

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::OnceLock;

use regex::Regex;

use cover::{cover, Block};
use store::*;

const USAGE: &str = "\
OptMem: a permanent, append-only memory for AI agents.

  {memo} init               create this memory; print the setup block.
  {memo} wake [part [T]]    read your memory. Run first, every session.
  {memo} note \"...\"         record one memory: one short line. `-` reads stdin.
  {memo} amend <id> \"...\"   correct memory #id; wake and recall show the fix.
  {memo} nap [id \"...\"]...  do the pending compressions.
  {memo} recall <regex>...  search every memory; all regexes must match.
                            --around N also prints N neighbours of each hit.
  {memo} zoom <lo>-<hi>     open a tree node: its two halves.
  {memo} forget <lo>-<hi>   drop a bad summary; nap rebuilds it.
  {memo} pin [\"...\"]        list the pinned facts, or pin one: wake always shows them.
  {memo} unpin <n>          drop pinned fact n.
  {memo} config [NAME=N]    show this memory's sizes, or change one.
  {memo} import <file>      bulk-load dated memories (bootstrap only).

The memories live in ~/.optmem/memory, or in $MEMORY_DIR if set.
See github.com/lennix1337/OptMem.";

/// The sizes a memory may override in its own `config` file: the default,
/// and what it means. A memory that overrides nothing follows the tool.
const KNOBS: [(&str, usize, &str); 4] = [
    (
        "WAKE_LINES",
        96,
        "the memory context: how many lines wake prints",
    ),
    (
        "ENTRY_CHARS",
        280,
        "the longest one memory may be, in bytes",
    ),
    // Every harness truncates a command that prints too much, and each drops
    // a different piece: Claude Code cuts the middle at 30,000 chars, pi cuts
    // the head at 50 KB, Codex budgets 10,000 tokens. So the memory is handed
    // over in parts that fit all of them. These are transport limits.
    ("PART_CHARS", 20000, "output paging: largest part, in bytes"),
    ("PART_LINES", 500, "output paging: largest part, in lines"),
];

/// Blocks up to this many memories compress from the raw log.
const RAW_MAX: u64 = 16;
/// Wake refuses (and hands over the work) only while the missing summaries
/// are cheap to pay. Past this, as after a bulk import, it wakes with gaps.
const REFUSE_MAX: u64 = 16;
/// Compressions handed over at once, when they are independent.
const NAP_BATCH: usize = 4;
/// How far back `note` looks for a memory it would repeat.
const DUP_WINDOW: u64 = 200;
const PIN_MAX: usize = 16;

// ---------------------------------------------------------------- output

/// Print a line, ignoring a closed pipe (`memo recall x | head`).
macro_rules! say {
    () => {{ let _ = writeln!(io::stdout()); }};
    ($($t:tt)*) => {{ let _ = writeln!(io::stdout(), $($t)*); }};
}

pub fn die(msg: impl AsRef<str>) -> ! {
    let _ = io::stdout().flush();
    eprintln!("{}", msg.as_ref());
    process::exit(1)
}

/// The filesystem is the one thing the tool does not control. Report it the
/// way every other failure is reported, with the path that failed.
pub trait OrDie<T> {
    fn or_die(self, path: &Path) -> T;
}

impl<T> OrDie<T> for io::Result<T> {
    fn or_die(self, path: &Path) -> T {
        self.unwrap_or_else(|e| {
            let msg = e.to_string();
            let msg = msg
                .split(" (os error")
                .next()
                .unwrap_or(&msg)
                .trim_end_matches('.');
            die(format!("{}: {msg}.", pretty(path)))
        })
    }
}

fn plural(n: impl Into<u64>, word: &str) -> String {
    let n = n.into();
    if n == 1 {
        return format!("1 {word}");
    }
    let w = if let Some(stem) = word.strip_suffix('y') {
        format!("{stem}ie")
    } else if word.ends_with(['s', 'h', 'x']) {
        format!("{word}e")
    } else {
        word.to_string()
    };
    format!("{n} {w}s")
}

// ---------------------------------------------------------------- paths

fn home() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(env::home_dir)
        .unwrap_or_default()
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with(['/', '\\']) => home().join(&rest[1..]),
        _ => PathBuf::from(p),
    }
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

fn slashes(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// A path as the user would type it: $HOME folded to ~, forward slashes.
pub fn pretty(p: &Path) -> String {
    let p = absolute(p);
    match p.strip_prefix(home()) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", slashes(rest)),
        _ => slashes(&p),
    }
}

fn memory_dir() -> PathBuf {
    match env::var("MEMORY_DIR") {
        Ok(d) if !d.is_empty() => expand(&d),
        _ => home().join(".optmem").join("memory"),
    }
}

/// How an agent's shell runs this tool. Every command the tool prints has to
/// RUN, in bash, cmd and PowerShell alike. A bare `memo` does only when the
/// first `memo` on PATH is this one; otherwise name the binary in full, with
/// forward slashes, which every one of those shells accepts.
fn invocation(exe: &Path, path_var: &OsStr, windows: bool) -> String {
    let same = |a: &Path, b: &Path| match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    let names: &[&str] = if windows {
        &["memo", "memo.exe", "memo.cmd", "memo.bat", "memo.com"]
    } else {
        &["memo"]
    };
    if exe.file_stem() == Some(OsStr::new("memo")) {
        let here = exe.parent().unwrap_or(Path::new(""));
        for dir in env::split_paths(path_var).filter(|d| !d.as_os_str().is_empty()) {
            let dir = absolute(&expand(&dir.to_string_lossy()));
            let found: Vec<&&str> = names.iter().filter(|n| dir.join(n).is_file()).collect();
            if found.is_empty() {
                continue;
            }
            // bash would run an extensionless `memo` beside memo.exe instead
            let ours = same(&dir, here) && (!windows || !found.contains(&&"memo"));
            if ours {
                return "memo".into();
            }
            break;
        }
    }
    let full = absolute(exe);
    if !windows {
        return pretty(&full);
    }
    let s = full.to_string_lossy().replace('\\', "/");
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s
    }
}

static ME: OnceLock<String> = OnceLock::new();

pub fn me() -> &'static str {
    ME.get_or_init(|| {
        let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("memo"));
        invocation(
            &exe,
            &env::var_os("PATH").unwrap_or_default(),
            cfg!(windows),
        )
    })
}

// ---------------------------------------------------------------- config

#[derive(Clone, Copy)]
struct Sizes {
    wake_lines: u64,
    entry_chars: usize,
    part_chars: usize,
    part_lines: usize,
}

type Over = HashMap<&'static str, usize>;

fn knob(name: &str) -> Option<&'static str> {
    KNOBS.iter().map(|k| k.0).find(|k| *k == name)
}

/// Validate one knob, wherever it came from: the config file or argv. A bad
/// knob stops every command, so the message says where it is written.
fn size(k: &str, v: &str, at: &str) -> usize {
    let n = v
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1 && v.bytes().all(|b| b.is_ascii_digit()));
    let n = n.unwrap_or_else(|| {
        die(format!(
            "{at}{k} must be a positive whole number, not '{v}'."
        ))
    });
    let top = (TREE_REC - 8).min(LOG_REC - 40) as usize;
    if k == "ENTRY_CHARS" && n > top {
        die(format!(
            "{at}ENTRY_CHARS is at most {top}: a memory has to fit the fixed-width records."
        ));
    }
    n
}

/// The knobs this memory sets for itself, read from its `config` file.
fn overrides(d: &Path) -> Over {
    let p = d.join("config");
    let mut out = Over::new();
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return out,
        Err(e) => Err::<String, _>(e).or_die(&p),
    };
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim().to_uppercase(), v.trim());
        let at = format!("{} line {}: ", pretty(&p), n + 1);
        let Some(k) = knob(&k) else {
            let names: Vec<_> = KNOBS.iter().map(|k| k.0).collect();
            die(format!(
                "{at}{k} is not a size. Delete the line, or name one of: {}.",
                names.join(", ")
            ));
        };
        out.insert(k, size(k, v, &at));
    }
    out
}

fn sizes(over: &Over) -> Sizes {
    let get = |k: &str| {
        over.get(k)
            .copied()
            .unwrap_or_else(|| KNOBS.iter().find(|x| x.0 == k).unwrap().1)
    };
    Sizes {
        wake_lines: get("WAKE_LINES") as u64,
        entry_chars: get("ENTRY_CHARS"),
        part_chars: get("PART_CHARS"),
        part_lines: get("PART_LINES"),
    }
}

/// Every knob on its own line, commented out unless this memory overrides
/// it. It names no path: a path written here goes stale when the tool moves.
fn config_text(over: &Over) -> String {
    let mut out = String::from(
        "# OptMem sizes for this memory. A commented line means: follow the\n\
         # tool's default. Edit with `memo config NAME=VALUE`.\n\n",
    );
    for (k, default, what) in KNOBS {
        let lead = if over.contains_key(k) { "  " } else { "# " };
        let v = over.get(k).copied().unwrap_or(default);
        out += &format!("{lead}{k:<12} = {v:<6} # {what}\n");
    }
    out
}

fn write_config(d: &Path, over: &Over) {
    let p = d.join("config");
    fs::write(&p, config_text(over)).or_die(&p);
}

// ---------------------------------------------------------------- the store

struct Ctx {
    d: PathBuf,
    s: Sizes,
}

/// Open the memory. The directory is only ever created by `memo init`:
/// creating it IS creating the identity. If any other command created it, a
/// typo in MEMORY_DIR would silently open an empty store, and the agent
/// would wake with no past and write a second identity.
fn store() -> PathBuf {
    let d = memory_dir();
    if !d.is_dir() {
        die(format!(
            "No memory at {}.\nTo create one, run: {} init\nTo use an existing one, point MEMORY_DIR at it.",
            d.display(),
            me()
        ));
    }
    let tree = d.join("TREE");
    fs::create_dir_all(&tree).or_die(&tree);
    let log = log_path(&d);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .or_die(&log);
    d
}

/// Parse `<lo>-<hi>` as wake and the nap prompts print it: inclusive at both
/// ends, and a real block -- an aligned power-of-two range. Without the shape
/// check, `4-5` and `5-6` read the same record.
fn block_id(s: &str) -> Block {
    let parsed = s.split_once('-').and_then(|(a, b)| {
        let digits = |x: &str| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit());
        (digits(a) && digits(b)).then(|| (a.parse::<u64>().ok(), b.parse::<u64>().ok()))
    });
    let Some((Some(lo), Some(hi))) = parsed else {
        die(format!("'{s}' is not a block id. Copy it from the prompt."))
    };
    let hi = hi + 1;
    let n = hi.saturating_sub(lo);
    if hi <= lo || n < 2 || !n.is_power_of_two() || lo % n != 0 {
        die(format!(
            "{s} is not a block. Copy the id printed by wake, like 16-31."
        ));
    }
    (lo, hi)
}

fn check(c: &Ctx, text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        die("Empty. A memory is one line of text.");
    }
    if text.contains(['\n', '\r']) {
        let n = text.matches('\n').count() + 1;
        die(format!(
            "{n} lines. A memory is one line: merge them, or note them separately."
        ));
    }
    let n = text.len();
    if n > c.s.entry_chars {
        die(format!(
            "Too long: {n} bytes, limit {}. Accented characters cost 2 bytes. Compress it further.",
            c.s.entry_chars
        ));
    }
    text.to_string()
}

/// The text an argument stands for: itself, or stdin when it is `-`. Stdin
/// carries any character verbatim; a shell argument loses `"` in PowerShell
/// 5.1 and `%` and `^` in cmd.
fn text_arg(a: &str) -> String {
    if a != "-" {
        return a.to_string();
    }
    let mut buf = Vec::new();
    io::stdin()
        .read_to_end(&mut buf)
        .unwrap_or_else(|e| die(format!("stdin: {e}.")));
    String::from_utf8(buf).unwrap_or_else(|_| die("stdin is not UTF-8 text."))
}

fn today() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// The memory an amendment corrects: `amends #12: ...` -> 12.
fn amends(text: &str) -> Option<u64> {
    let rest = text.strip_prefix("amends #")?;
    let (id, _) = rest.split_once(':')?;
    id.parse().ok()
}

fn amended_note(id: u64, by: &HashMap<u64, u64>) -> String {
    by.get(&id)
        .map(|k| format!(" (amended by #{k})"))
        .unwrap_or_default()
}

/// Words, lowercased, with punctuation dropped: what makes two notes the
/// same note, whatever their spacing or casing.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// Two notes that share nearly all their words say nearly the same thing.
fn similar(a: &[String], b: &[String]) -> bool {
    if a.len().min(b.len()) < 5 {
        return false;
    }
    let (a, b): (HashSet<_>, HashSet<_>) = (a.iter().collect(), b.iter().collect());
    let shared = a.intersection(&b).count();
    shared * 5 >= (a.len() + b.len() - shared) * 4
}

// ---------------------------------------------------------------- naps

/// Blocks that can be built and have not been, smallest first. Each level
/// file holds a dense prefix, so its length says exactly how far that level
/// got: one stat per level, never a scan.
fn pending(d: &Path, t: u64, limit: usize) -> Vec<Block> {
    let (mut todo, mut size) = (Vec::new(), 2);
    while size <= t {
        for k in count(&tree_path(d, size), TREE_REC)..t / size {
            todo.push((k * size, (k + 1) * size));
            if todo.len() >= limit {
                return todo;
            }
        }
        size *= 2;
    }
    todo
}

/// How many blocks pending() would list, without listing them. A level can
/// hold MORE blocks than t needs -- t is a snapshot, and memories keep
/// arriving while an agent reads -- so each level is clamped at zero.
fn pending_count(d: &Path, t: u64) -> u64 {
    let (mut n, mut size) = (0, 2);
    while size <= t {
        n += (t / size).saturating_sub(count(&tree_path(d, size), TREE_REC));
        size *= 2;
    }
    n
}

/// What a block compresses: its raw memories, or its two halves' summaries.
fn nap_body(c: &Ctx, log: &mut Log, (lo, hi): Block) -> String {
    if hi - lo <= RAW_MAX {
        return log
            .slice(lo, hi)
            .iter()
            .map(|e| format!("  {e}"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mid = (lo + hi) / 2;
    let mut halves = Vec::new();
    for (a, b) in [(lo, mid), (mid, hi)] {
        // pending() lists a block only after its halves settled, so a missing
        // half is a blank record -- a corrupt write. Drop it; nap rebuilds it.
        let Some(s) = tree_get(&c.d, a, b) else {
            die(format!(
                "The summary of #{a}-{} is blank. Run: {} forget {a}-{}",
                b - 1,
                me(),
                b - 1
            ))
        };
        halves.push(format!("  #{a}-{} {} {s}", b - 1, log.span(a, b)));
    }
    halves.join("\n")
}

/// The next compressions, as one prompt. Blocks of the same level are
/// independent (their inputs all exist), so several go out at once, as long
/// as the prompt stays well inside one output part.
fn next_nap(c: &Ctx, t: u64) -> Option<String> {
    let todo = pending(&c.d, t, NAP_BATCH);
    let first = *todo.first()?;
    let mut log = Log::open(&c.d);
    let mut batch: Vec<(Block, String)> = Vec::new();
    let mut bytes = 0;
    for b in todo.into_iter().filter(|b| b.1 - b.0 == first.1 - first.0) {
        let body = nap_body(c, &mut log, b);
        if !batch.is_empty() && bytes + body.len() > c.s.part_chars / 2 {
            break;
        }
        bytes += body.len();
        batch.push((b, body));
    }
    let left = pending_count(&c.d, t) - batch.len() as u64;
    let rule = "Keep what has lasting effect, drop what does not. Invent nothing.";
    let id = |(lo, hi): Block| format!("{lo}-{}", hi - 1);
    if let [(b, body)] = batch.as_slice() {
        let tail = match left {
            0 => String::new(),
            1 => "\n1 compression remains after this one.".into(),
            n => format!("\n{n} compressions remain after this one."),
        };
        return Some(format!(
            "Compress memories #{} into one line of at most {} bytes.\n{rule}\n\n{body}\n{tail}\nRun: {} nap {} \"<your line>\"",
            id(*b),
            c.s.entry_chars,
            me(),
            id(*b)
        ));
    }
    let blocks: Vec<String> = batch
        .iter()
        .map(|(b, body)| format!("Block #{}:\n{body}", id(*b)))
        .collect();
    let tail = match left {
        0 => String::new(),
        1 => "\n1 compression remains after these.".into(),
        n => format!("\n{n} compressions remain after these."),
    };
    let args: Vec<String> = batch
        .iter()
        .map(|(b, _)| format!("{} \"<line for {}>\"", id(*b), id(*b)))
        .collect();
    Some(format!(
        "Compress each of these {} blocks into one line of at most {} bytes.\n{rule}\n\n{}\n{tail}\nRun: {} nap {}",
        batch.len(),
        c.s.entry_chars,
        blocks.join("\n\n"),
        me(),
        args.join(" ")
    ))
}

fn print_nap(c: &Ctx, t: u64, gap: bool) {
    if let Some(nap) = next_nap(c, t) {
        if gap {
            say!();
        }
        say!("{nap}");
    }
}

// ---------------------------------------------------------------- commands

const TEMPLATE: &str = "\
## Memory

Your memory is OptMem:
- The tool is `{memo}`
- Your memories are in `{data}`

OptMem outlives every session, compaction, model and vendor change.
Without it you do not know who you are, or what was decided and tried.

### At startup: activating OptMem (mandatory)

Run `{memo} wake` before any other tool call, in every session, and
then do exactly what it prints, to the end of its output.

### While working: register memories (mandatory)

Call `{memo} note \"<1 line, max {chars} bytes>\"` whenever you learn
something new, or something worth keeping happens. That covers a task
worth real effort, a fact or insight the user teaches you, anything you
learn about their life (even indirectly), any event of lasting effect.
A note with quotes, `%` or `^` in it goes through stdin: `{memo} note -`.

Do not register redundant memories. If an old memory is wrong, correct it
with `{memo} amend <id> \"...\"`. Facts that must never fade go in
`{memo} pin \"...\"`.

If `{memo} note` asks a compression: do it before your next action.

Never edit or delete anything under `{data}`: the tool manages it.

### When you need an old memory: search, or navigate

`{memo} recall <regex>` searches every memory, word for word.

Your memories also form a binary tree: #0-1, #2-3 ... exist as one-line
summaries, pairs of those as #0-3, and so on -- every `#a-b` line wake
prints is one node of it. `{memo} zoom <a-b>` opens a node into its
two halves, down to the raw memories.

### If you're a subagent: skip everything above

Parallel sessions on this machine are all you, and may all write memories.
A subagent is not: it must never run `memo`, because it cannot judge what
is already known, and its notes would arrive duplicated and incorrectly.
When you spawn one, write: `You are a subagent. Don't run memo.`
";

/// The one command that may create the memory directory, and the whole
/// setup. Re-running it is safe: it only ever creates what is missing, and
/// never rewrites what is there.
fn cmd_init(d: &Path, args: &[String]) {
    if !args.is_empty() {
        die(format!("usage: {} init", me()));
    }
    let fresh = !d.is_dir();
    let tree = d.join("TREE");
    fs::create_dir_all(&tree).or_die(&tree);
    let log = log_path(d);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .or_die(&log);
    if !d.join("config").exists() {
        write_config(d, &Over::new());
    }
    let s = sizes(&overrides(d));
    if fresh {
        say!(
            "Created {}: this machine's memory, one identity, forever.",
            pretty(d)
        );
    } else {
        say!("Found {}: {}.", pretty(d), plural(log_len(d), "memory"));
    }
    say!("Sizes live in {}/config; the defaults are fine.", pretty(d));
    say!();
    say!("Paste this at the top of your agent's AGENTS.md (or CLAUDE.md), done:");
    say!();
    let block = TEMPLATE
        .replace("{memo}", me())
        .replace("{data}", &pretty(d))
        .replace("{chars}", &s.entry_chars.to_string());
    say!("{}", block.trim_end());
}

/// Split the document into parts that survive any harness's output cap.
fn paginate(c: &Ctx, lines: Vec<String>) -> Vec<Vec<String>> {
    let (mut parts, mut cur, mut size) = (Vec::new(), Vec::new(), 0);
    for line in lines {
        let n = line.len() + 1;
        if !cur.is_empty() && (cur.len() >= c.s.part_lines || size + n > c.s.part_chars) {
            parts.push(std::mem::take(&mut cur));
            size = 0;
        }
        cur.push(line);
        size += n;
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

fn print_pins(d: &Path) {
    let pins = read_pins(d);
    if pins.is_empty() {
        return;
    }
    say!("Pinned:");
    for (i, p) in pins.iter().enumerate() {
        say!("  {}. {p}", i + 1);
    }
}

fn cmd_wake(c: &Ctx, args: &[String]) {
    let d = &c.d;
    let now = log_len(d);
    let (mut k, mut t) = (1usize, now);
    if !args.is_empty() {
        let digits = |a: &String| !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit());
        if args.len() > 2 || !args.iter().all(digits) {
            die(format!("usage: {} wake [part [T]]", me()));
        }
        k = args[0].parse().unwrap_or(0);
        if args.len() == 2 {
            t = args[1].parse().unwrap_or(u64::MAX);
            if t > now {
                die(format!(
                    "T={t}, but the log holds {}. Run: {} wake",
                    plural(now, "memory"),
                    me()
                ));
            }
        }
    }
    // A part is rendered as of t, so a note landing between two parts cannot
    // shift a boundary and drop a line.
    if t == 0 {
        print_pins(d);
        say!(
            "No memories yet. Record the first with: {} note \"<one line>\"",
            me()
        );
        say!("You are awake.");
        return;
    }
    let blocks = cover(t, c.s.wake_lines);
    let mut log = Log::open(d);
    // Detail only grows toward the present, so the verbatim memories are one
    // contiguous tail: read it in one go, and with it any amendments.
    let raw_from = blocks.iter().find(|b| b.1 - b.0 == 1).map_or(t, |b| b.0);
    let raw = log.slice(raw_from, t);
    let by: HashMap<u64, u64> = raw
        .iter()
        .filter_map(|e| Some((amends(&e.text)?, e.id)))
        .collect();
    let (mut lines, mut gaps) = (Vec::new(), 0);
    for &(lo, hi) in blocks.iter().filter(|b| b.1 - b.0 > 1) {
        let mut s = tree_get(d, lo, hi);
        if s.is_none() {
            let owed = pending_count(d, t);
            if owed > REFUSE_MAX {
                // A bulk import owes thousands of compressions. Refusing would
                // lock the agent out of its memory until it paid them all:
                // wake with this line marked instead, and hand the work over.
                gaps += 1;
                lines.push(format!(
                    "#{lo}-{} {} not compressed yet: zoom or recall reach it",
                    hi - 1,
                    log.span(lo, hi)
                ));
                continue;
            }
            if let Some(nap) = next_nap(c, t) {
                // The ONLY reason to refuse: this document cannot be written
                // without that summary, and it is cheap to pay now.
                say!(
                    "Cannot wake: the memory context needs #{lo}-{}, which is not compressed yet.\n\
                     Do the {} below, then run {} wake again.\n",
                    hi - 1,
                    plural(owed, "compression"),
                    me()
                );
                say!("{nap}");
                let _ = io::stdout().flush();
                process::exit(1);
            }
            s = tree_get(d, lo, hi); // a parallel session may have paid it
        }
        let Some(s) = s else {
            // Nothing is pending, so the record exists but is blank -- a
            // corrupt write. Drop it and the next nap rebuilds it.
            die(format!(
                "The summary of #{lo}-{} is blank. Run: {} forget {lo}-{}",
                hi - 1,
                me(),
                hi - 1
            ))
        };
        lines.push(format!("#{lo}-{} {} {s}", hi - 1, log.span(lo, hi)));
    }
    lines.extend(
        raw.iter()
            .map(|e| format!("{e}{}", amended_note(e.id, &by))),
    );
    let parts = paginate(c, lines);
    if k < 1 || k > parts.len() {
        die(format!(
            "No part {k}: the memory has {}. Run: {} wake",
            plural(parts.len() as u64, "part"),
            me()
        ));
    }
    if parts.len() > 1 {
        // The count is here so the T in `memo wake 2 296` reads as what it
        // is: the snapshot this document was written from.
        say!(
            "Your memory, part {k} of {}, oldest first ({}).",
            parts.len(),
            plural(t, "memory")
        );
    }
    if k == 1 {
        print_pins(d);
    }
    say!("{}", parts[k - 1].join("\n"));
    if k < parts.len() {
        // The only instruction that survives every harness's truncation (pi
        // drops the HEAD of a long output): the read is unfinished, and how
        // to continue it.
        say!("Not awake yet. Run: {} wake {} {t}", me(), k + 1);
        return;
    }
    // Always, even for a one-part memory: the contract an agent is given is
    // "run parts until one says awake".
    say!("You are awake.");
    if gaps > 0 {
        say!(
            "{} of these lines are not compressed yet; {} owed. They fill in as the naps are paid.",
            gaps,
            plural(pending_count(d, t), "compression")
        );
    }
    print_nap(c, t, true);
}

fn cmd_note(c: &Ctx, args: &[String]) {
    if args.len() != 1 {
        die(format!(
            "usage: {} note \"<one line, at most {} bytes>\"   (or `-` to read stdin)",
            me(),
            c.s.entry_chars
        ));
    }
    let text = check(c, &text_arg(&args[0]));
    let t = log_len(&c.d);
    let recent = Log::open(&c.d).slice(t.saturating_sub(DUP_WINDOW), t);
    let mine = words(&text);
    if let Some(e) = recent
        .iter()
        .rev()
        .find(|e| words(&e.text) == mine && !mine.is_empty())
    {
        say!("Already noted as #{}: nothing saved.", e.id);
        return;
    }
    let close = recent
        .iter()
        .rev()
        .find(|e| similar(&mine, &words(&e.text)));
    let i = log_append(&c.d, &[(today(), text)]);
    say!("Saved as #{i}.");
    if let Some(e) = close {
        say!("It nearly repeats #{}: {}", e.id, e.text);
    }
    print_nap(c, i + 1, true);
}

fn cmd_amend(c: &Ctx, args: &[String]) {
    if args.len() != 2 {
        die(format!(
            "usage: {} amend <id> \"<what is true instead>\"",
            me()
        ));
    }
    let t = log_len(&c.d);
    let id: u64 = args[0]
        .trim_start_matches('#')
        .parse()
        .unwrap_or_else(|_| die(format!("'{}' is not a memory id, like 12.", args[0])));
    if id >= t {
        die(format!(
            "#{id} does not exist: the memory holds {}.",
            plural(t, "memory")
        ));
    }
    let text = check(c, &format!("amends #{id}: {}", text_arg(&args[1]).trim()));
    let i = log_append(&c.d, &[(today(), text)]);
    say!("Saved as #{i}, amending #{id}.");
    print_nap(c, i + 1, true);
}

fn cmd_nap(c: &Ctx, args: &[String]) {
    let t = log_len(&c.d);
    if !args.len().is_multiple_of(2) {
        die(format!(
            "usage: {} nap <lo>-<hi> \"<one line>\" [<lo>-<hi> \"<one line>\" ...]",
            me()
        ));
    }
    for pair in args.chunks(2) {
        let (lo, hi) = block_id(&pair[0]);
        let Some(&next) = pending(&c.d, t, 1).first() else {
            say!("Nothing left to compress.");
            return;
        };
        if (lo, hi) != next {
            if tree_get(&c.d, lo, hi).is_some() {
                say!("{lo}-{} is already settled.", hi - 1);
                continue;
            }
            die(format!(
                "Wrong block: {}. Blocks are built in order; the next is {}-{}. Run: {} nap",
                pair[0],
                next.0,
                next.1 - 1,
                me()
            ));
        }
        if tree_put(&c.d, (lo, hi), &check(c, &text_arg(&pair[1]))) {
            say!("{lo}-{} saved.", hi - 1);
        } else {
            say!("{lo}-{} was settled or forgotten meanwhile.", hi - 1);
        }
    }
    match next_nap(c, t) {
        None => say!("Nothing left to compress."),
        Some(nap) => {
            if !args.is_empty() {
                say!();
            }
            say!("{nap}");
        }
    }
}

/// Show this memory's sizes, or change one. An empty value restores the
/// default. A config the tool wrote with stale notes (an old path, an old
/// default) is rewritten, keeping its overrides.
fn cmd_config(c: &Ctx, args: &[String]) {
    let mut over = overrides(&c.d);
    let names: Vec<_> = KNOBS.iter().map(|k| k.0).collect();
    for a in args {
        let (k, v) = a.split_once('=').unwrap_or((a, "\0"));
        let Some(k) = knob(&k.trim().to_uppercase()).filter(|_| v != "\0") else {
            die(format!(
                "usage: {} config [NAME=VALUE ...]   # NAME one of {}",
                me(),
                names.join(", ")
            ))
        };
        if v.trim().is_empty() {
            over.remove(k);
        } else {
            over.insert(k, size(k, v.trim(), ""));
        }
    }
    let p = c.d.join("config");
    if fs::read_to_string(&p).ok().as_deref() != Some(config_text(&over).as_str()) {
        write_config(&c.d, &over);
    }
    for (k, default, what) in KNOBS {
        let v = over.get(k).copied().unwrap_or(default);
        let note = if over.contains_key(k) {
            format!(" (default {default})")
        } else {
            String::new()
        };
        say!("{k:<12} {v:<7} {what}{note}");
    }
}

/// A summary can be wrong -- mistyped, or a bad compression. Drop it and
/// everything built on top of it; the next nap computes them again.
fn cmd_forget(c: &Ctx, args: &[String]) {
    if args.len() != 1 {
        die(format!("usage: {} forget <lo>-<hi>", me()));
    }
    let gone = tree_drop(&c.d, block_id(&args[0]));
    let Some(first) = gone.first() else {
        die(format!("No summary at {}.", args[0]))
    };
    say!(
        "Forgot {}, from {}-{} up. Run: {} nap",
        plural(gone.len() as u64, "summary"),
        first.0,
        first.1 - 1,
        me()
    );
}

fn cmd_recall(c: &Ctx, args: &[String]) {
    let usage = || -> ! {
        die(format!(
            "usage: {} recall [--around N] <regex> [<regex> ...]",
            me()
        ))
    };
    let (mut around, mut terms, mut it) = (0u64, Vec::new(), args.iter());
    while let Some(a) = it.next() {
        let n = match a.as_str() {
            "--around" | "-C" => it.next().unwrap_or_else(|| usage()),
            _ => match a.strip_prefix("--around=") {
                Some(n) => n,
                None => {
                    terms.push(a);
                    continue;
                }
            },
        };
        around = n
            .parse()
            .ok()
            .filter(|n| *n <= 10)
            .unwrap_or_else(|| die("--around takes 0 to 10."));
    }
    if terms.is_empty() {
        usage();
    }
    let pats: Vec<Regex> = terms
        .iter()
        .map(|t| Regex::new(&format!("(?i){t}")).unwrap_or_else(|e| die(format!("bad regex: {e}"))))
        .collect();
    // One pass, keeping only the newest matches that fit the cap `wake`
    // respects: a vague regex matches the whole log, and the whole log does
    // not fit in a harness's output.
    let (mut hits, mut size) = (0u64, 0usize);
    let mut out: VecDeque<(u64, String, usize)> = VecDeque::new();
    let mut by = HashMap::new();
    let t = log_len(&c.d);
    log_scan(&c.d, |at, e| {
        if let Some(target) = amends(&e.text) {
            by.insert(target, e.id);
        }
        let line = e.to_string();
        if !pats.iter().all(|p| p.is_match(&line)) {
            return;
        }
        hits += 1;
        let cost = (line.len() + 1) * (2 * around as usize + 1);
        out.push_back((at, line, cost));
        size += cost;
        while size > c.s.part_chars && out.len() > 1 {
            size -= out.pop_front().map_or(0, |x| x.2);
        }
    });
    if hits == 0 {
        say!("No match.");
        return;
    }
    if around == 0 {
        for (at, line, _) in &out {
            say!("{line}{}", amended_note(*at, &by));
        }
    } else {
        // Each hit with its neighbours; overlapping windows merge, and a
        // neighbour is indented so the hits stand out.
        let hit: HashSet<u64> = out.iter().map(|x| x.0).collect();
        let mut windows: Vec<Block> = Vec::new();
        for (at, _, _) in &out {
            let w = (at.saturating_sub(around), (at + around + 1).min(t));
            match windows.last_mut() {
                Some(last) if w.0 <= last.1 => last.1 = last.1.max(w.1),
                _ => windows.push(w),
            }
        }
        let mut log = Log::open(&c.d);
        for (i, &(lo, hi)) in windows.iter().enumerate() {
            if i > 0 {
                say!("--");
            }
            for (at, e) in (lo..hi).zip(log.slice(lo, hi)) {
                let lead = if hit.contains(&at) { "" } else { "  " };
                say!("{lead}{e}{}", amended_note(at, &by));
            }
        }
    }
    if (out.len() as u64) < hits {
        say!(
            "Newest {} of {}. Narrow the regex.",
            out.len(),
            plural(hits, "match")
        );
    } else {
        say!("{}.", plural(hits, "match"));
    }
    if around == 0 {
        let newest = out.back().map_or(0, |x| x.0);
        let lo = newest / RAW_MAX * RAW_MAX;
        say!(
            "Context: {} recall --around 3 <regex>, or {} zoom {lo}-{} (holds #{newest}).",
            me(),
            me(),
            lo + RAW_MAX - 1
        );
    }
}

/// One node of the tree, opened: its two halves, each rendered as wake
/// renders it -- a summary, or the raw memory once a half is single.
fn cmd_zoom(c: &Ctx, args: &[String]) {
    if args.len() != 1 {
        die(format!(
            "usage: {} zoom <lo>-<hi>   # a block id, as wake prints them",
            me()
        ));
    }
    let (lo, hi) = block_id(&args[0]);
    let t = log_len(&c.d);
    if lo >= t {
        die(format!(
            "#{} is beyond the memory: it holds {}. Run: {} wake",
            args[0],
            plural(t, "memory"),
            me()
        ));
    }
    let mid = (lo + hi) / 2;
    let mut log = Log::open(&c.d);
    for (a, b) in [(lo, mid), (mid, hi)] {
        if a >= t {
            continue; // the future: no memories there yet
        }
        if b - a == 1 {
            say!("{}", log.get(a));
        } else {
            match tree_get(&c.d, a, b) {
                Some(s) => say!("#{a}-{} {} {s}", b - 1, log.span(a, b)),
                None => say!("#{a}-{} not compressed yet", b - 1),
            }
        }
    }
}

fn cmd_pin(c: &Ctx, args: &[String]) {
    if args.len() > 1 {
        die(format!(
            "usage: {} pin [\"<a fact wake must always show>\"]",
            me()
        ));
    }
    let Some(a) = args.first() else {
        let pins = read_pins(&c.d);
        if pins.is_empty() {
            say!(
                "No pins. Pin a fact wake must always show: {} pin \"...\"",
                me()
            );
        }
        for (i, p) in pins.iter().enumerate() {
            say!("{}. {p}", i + 1);
        }
        return;
    };
    let text = check(c, &text_arg(a));
    let _lock = locked(&c.d);
    let mut pins = read_pins(&c.d);
    if let Some(i) = pins.iter().position(|p| *p == text) {
        say!("Already pinned as {}.", i + 1);
        return;
    }
    if pins.len() >= PIN_MAX {
        die(format!(
            "{PIN_MAX} pins is the limit. Unpin one first: {} unpin <n>",
            me()
        ));
    }
    pins.push(text);
    write_pins(&c.d, &pins);
    say!("Pinned as {}.", pins.len());
}

fn cmd_unpin(c: &Ctx, args: &[String]) {
    let usage = || -> ! {
        die(format!(
            "usage: {} unpin <n>   # n as `{} pin` lists it",
            me(),
            me()
        ))
    };
    let [a] = args else { usage() };
    let n: usize = a.parse().unwrap_or_else(|_| usage());
    let _lock = locked(&c.d);
    let mut pins = read_pins(&c.d);
    if n < 1 || n > pins.len() {
        die(format!(
            "No pin {n}: there {}.",
            if pins.len() == 1 {
                "is 1".into()
            } else {
                format!("are {}", pins.len())
            }
        ));
    }
    let gone = pins.remove(n - 1);
    write_pins(&c.d, &pins);
    say!("Unpinned {n}: {gone}");
}

/// Bulk-append historical memories: 'YYYY-MM-DD <text>' per line. For
/// bootstrapping an identity from older records. Used once.
fn cmd_import(c: &Ctx, args: &[String]) {
    let [file] = args else {
        die(format!(
            "usage: {} import <file>   # lines of 'YYYY-MM-DD <text>'",
            me()
        ))
    };
    let path = PathBuf::from(file);
    let bytes = fs::read(&path).or_die(&path);
    let src = String::from_utf8(bytes).unwrap_or_else(|_| {
        die(format!(
            "{} is not UTF-8 text. Convert it, then import again.",
            pretty(&path)
        ))
    });
    let t = log_len(&c.d);
    let mut last = if t > 0 {
        Log::open(&c.d).get(t - 1).date
    } else {
        "0000-00-00".into()
    };
    let shape = Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap();
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate().map(|(i, l)| (i + 1, l)) {
        if line.trim().is_empty() {
            continue;
        }
        let (date, text) = line.split_once(' ').unwrap_or((line, ""));
        if !shape.is_match(date) {
            die(format!(
                "line {i}: expected 'YYYY-MM-DD <text>', got: {line}"
            ));
        }
        if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
            die(format!("line {i}: {date} is not a real date."));
        }
        if date < last.as_str() {
            die(format!(
                "line {i}: date {date} precedes the previous memory ({last})."
            ));
        }
        let text = text.trim();
        if text.is_empty() || text.len() > c.s.entry_chars {
            die(format!(
                "line {i}: {} bytes, limit {}.",
                text.len(),
                c.s.entry_chars
            ));
        }
        out.push((date.to_string(), text.to_string()));
        last = date.to_string();
    }
    if out.is_empty() {
        die(format!("{file} has no memories."));
    }
    let base = log_append(&c.d, &out);
    say!(
        "Imported {}, #{base} to #{}.",
        plural(out.len() as u64, "memory"),
        base + out.len() as u64 - 1
    );
    let n = pending_count(&c.d, log_len(&c.d));
    if n > 0 {
        say!("{} pending. Run: {} nap", plural(n, "compression"), me());
    }
}

fn main() {
    let args: Vec<String> = env::args_os()
        .skip(1)
        .map(|a| {
            a.into_string()
                .unwrap_or_else(|_| die("Arguments must be valid Unicode."))
        })
        .collect();
    let usage = USAGE.replace("{memo}", me());
    let Some(cmd) = args.first() else {
        say!("{usage}");
        return;
    };
    let run: fn(&Ctx, &[String]) = match cmd.as_str() {
        "-h" | "--help" | "help" => {
            say!("{usage}");
            return;
        }
        "--version" => {
            say!("memo {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        // `init` is the only command that may run without an existing memory:
        // it is the one that creates it.
        "init" => return cmd_init(&memory_dir(), &args[1..]),
        "wake" => cmd_wake,
        "note" => cmd_note,
        "amend" => cmd_amend,
        "nap" => cmd_nap,
        "recall" => cmd_recall,
        "zoom" => cmd_zoom,
        "forget" => cmd_forget,
        "pin" => cmd_pin,
        "unpin" => cmd_unpin,
        "config" => cmd_config,
        "import" => cmd_import,
        _ => {
            eprintln!("No such command: {cmd}\n");
            eprintln!("{usage}");
            process::exit(1);
        }
    };
    let d = store();
    let s = sizes(&overrides(&d));
    run(&Ctx { d, s }, &args[1..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("optmem-unit-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn bare_memo_only_when_the_first_memo_on_path_is_this_one() {
        let (mine, other) = (scratch("mine"), scratch("other"));
        let exe = mine.join("memo.exe");
        fs::write(&exe, b"").unwrap();
        let path = |dirs: &[&Path]| env::join_paths(dirs).unwrap();
        assert_eq!(invocation(&exe, &path(&[&mine]), true), "memo");
        assert_eq!(
            invocation(&exe, &path(&[&other]), true),
            slashes(&absolute(&exe))
        );
        fs::write(other.join("memo.cmd"), b"").unwrap();
        assert_ne!(
            invocation(&exe, &path(&[&other, &mine]), true),
            "memo",
            "a shim earlier on PATH shadows it"
        );
        fs::write(mine.join("memo"), b"").unwrap();
        assert_ne!(
            invocation(&exe, &path(&[&mine]), true),
            "memo",
            "bash would run the extensionless file"
        );
        let spaced = scratch("with space").join("memo.exe");
        fs::write(&spaced, b"").unwrap();
        assert!(
            invocation(&spaced, OsStr::new(""), true).starts_with('"'),
            "a path with spaces is unquoted"
        );
        assert!(!invocation(&spaced, OsStr::new(""), true).contains('\\'));
    }

    #[test]
    fn similar_notes_share_nearly_all_words() {
        let w = |s: &str| words(s);
        assert!(similar(
            &w("the user has a dog named Cisco"),
            &w("The user has a dog, named Cisco!")
        ));
        assert!(!similar(&w("parallel note 3"), &w("parallel note 4")));
        assert!(!similar(
            &w("the user has a dog named Cisco"),
            &w("the user has a cat named Mia")
        ));
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1u64, "memory"), "1 memory");
        assert_eq!(plural(2u64, "memory"), "2 memories");
        assert_eq!(plural(3u64, "match"), "3 matches");
        assert_eq!(plural(0u64, "part"), "0 parts");
    }
}
