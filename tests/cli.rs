//! OptMem invariants, checked through the real binary against a synthetic
//! life of 2000 memories, with a fake compressor so the run is deterministic.
//! Every store lives in a scratch directory, and HOME/USERPROFILE point there
//! too: a test must never reach the user's real memory.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use regex::Regex;

const N: u64 = 2000;
const WAKE_LINES: usize = 96;
// Verified caps of the harnesses in the wild: Claude Code cuts a command's
// output at 30,000 chars, pi at 50 KB / 2000 lines. A part must fit both.
const CAP_CHARS: usize = 30000;
const CAP_LINES: usize = 2000;

struct Out {
    code: i32,
    out: String,
    err: String,
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "optmem-test-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn command(env: &[(&str, &Path)]) -> Command {
    let sandbox = std::env::temp_dir().join("optmem-test-home");
    let mut c = Command::new(env!("CARGO_BIN_EXE_memo"));
    c.env("HOME", &sandbox)
        .env("USERPROFILE", &sandbox)
        .env_remove("MEMORY_DIR");
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

fn finish(c: &mut Command, stdin: Option<&str>) -> Out {
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().expect("memo runs");
    {
        let mut pipe = child.stdin.take().unwrap();
        if let Some(s) = stdin {
            pipe.write_all(s.as_bytes()).unwrap();
        }
    }
    let o = child.wait_with_output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn memo(d: &Path, args: &[&str]) -> Out {
    finish(command(&[("MEMORY_DIR", d)]).args(args), None)
}

fn memo_in(d: &Path, args: &[&str], stdin: &str) -> Out {
    finish(command(&[("MEMORY_DIR", d)]).args(args), Some(stdin))
}

fn new_store(name: &str) -> PathBuf {
    let d = scratch(name);
    assert_eq!(memo(&d, &["init"]).code, 0);
    d
}

/// Every block buildable from t memories, smallest first.
fn complete(t: u64) -> Vec<(u64, u64)> {
    let (mut out, mut size) = (Vec::new(), 2);
    while size <= t {
        out.extend((0..t / size).map(|i| (i * size, (i + 1) * size)));
        size *= 2;
    }
    out
}

/// The block ids a nap prompt orders, in order.
fn nap_ids(out: &str) -> Vec<String> {
    let run = out
        .lines()
        .find(|l| l.starts_with("Run: ") && l.contains(" nap "))
        .unwrap_or("");
    Regex::new(r"(\d+-\d+) ")
        .unwrap()
        .captures_iter(run)
        .map(|c| c[1].to_string())
        .collect()
}

/// Pay every pending compression with a fake compressor, all ids at once.
fn settle(d: &Path, text: &str) -> usize {
    let mut paid = 0;
    let mut r = memo(d, &["nap"]);
    while !r.out.contains("Nothing left to compress") {
        let ids = nap_ids(&r.out);
        assert!(!ids.is_empty(), "no command offered:\n{}{}", r.out, r.err);
        let mut args = vec!["nap".to_string()];
        for id in &ids {
            args.push(id.clone());
            args.push(format!(
                "{text} {id} {}",
                "and the rest of what mattered ".repeat(7)
            ));
        }
        r = memo(d, &args.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(r.code, 0, "nap rejected a valid merge: {}", r.err);
        assert!(
            !r.out.contains("You are awake"),
            "nap must never claim the agent is awake"
        );
        paid += ids.len();
    }
    paid
}

fn size_of(p: &Path) -> u64 {
    fs::metadata(p).unwrap().len()
}

fn tree_size(d: &Path) -> u64 {
    fs::read_dir(d.join("TREE"))
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum()
}

fn fingerprint(d: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![d.to_path_buf()];
    while let Some(p) = stack.pop() {
        for e in fs::read_dir(&p).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().unwrap() != ".lock" {
                out.insert(
                    p.strip_prefix(d).unwrap().to_path_buf(),
                    fs::read(&p).unwrap(),
                );
            }
        }
    }
    out
}

#[test]
fn a_fresh_user_gets_a_memory_that_works_with_nothing_on_path() {
    let home = scratch("home");
    let fresh = || {
        let mut c = command(&[("HOME", &home), ("USERPROFILE", &home)]);
        c.env_remove("MEMORY_DIR");
        c
    };
    let r = finish(fresh().arg("wake"), None);
    assert!(
        r.code == 1 && r.err.contains(" init"),
        "wake must point at init: {}",
        r.err
    );
    let r = finish(fresh().arg("init"), None);
    assert!(
        r.code == 0 && r.out.contains("## Memory") && r.out.contains("You are a"),
        "{}",
        r.out
    );
    assert!(
        home.join(".optmem/memory/config").exists(),
        "init must create ~/.optmem/memory"
    );
    assert!(
        finish(fresh().arg("init"), None).out.contains("Found"),
        "init must be idempotent"
    );
    let r = finish(fresh().arg("wake"), None);
    assert!(r.code == 0 && r.out.contains("You are awake."));

    // Every command the tool prints must RUN on the machine that printed it,
    // even with nothing on PATH.
    let bare = if cfg!(windows) {
        PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32")
    } else {
        PathBuf::from("/usr/bin:/bin")
    };
    let note = |text: &str| finish(fresh().env("PATH", &bare).args(["note", text]), None);
    note("the first thing that happened");
    let asked = note("the second thing that happened");
    let order: Vec<&str> = asked
        .out
        .lines()
        .filter_map(|l| l.strip_prefix("Run: "))
        .collect();
    assert_eq!(
        order.len(),
        1,
        "note did not order a compression: {}",
        asked.out
    );
    let line = order[0].replace("\"<your line>\"", "\"both things\"");
    #[cfg(windows)]
    let mut shell = {
        let mut c = Command::new("cmd");
        std::os::windows::process::CommandExt::raw_arg(&mut c, format!("/C {line}"));
        c
    };
    #[cfg(not(windows))]
    let mut shell = {
        let mut c = Command::new("sh");
        c.args(["-c", &line]);
        c
    };
    shell
        .env("PATH", &bare)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("MEMORY_DIR");
    let obeyed = finish(&mut shell, None);
    assert!(
        obeyed.code == 0 && obeyed.out.contains("saved"),
        "{line:?} -> {}",
        obeyed.err
    );

    // a size typed by hand into `config` names its file and line
    let cfg = home.join(".optmem/memory/config");
    OpenOptions::new()
        .append(true)
        .open(&cfg)
        .unwrap()
        .write_all(b"WAKE_LNES = 100\n")
        .unwrap();
    for c in ["wake", "config"] {
        let r = finish(fresh().arg(c), None);
        assert!(
            r.code == 1 && r.err.contains("config line") && r.err.contains("WAKE_LNES"),
            "{}",
            r.err
        );
    }
    fs::write(&cfg, "").unwrap();

    // a filesystem error is reported in the tool's own voice
    let file = home.join("not-a-dir");
    fs::write(&file, "x").unwrap();
    let r = finish(command(&[("MEMORY_DIR", &file)]).arg("init"), None);
    assert!(
        r.code == 1 && !r.err.contains("panicked") && r.err.contains("not-a-dir"),
        "{}",
        r.err
    );
}

#[test]
fn a_typo_in_memory_dir_never_creates_a_second_identity() {
    let d = scratch("typo").join("missing");
    let r = memo(&d, &["wake"]);
    assert!(r.code == 1 && r.err.contains("No memory at"));
    assert!(!d.exists());
}

#[test]
fn a_life_of_two_thousand_memories() {
    let d = new_store("life");
    fs::remove_file(d.join("config")).unwrap();

    for (args, want) in [
        (vec!["note", &*"x".repeat(281)], "Too long"),
        (vec!["note", "two\nlines"], "one line"),
        (vec!["note", "   "], "Empty"),
    ] {
        let r = memo(&d, &args);
        assert!(r.code == 1 && r.err.contains(want), "{want}: {}", r.err);
    }
    let r = memo(&d, &["wake"]);
    assert!(r.out.contains("No memories yet") && r.out.trim_end().ends_with("You are awake."));

    let seed = d.join("seed.txt");
    let day0 = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
    let mut f = File::create(&seed).unwrap();
    for i in 0..N {
        let day = day0 + chrono::Days::new(i / 5);
        writeln!(f, "{day} memory number {i}, a thing that happened, was weighed against the rest of the week, turned out to matter more than anyone guessed at the time").unwrap();
    }
    drop(f);
    let r = memo(&d, &["import", seed.to_str().unwrap()]);
    assert!(
        r.out.contains(&format!("Imported {N}")),
        "{}{}",
        r.out,
        r.err
    );
    assert!(
        !d.join("config").exists(),
        "a store wrote its own config: the defaults are now frozen in it"
    );

    // after a bulk import, wake does not lock the agent out: it wakes with the
    // missing summaries marked, and hands the work over
    let r = memo(&d, &["wake", "1"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(
        r.out
            .contains("not compressed yet: zoom or recall reach it"),
        "{}",
        r.out
    );
    let last = (1..10)
        .map(|k| memo(&d, &["wake", &k.to_string()]))
        .take_while(|r| r.code == 0)
        .last()
        .unwrap();
    assert!(
        last.out.contains("You are awake.") && last.out.contains("owed"),
        "{}",
        last.out
    );
    assert!(
        last.out.contains("Compress each of these 4 blocks"),
        "independent naps are batched:\n{}",
        last.out
    );

    let paid = settle(&d, "merged");
    assert_eq!(paid, complete(N).len(), "did {paid} naps");
    assert_eq!(
        memo(&d, &["wake"]).code,
        0,
        "wake still refuses after a full nap chain"
    );

    // the document survives pagination, and every part fits every harness
    let (mut lines, mut k) = (Vec::new(), 1);
    loop {
        let r = memo(&d, &["wake", &k.to_string()]);
        if r.code != 0 {
            break;
        }
        assert!(
            r.out.len() < CAP_CHARS && r.out.lines().count() < CAP_LINES,
            "part {k} too big"
        );
        lines.extend(
            r.out
                .lines()
                .filter(|l| l.starts_with('#'))
                .map(String::from),
        );
        k += 1;
    }
    let parts = k - 1;
    assert!(
        parts > 1,
        "a {WAKE_LINES}-line memory should need more than one part"
    );
    assert_eq!(lines.len(), WAKE_LINES);
    assert!(
        lines[lines.len() - 1].starts_with(&format!("#{} ", N - 1)),
        "newest memory not last / not raw"
    );
    assert!(
        Regex::new(r"^#0-\d+ \[2020-01-01\.\.\d\d-\d\d\] merged")
            .unwrap()
            .is_match(&lines[0]),
        "{}",
        lines[0]
    );
    assert!(Regex::new(r"Run: .*memo(\.exe)? wake 2")
        .unwrap()
        .is_match(&memo(&d, &["wake"]).out));
    assert!(memo(&d, &["wake", &parts.to_string()])
        .out
        .contains("You are awake."));
    assert_eq!(memo(&d, &["wake", &(parts + 1).to_string()]).code, 1);

    // append-only, fixed width
    let log = d.join("LOG.txt");
    let logsz = size_of(&log);
    memo(&d, &["note", "one more thing happened today"]);
    assert!(size_of(&log) > logsz && logsz.is_multiple_of(320));
    for e in fs::read_dir(d.join("TREE")).unwrap() {
        assert_eq!(e.unwrap().metadata().unwrap().len() % 288, 0);
    }
    let r = memo(&d, &["nap", "0-1", "attempted overwrite"]);
    assert!(r.code == 0 && r.out.contains("Nothing left to compress"));

    // recall matches the whole line: id and date included
    let r = memo(&d, &["recall", "memory number 7,"]);
    assert!(
        r.out.contains("#7 ") && r.out.contains("1 match."),
        "{}",
        r.out
    );
    assert!(r.out.contains("zoom 0-15 (holds #7)"), "{}", r.out);
    assert!(memo(&d, &["recall", "^#7 "])
        .out
        .contains("memory number 7,"));
    let r = memo(&d, &["recall", "2020-01-02"]);
    assert!(
        r.out.contains("#7 ") && r.out.contains("5 matches."),
        "{}",
        r.out
    );

    // zoom: halving 1024 memories down to one takes 10 calls
    let (target, mut lo, mut hi) = (777u64, 0u64, 1024u64);
    let id_re = Regex::new(r"^#(\d+)(?:-(\d+))? ").unwrap();
    for _ in 0..10 {
        let r = memo(&d, &["zoom", &format!("{lo}-{}", hi - 1)]);
        let kids: Vec<(u64, u64)> = r
            .out
            .lines()
            .map(|l| {
                let c = id_re.captures(l).expect("zoom line has an id");
                let a: u64 = c[1].parse().unwrap();
                (
                    a,
                    c.get(2)
                        .map_or(a + 1, |m| m.as_str().parse::<u64>().unwrap() + 1),
                )
            })
            .collect();
        let mid = (lo + hi) / 2;
        assert_eq!(kids, vec![(lo, mid), (mid, hi)]);
        (lo, hi) = kids[(target >= mid) as usize];
    }
    assert_eq!(lo, target);
    assert!(memo(&d, &["zoom", "776-777"])
        .out
        .contains("memory number 777,"));
    assert!(memo(&d, &["zoom", "0-3"])
        .out
        .contains("#0-1 [2020-01-01] merged 0-1"));
    assert!(memo(&d, &["zoom", "1024-2047"])
        .out
        .contains("#1536-2047 not compressed yet"));
    let r = memo(&d, &["zoom", &format!("{N}-{}", N + 1)]);
    assert!(r.out.lines().count() == 1 && r.out.contains(&format!("#{N} ")));
    for bad in [&["zoom", "3-9"][..], &["zoom", "9-3"], &["zoom"]] {
        assert_eq!(memo(&d, bad).code, 1, "{bad:?}");
    }
    let r = memo(&d, &["zoom", "1048576-2097151"]);
    assert!(r.code == 1 && r.err.contains("beyond the memory") && r.err.contains(" wake"));

    // forget drops a summary and everything above it; the log is untouched
    let (before, logsize) = (tree_size(&d), size_of(&log));
    let r = memo(&d, &["forget", "16-31"]);
    assert!(r.out.contains("16-31"));
    assert!(tree_size(&d) < before && size_of(&log) == logsize);
    let r = memo(&d, &["wake"]);
    // forget truncates every level from there on: hundreds owed, so wake
    // degrades to marked gaps instead of refusing
    assert!(
        r.code == 0 && r.out.contains("not compressed yet") && r.out.contains("owed"),
        "{}{}",
        r.out,
        r.err
    );
    let mid = tree_size(&d);
    let r = memo(&d, &["nap", "0-1", "attempted overwrite"]);
    assert!(r.code == 0 && r.out.contains("already settled"));
    assert_eq!(tree_size(&d), mid);
    let r = memo(&d, &["nap", "0-31", "out of order"]);
    assert!(r.code == 1 && r.err.contains("Wrong block"));
    assert!(settle(&d, "rebuilt") > 0);
    assert_eq!(memo(&d, &["wake"]).code, 0);
    assert_eq!(tree_size(&d), before);
    assert_eq!(memo(&d, &["forget", "17-32"]).code, 1);
    memo(&d, &["forget", "16-31"]);
    assert!(memo(&d, &["zoom", "0-31"])
        .out
        .contains("#16-31 not compressed yet"));
    settle(&d, "rebuilt");
    assert_eq!(memo(&d, &["forget", "1048576-1048577"]).code, 1);

    // UTF-8: multi-byte characters shift no boundary and dodge no limit
    memo(
        &d,
        &[
            "note",
            "reunião com João em São Paulo: ação aprovada, coração tranquilo",
        ],
    );
    memo(
        &d,
        &["note", "a plain ascii memory right after the accented one"],
    );
    assert!(memo(&d, &["recall", "coração"]).out.contains("João"));
    assert!(memo(&d, &["recall", "plain ascii memory right after"])
        .out
        .contains(&format!("#{} ", N + 2)));
    let r = memo(&d, &["note", &"ã".repeat(150)]);
    assert!(r.code == 1 && r.err.contains("300 bytes"), "{}", r.err);
    settle(&d, "settled");

    // a part is rendered as of T: a note landing mid-wake shifts nothing,
    // and neither do the compressions it owes
    let t0 = (size_of(&log) / 320).to_string();
    let before = memo(&d, &["wake", "1", &t0]);
    assert_eq!(before.code, 0);
    memo(&d, &["note", "a note that lands between two wake calls"]);
    assert_eq!(memo(&d, &["wake", "1", &t0]).out, before.out);
    settle(&d, "settled mid-wake");
    assert_eq!(memo(&d, &["wake", "1", &t0]).out, before.out);
    assert_eq!(
        memo(&d, &["wake", "1", &(size_of(&log) / 320 + 99).to_string()]).code,
        1
    );

    let r = memo(&d, &["recall", "memory number"]);
    assert!(r.out.len() < CAP_CHARS && r.out.contains("Narrow the regex"));

    // sizes: set, obey, restore; bad values refused
    let r = memo(&d, &["config", "WAKE_LINES=12"]);
    assert!(
        r.out.contains("12") && r.out.contains("default 96"),
        "{}",
        r.out
    );
    assert!(memo(&d, &["wake"]).out.lines().count() <= 13);
    assert!(!memo(&d, &["config", "WAKE_LINES="]).out.contains("default"));
    assert!(memo(&d, &["wake"]).out.lines().count() > 13);
    for bad in [
        "WAKE_LINES=0",
        "WAKE_LINES=x",
        "ENTRY_CHARS=999",
        "NOPE=1",
        "WAKE_LINES",
    ] {
        assert_eq!(memo(&d, &["config", bad]).code, 1, "config accepted {bad}");
    }

    // re-running init on a lived-in store touches not one byte of it
    OpenOptions::new()
        .append(true)
        .open(d.join("config"))
        .unwrap()
        .write_all(b"WAKE_LINES=12\n")
        .unwrap();
    let before = fingerprint(&d);
    for _ in 0..3 {
        assert!(memo(&d, &["init"]).out.contains("Found"));
    }
    assert!(
        fingerprint(&d) == before,
        "init modified an existing memory"
    );
    assert!(memo(&d, &["wake"])
        .out
        .trim_end()
        .ends_with("You are awake."));
}

#[test]
fn wake_refuses_while_the_missing_summaries_are_cheap() {
    let d = new_store("refuse");
    for i in 0..32 {
        memo(&d, &["note", &format!("refusal probe memory {i}")]);
    }
    settle(&d, "settled");
    memo(&d, &["config", "WAKE_LINES=2"]);
    assert!(memo(&d, &["wake"]).out.contains("#16-31 ["));
    memo(&d, &["forget", "16-31"]);
    let r = memo(&d, &["wake"]);
    assert!(
        r.code == 1 && r.out.contains("Cannot wake") && r.out.contains("wake again"),
        "{}{}",
        r.out,
        r.err
    );
    assert!(
        r.out.contains("Do the 2 compressions below") && !r.out.contains("None"),
        "{}",
        r.out
    );
    settle(&d, "rebuilt");
    assert_eq!(memo(&d, &["wake"]).code, 0);
}

#[test]
fn parallel_notes_and_torn_writes() {
    let d = new_store("race");
    let procs: Vec<_> = (0..16)
        .map(|i| {
            command(&[("MEMORY_DIR", &d)])
                .args(["note", &format!("parallel note {i}")])
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut p in procs {
        p.wait().unwrap();
    }
    let log = fs::read(d.join("LOG.txt")).unwrap();
    let mut ids: Vec<u64> = log
        .chunks(320)
        .map(|r| {
            String::from_utf8_lossy(r).split(' ').next().unwrap()[1..]
                .parse()
                .unwrap()
        })
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        (0..16).collect::<Vec<_>>(),
        "parallel notes collided or vanished"
    );

    // a crash mid-append leaves a partial record; the next append drops it
    OpenOptions::new()
        .append(true)
        .open(d.join("LOG.txt"))
        .unwrap()
        .write_all(b"#99 2026-01-01 a half-written")
        .unwrap();
    let r = memo(&d, &["note", "the memory right after a torn write"]);
    assert!(
        r.code == 0 && r.out.contains("Saved as #16"),
        "{}{}",
        r.out,
        r.err
    );
    assert_eq!(size_of(&d.join("LOG.txt")) % 320, 0);
    assert!(memo(&d, &["recall", "right after a torn write"])
        .out
        .contains("#16 "));

    settle(&d, "settled");
    assert!(
        memo(&d, &["wake"])
            .out
            .trim_end()
            .ends_with("You are awake."),
        "a one-part wake must end awake"
    );
}

#[test]
fn a_command_queues_behind_the_lock() {
    let d = new_store("lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(d.join(".lock"))
        .unwrap();
    lock.lock().unwrap();
    let t = Instant::now();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        drop(lock);
    });
    let r = memo(&d, &["note", "written once the lock is free"]);
    releaser.join().unwrap();
    assert!(r.code == 0 && r.out.contains("Saved as #0"), "{}", r.err);
    assert!(
        t.elapsed() >= Duration::from_millis(500),
        "the note did not wait for the lock"
    );
}

#[test]
fn corrupt_records_point_at_forget() {
    // a blank summary record is work nap cannot see: wake names `forget`
    let d = new_store("blank");
    for i in 0..4 {
        memo(&d, &["note", &format!("corrupt store memory {i}")]);
    }
    for (id, s) in [("0-1", "one"), ("2-3", "two"), ("0-3", "all")] {
        assert_eq!(memo(&d, &["nap", id, s]).code, 0);
    }
    fs::write(d.join("config"), "WAKE_LINES = 2\n").unwrap();
    let mut f = OpenOptions::new()
        .write(true)
        .open(d.join("TREE/2"))
        .unwrap();
    f.write_all(&[b' '; 287]).unwrap();
    drop(f);
    let r = memo(&d, &["wake"]);
    assert!(
        r.code == 1 && r.err.contains("forget 0-1"),
        "{}{}",
        r.out,
        r.err
    );

    let bad = d.join("bad.txt");
    fs::write(&bad, "2027-99-99 an impossible date\n").unwrap();
    assert!(memo(&d, &["import", bad.to_str().unwrap()])
        .err
        .contains("not a real date"));
    let latin = d.join("latin1.txt");
    fs::write(&latin, b"2027-01-01 caf\xe9 in latin-1\n").unwrap();
    assert!(memo(&d, &["import", latin.to_str().unwrap()])
        .err
        .contains("not UTF-8"));

    // the same dead end at a big block's half
    let d = new_store("half");
    for i in 0..32 {
        memo(&d, &["note", &format!("half probe memory {i}")]);
    }
    let mut r = memo(&d, &["nap"]);
    while !r.out.contains("Compress memories #0-31 ") {
        let mut args = vec!["nap".to_string()];
        for id in nap_ids(&r.out) {
            args.extend([id, "settled".into()]);
        }
        r = memo(&d, &args.iter().map(String::as_str).collect::<Vec<_>>());
    }
    let mut f = OpenOptions::new()
        .write(true)
        .open(d.join("TREE/16"))
        .unwrap();
    f.write_all(&[b' '; 287]).unwrap();
    drop(f);
    let r = memo(&d, &["nap"]);
    assert!(
        r.code == 1 && r.err.contains("forget 0-15"),
        "{}{}",
        r.out,
        r.err
    );

    // a summary holding invalid UTF-8 is the same dead end in another coat
    let d = new_store("utf8");
    memo(&d, &["note", "an arrow \u{2192} survives any locale"]);
    let r = memo(&d, &["wake"]);
    assert!(r.code == 0 && r.out.contains('\u{2192}'));
    memo(&d, &["note", "utf8 probe second memory"]);
    memo(&d, &["nap", "0-1", "both utf8 probes"]);
    let mut f = OpenOptions::new()
        .write(true)
        .open(d.join("TREE/2"))
        .unwrap();
    f.write_all(b"\xff\xfe corrupt bytes").unwrap();
    drop(f);
    let r = memo(&d, &["zoom", "0-3"]);
    assert!(
        r.code == 1 && r.err.contains("forget 0-1"),
        "{}{}",
        r.out,
        r.err
    );
}

#[test]
fn stdin_carries_any_character_verbatim() {
    let d = new_store("stdin");
    let text = r#"say "hi" at 50% ^caret & $HOME `tick` 'single'"#;
    let r = memo_in(&d, &["note", "-"], &format!("{text}\n"));
    assert!(r.code == 0 && r.out.contains("Saved as #0"), "{}", r.err);
    assert!(memo(&d, &["recall", "caret"]).out.contains(text));
    let r = memo_in(&d, &["note", "-"], "two\nlines\n");
    assert!(r.code == 1 && r.err.contains("one line"));
}

#[test]
fn repeats_are_caught() {
    let d = new_store("dup");
    memo(&d, &["note", "The user's dog is named Cisco."]);
    let r = memo(&d, &["note", "the user's dog is named cisco"]);
    assert!(
        r.out.contains("Already noted as #0: nothing saved."),
        "{}",
        r.out
    );
    assert_eq!(size_of(&d.join("LOG.txt")), 320);
    memo(
        &d,
        &["note", "deploy finished for dentai web and api today"],
    );
    let r = memo(
        &d,
        &["note", "deploy finished for dentai web and the api today"],
    );
    assert!(
        r.out.contains("Saved as #2") && r.out.contains("nearly repeats #1"),
        "{}",
        r.out
    );
}

#[test]
fn pins_head_every_wake() {
    let d = new_store("pins");
    assert!(memo(&d, &["pin"]).out.contains("No pins"));
    assert!(memo(&d, &["pin", "projects live in C:/projetos"])
        .out
        .contains("Pinned as 1."));
    memo(&d, &["pin", "the dog is Cisco"]);
    assert!(memo(&d, &["pin", "the dog is Cisco"])
        .out
        .contains("Already pinned as 2."));
    assert!(memo(&d, &["wake"])
        .out
        .starts_with("Pinned:\n  1. projects live in C:/projetos\n  2. the dog is Cisco\n"));
    memo(&d, &["config", "PART_CHARS=2000"]);
    for i in 0..40 {
        memo(
            &d,
            &["note", &format!("filler {i}: {}", "word ".repeat(40))],
        );
    }
    settle(&d, "settled");
    assert!(memo(&d, &["wake", "1"]).out.contains("Pinned:"));
    assert!(
        !memo(&d, &["wake", "2"]).out.contains("Pinned:"),
        "pins print once, in part 1"
    );
    assert!(memo(&d, &["unpin", "1"])
        .out
        .contains("Unpinned 1: projects"));
    assert_eq!(memo(&d, &["pin"]).out, "1. the dog is Cisco\n");
    assert_eq!(memo(&d, &["unpin", "5"]).code, 1);
    for i in 0..15 {
        memo(&d, &["pin", &format!("fact {i}")]);
    }
    assert!(memo(&d, &["pin", "one too many"])
        .err
        .contains("16 pins is the limit"));
}

#[test]
fn amendments_show_where_the_original_does() {
    let d = new_store("amend");
    memo(&d, &["note", "the meeting is on monday"]);
    memo(&d, &["note", "something else entirely"]);
    let r = memo(&d, &["amend", "0", "the meeting moved to tuesday"]);
    assert!(r.out.contains("Saved as #2, amending #0."), "{}", r.out);
    let w = memo(&d, &["wake"]).out;
    assert!(
        w.contains("the meeting is on monday (amended by #2)"),
        "{w}"
    );
    assert!(w.contains("#2 ") && w.contains("amends #0: the meeting moved to tuesday"));
    assert!(memo(&d, &["recall", "monday"])
        .out
        .contains("(amended by #2)"));
    assert_eq!(memo(&d, &["amend", "9", "nope"]).code, 1);
}

#[test]
fn recall_takes_several_terms_and_context() {
    let d = new_store("recall");
    for t in [
        "alpha one",
        "beta two",
        "alpha beta three",
        "gamma four",
        "delta five",
    ] {
        memo(&d, &["note", t]);
    }
    let r = memo(&d, &["recall", "alpha", "beta"]);
    assert!(
        r.out.contains("#2 ") && !r.out.contains("#0 ") && r.out.contains("1 match."),
        "{}",
        r.out
    );
    let r = memo(&d, &["recall", "--around", "1", "gamma"]);
    assert!(
        r.out.contains("  #2 ") && r.out.contains("\n#3 ") && r.out.contains("  #4 "),
        "{}",
        r.out
    );
    assert_eq!(memo(&d, &["recall", "--around", "99", "x"]).code, 1);
    assert!(memo(&d, &["recall", "("]).err.contains("bad regex"));
}

#[test]
fn naps_go_out_in_batches() {
    let d = new_store("batch");
    for i in 0..8 {
        memo(&d, &["note", &format!("batch note {i}")]);
    }
    let r = memo(&d, &["nap"]);
    assert!(
        r.out.contains("Compress each of these 4 blocks"),
        "{}",
        r.out
    );
    assert!(
        r.out.contains("3 compressions remain after these."),
        "{}",
        r.out
    );
    assert_eq!(nap_ids(&r.out), ["0-1", "2-3", "4-5", "6-7"]);
    let r = memo(&d, &["nap", "0-1", "a", "2-3", "b", "4-5", "c", "6-7", "d"]);
    assert!(
        r.out.contains("6-7 saved.") && r.out.contains("Compress each of these 2 blocks"),
        "{}",
        r.out
    );
    let r = memo(&d, &["nap", "0-3", "ab", "4-7", "cd"]);
    assert!(
        r.out.contains("Compress memories #0-7 ")
            && r.out.contains("  #7 ")
            && !r.out.contains("remain"),
        "{}",
        r.out
    );
    assert_eq!(
        memo(&d, &["nap", "0-7"]).code,
        1,
        "an odd argument list is refused"
    );
}

#[test]
fn config_drops_stale_notes_and_keeps_overrides() {
    let d = new_store("config");
    fs::write(
        d.join("config"),
        "# Edit with `~\\.pi\\agent\\local-packages\\pi-optmem-windows\\memo config NAME=VALUE`.\n\
         # WAKE_LINES   = 208    # stale default\n  ENTRY_CHARS  = 200    # mine\n",
    )
    .unwrap();
    let r = memo(&d, &["config"]);
    assert!(r.out.contains("ENTRY_CHARS  200"), "{}", r.out);
    let text = fs::read_to_string(d.join("config")).unwrap();
    assert!(!text.contains(".pi") && !text.contains("208"), "{text}");
    assert!(
        text.contains("  ENTRY_CHARS  = 200") && text.contains("# WAKE_LINES   = 96"),
        "{text}"
    );
}
