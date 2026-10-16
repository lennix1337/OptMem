# OptMem

Permanent memory for AI agents. A short prompt, one small binary, plug and play.

A Rust port of [VictorTaelin/OptMem](https://github.com/VictorTaelin/OptMem).
It reads and writes the same files, so an existing memory needs no migration.

![how OptMem works](anim/optmem.gif)

## Install

Needs a Rust toolchain ([rustup.rs](https://rustup.rs)); on Windows run it from Git Bash.

```sh
curl -fsSL https://raw.githubusercontent.com/lennix1337/OptMem/main/install.sh | sh
```

It builds `memo` into `~/.optmem` and prints a `## Memory` block. Paste that at
the top of your agent's `AGENTS.md` (or `CLAUDE.md`), and you are done. Run the
same line again to update: it only replaces the binary.

Put `~/.optmem` on `PATH` to type `memo`. Once it is the first `memo` on `PATH`,
every command the tool prints uses a bare `memo`. Until then they name the
binary by its full path, with forward slashes, so bash, cmd and PowerShell
all run them.

## Commands

| | |
|---|---|
| `memo wake` | read the memory — the first command of every session |
| `memo note "..."` | record one memory: one line, up to 280 bytes; `memo note -` reads it from stdin |
| `memo amend <id> "..."` | correct memory #id; wake and recall mark the original as amended |
| `memo nap` | answer the merges that came due |
| `memo recall <regex>...` | search every memory; every regex must match; `--around N` adds neighbours |
| `memo zoom <lo>-<hi>` | open a tree node into its two halves |
| `memo forget <lo>-<hi>` | drop a bad summary; the next nap rebuilds it |
| `memo pin "..."` / `memo unpin <n>` | facts wake always shows first, up to 16 |
| `memo config [NAME=N]` | show this memory's sizes, or change one |
| `memo import <file>` | bulk-load `YYYY-MM-DD <text>` lines (bootstrap only) |

Merges arrive in the output of `note`. Independent ones (same tree level) come
in batches of up to 4, answered in one `memo nap 0-1 "..." 2-3 "..."`.
Nothing ever runs in the background.

What the tool does for you:

- Wake prints summary lines with the days they span: `#0-63 [2026-10-05..10-09] ...`.
- When a summary is missing and the work owed is small (16 compressions or
  fewer), wake refuses and hands that work over. When more is owed, as after
  a bulk import or a `forget`, it wakes anyway and marks the gaps.
- `note` refuses an exact repeat of any of the last 200 memories, and points
  out a near repeat.
- Parallel sessions queue on one lock for up to 30 seconds, then fail with a
  clear message.

## Files

```
~/.optmem/
  memo(.exe)    the tool: one static binary
  memory/
    LOG.txt     every memory, one per line, append-only, never edited
    TREE/       the summaries: a cache, rebuildable from the log alone
    PINS.txt    the pinned facts, written by `memo pin`/`unpin`
    config      the sizes, written by `memo config`
```

```sh
memo config                  # show the sizes
memo config WAKE_LINES=300   # how many lines wake prints (96 ≈ 8k tokens)
memo config WAKE_LINES=      # back to the default
```

`WAKE_LINES` is the only size worth touching, and it is a reading budget, not
a storage budget: change it whenever, in either direction, and nothing is
recomputed.

Records are fixed width, so position *is* identity and every lookup is one
seek. Measured at 100,000 memories on Windows: `wake` 38 ms, `note` 44 ms,
`recall` 68 ms, `import` of all 100k 117 ms. Process startup is most of
that.

Set `$MEMORY_DIR` to keep `memory/` elsewhere — a synced folder, a git repo.

## The prompt

This is what the installer prints, and the whole of the integration.

```markdown
## Memory

Your memory is OptMem:
- The tool is `~/.optmem/memo`
- Your memories are in `~/.optmem/memory`

OptMem outlives every session, compaction, model and vendor change.
Without it you do not know who you are, or what was decided and tried.

### At startup: activating OptMem (mandatory)

Run `~/.optmem/memo wake` before any other tool call, in every session, and
then do exactly what it prints, to the end of its output.

### While working: register memories (mandatory)

Call `~/.optmem/memo note "<1 line, max 280 bytes>"` whenever you learn
something new, or something worth keeping happens. That covers a task
worth real effort, a fact or insight the user teaches you, anything you
learn about their life (even indirectly), any event of lasting effect.

A note with quotes, `%` or `^` in it goes through stdin: `~/.optmem/memo note -`.

Do not register redundant memories. If an old memory is wrong, correct it
with `~/.optmem/memo amend <id> "..."`. Facts that must never fade go in
`~/.optmem/memo pin "..."`.

If `~/.optmem/memo note` asks a compression: do it before your next action.

Never edit or delete anything under `~/.optmem/memory`: the tool manages it.

### When you need an old memory: search, or navigate

`~/.optmem/memo recall <regex>` searches every memory, word for word.

Your memories also form a binary tree: #0-1, #2-3 ... exist as one-line
summaries, pairs of those as #0-3, and so on -- every `#a-b` line wake
prints is one node of it. `~/.optmem/memo zoom <a-b>` opens a node into its
two halves, down to the raw memories.

### If you're a subagent: skip everything above

Parallel sessions on this machine are all you, and may all write memories.
A subagent is not: it must never run `memo`, because it cannot judge what
is already known, and its notes would arrive duplicated and incorrectly.
When you spawn one, write: `You are a subagent. Don't run memo.`
```

## Development

```sh
cargo test        # unit tests + the CLI suite (a synthetic life of 2000 memories)
cargo clippy --all-targets -- -D warnings
```

The tests run the real binary against scratch stores, with `MEMORY_DIR`, `HOME`
and `USERPROFILE` all pointing away from your memory.
