# AGENTS.md

This file provides guidance to coding agents — Claude Code (claude.ai/code) and any
other tool that reads `AGENTS.md` — when working with code in this repository.
`CLAUDE.md` is a symlink to this file: one source, no drift.

## Two documents are authoritative

- **[CONTRIBUTING.md](CONTRIBUTING.md)** — the procedure: issue, branch, pull
  request, *rebase* merge. **Nothing reaches `main` except through a pull
  request with green CI**, one-line fixes included. The `.githooks/pre-push`
  hook is the reminder; it is installed once per clone:
  `git config core.hooksPath .githooks`. It also carries the "How the code is
  laid out" table giving the role of every file, and what the tests cover.
- **[README.md](README.md)** — what the tool does and its known limits, for
  someone deciding whether to use it. The reference pages sit under `docs/`:
  [`docs/symfony.md`](docs/symfony.md) for what refrain needs from the
  application, [`docs/reports.md`](docs/reports.md) for everything that is not
  the dashboard — thresholds, JSON, time windows, the full option list.

This file repeats neither of them: it says what you need in mind before writing
the first line.

## The language is not a detail

**Everything here is in English** — the code, the comments, the test names, the
commit messages, the documents, and what a user sees: `clap` help, tabs,
summaries, JSON, error messages.

It was not always so: until September 2026 the comments and commit messages
were French, and a `README.fr.md` shadowed the README. That made sense for a
private repository written by one person, and stopped making sense the day the
code became readable by everyone. The git history keeps its French; nothing
else does.

Any displayed string that changes must be reflected in the README, and the
header GIF is remade (`./docs/demo.sh`) if the interface moved.

## Commands

```bash
cargo test                       # unit tests and end-to-end tests
cargo test the_route_ceiling     # a single test, by name
cargo test --test cli            # only the end-to-end tests
cargo test --lib stats::         # only one module's tests
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check

cargo run -- var/log/prod.log                # the dashboard
cargo run --bin genlogs -- --rate 0 --count 5000 sample.log   # test logs
cargo run --release --bin bench              # the throughput benchmark
cargo run --release --bin bench -- --requests 20000 --min 100000   # what CI runs
```

Rust 1.88 minimum (ratatui 0.30 requires it), 2024 edition. CI builds with
`--locked`: raising a version in `Cargo.toml` requires `cargo update --workspace`
in the same commit.

The end-to-end tests run the real binaries through `CARGO_BIN_EXE_*`:
`cargo test` is enough, Cargo builds them for it.

## Four architectural invariants

1. **A library, then binaries** — not a single binary. That is what lets
   `src/bin/bench.rs` call `parse_line` and `Stats::ingest` directly, to say
   which of the two costs what.
2. **One thread touches the state.** File following, the keyboard and the clock
   *push* their events into the `mpsc` channel in `src/event.rs`; the main loop
   reads them. There is no lock in this project — do not introduce one.
3. **`app.rs` decides, `ui.rs` draws.** The sorted tables are recomputed once
   per clock tick, never per frame: that is what keeps rendering nearly free at
   100,000 lines per second.
4. **Memory is bounded.** Every table indexed by a key coming from the logs has
   a ceiling (`src/stats.rs`: `MAX_ROUTES`, `MAX_ERRORS`, `MAX_CHANNELS`,
   `MAX_SQL_SHAPES`, `MAX_OPEN_REQUESTS`, `MAX_NPLUS1`). A ceiling that is
   reached stops **detailing**, never **counting** — and says so, through
   `Stats::capped`. Each has its test. That is what lets refrain swallow 40 GB
   without the footprint moving.

## Three modes, one reading chain

`src/main.rs` wires three paths over the same `tail` → `parser` → `stats`:
`run_tui` (the default), `run_report` (`--summary`, or `--json` without
`--every`) and `run_json_stream` (`--json --every`, NDJSON). A reading
behaviour that changes concerns all three.

## The exit codes are a contract

| Code | Cause |
| --- | --- |
| 0 | all is well |
| 1 | a source could not be read |
| 2 | the command line is at fault (returned by `clap`) |
| 3 | a `--fail-if` threshold was crossed |

An unreadable source **takes precedence** over a crossed threshold: without
reading everything, the figures mean nothing, and a job must tell "the
application is unwell" from "refrain could not read anything". The 3 exists
because `clap` already occupies 2. A consumer closing the pipe is none of
those — it is a normal end, and exits 0. These codes are checked by
`tests/cli.rs`: changing them breaks cron jobs.

## Numbers must mean one thing

Several figures once described only the end of the file while claiming to
describe the file: the peak covered the last ten minutes, the quantiles the
last 1024 requests per endpoint. They now cover everything read — or everything
since `r` in the dashboard. Keep it that way: a figure whose window depends on
where you look is worse than no figure.

Same rule for denominators. `error-rate` is over all lines and therefore moves
when you hand refrain another file; `request-error-rate` is over requests and
does not; `5xx-rate` is over responses carrying a status. Each is named after
what it divides by, and none of them invents a zero when its denominator is
empty — it stays silent instead.

Two reads of the same files must produce the same report, row order included:
hash-map iteration is not stable, so every sort breaks ties by name.

## What CI checks

Linux **and** macOS on every pull request, plus formatting, clippy without a
warning, that the release binary starts, and that throughput has not collapsed.
Until the repository went public, macOS only ran at merge time — billed minutes
— which left rotation detection (the inode) and the clipboard (OSC 52) unchecked
before merging. It no longer does.

## Writing code that will be reread

- **Comments say why**, not what: why this bound, why this ceiling, why this
  trade-off. The project is written that way end to end, including the module
  headers (`//!`) that explain each file before you read it.
- **A fixed behaviour comes with its test**, and test names state the rule they
  check — `the_peak_covers_the_whole_file_not_the_last_window`, not `test_peak`.
- **The parser must never panic**: it is exercised on seventeen thousand twisted
  lines — truncations at every position, then fixed-seed mutations.
