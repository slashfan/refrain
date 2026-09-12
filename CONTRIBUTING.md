# Contributing to refrain

## The rule

**Nothing reaches `main` except through a pull request with green CI.** No
direct commit, no direct push — one-line fixes included.

This is not ceremony: CI builds in debug (where Rust checks integer overflow),
replays the whole test suite, runs clippy without indulgence and checks that
release binary starts. That is the net a direct commit goes around.

## The loop

1. **An issue first.** It carries the why, the leads, and the "done when"
   criterion. If a piece of work has none, open it before writing code.
2. **A branch from `main`**, named after the work, lowercase and hyphenated:
   `multi-source-correlation`, `pull-request-rule`. No `feat/` or `fix/`
   prefix — the issue already says what it is.
3. **A pull request** attached to its milestone, referencing its issue
   (`Closes #12`).
4. **Green CI**, then a *rebase* merge: history stays linear, and every commit
   keeps its message.

```bash
git switch -c my-work main
# … code, tests …
cargo test && cargo clippy --all-targets && cargo fmt --check
git push -u origin my-work
gh pr create --milestone "v0.5.0 — Windows and thresholds"
gh pr checks --watch
gh pr merge --rebase --delete-branch
```

## The guards

`main` carries a protection rule on GitHub: the four CI checks must pass —
Linux, macOS, format and clippy, version — the branch must be up to date before
merging, history stays linear, and neither force-push nor deletion is allowed. That is the barrier — it holds whatever
anyone's clone is configured to do.

A versioned hook refuses the push before it leaves your machine. Install it
once per clone:

```bash
git config core.hooksPath .githooks
```

It enforces nothing the server does not already enforce; it saves the round
trip, and catches the absent-minded gesture where it happens. `--no-verify`
goes around it, and it only exists on the machines where it is installed —
which is fine, since it is no longer the thing standing between a mistake and
`main`.

Until September 2026 it was: GitHub cannot protect a branch on a private
repository under a free account (the API answers `403` on branch protection as
on rulesets), so the hook and discipline were all there was.

## The language

**Everything is in English**: the code, the comments, the test names, the
commit messages, these documents, and what a user sees — command-line help,
tabs, summaries, JSON, error messages.

Until September 2026 it was not: comments and commit messages were French, and
a `README.fr.md` shadowed the README. That was tenable for a private repository
written by one person; it stopped being so the day the code became readable by
everyone. The git history keeps its French, and that is fine — it is a record,
not a document to maintain.

Any displayed string that changes must have its counterpart in the README, and
the demo GIF is remade — the interface it shows has changed.

## Writing a commit

The title says what changes, as an infinitive or a noun phrase:

```
Correlation: pace the sweep on the furthest-behind source
```

The body says **why**, with figures when there are any — that is what makes the
history readable in six months:

> Over the same 23,299 lines, depending on whether they sit in one file or two:
> SQL/req 28.8 against 7.3.

One commit per idea. Two unrelated fixes make two commits, and often two pull
requests.

## How the code is laid out

| File | Role |
| --- | --- |
| [`src/lib.rs`](src/lib.rs) | the library: everything but the wiring |
| [`src/main.rs`](src/main.rs) | main loop, thread wiring |
| [`src/cli.rs`](src/cli.rs) | command-line options (clap) |
| [`src/event.rs`](src/event.rs) | single event channel, keyboard and clock threads |
| [`src/tail.rs`](src/tail.rs) | following files: rotation, truncation, partial line, gzip |
| [`src/parser.rs`](src/parser.rs) | one raw line → `LogEntry` |
| [`src/stats.rs`](src/stats.rs) | aggregation: time axis, quantiles, correlation |
| [`src/app.rs`](src/app.rs) | application state and reaction to keys |
| [`src/ui.rs`](src/ui.rs) | ratatui rendering |
| [`src/threshold.rs`](src/threshold.rs) | `--fail-if` thresholds: grammar and verdict |
| [`src/export.rs`](src/export.rs) | exporting the selection: report, file, OSC 52 |
| [`src/bin/genlogs.rs`](src/bin/genlogs.rs) | fake Symfony log generator |
| [`src/bin/bench.rs`](src/bin/bench.rs) | throughput benchmark |

The overall shape:

```
   tail thread(s) ──┐
   keyboard thread ─┼──► mpsc channel ──► main loop ──► ratatui
   clock thread ────┘                    (app: decides)  (ui: draws)
```

A single thread touches the state: no locks, all concurrency goes through the
channel. Reading and parsing run alongside rendering.

A single thread touches the state: no locks, all concurrency goes through the
channel. Reading and parsing run alongside rendering.

The unit tests cover the parser, file following (rotation, truncation, partial
line, gzipped log including multi-member archives, invalid UTF-8 byte), the
aggregation — including every memory ceiling and the synchronisation between
several files read in parallel — N+1 detection, and rendering, that one through
ratatui's test backend, including on a tiny terminal, while a search is being
typed and while an endpoint is followed — and the export, down to the base64
encoding of the OSC 52 sequence. The parser is further exercised on seventeen
thousand twisted lines — every possible truncation, then fixed-seed mutations —
which it must survive without panicking.

The end-to-end tests ([`tests/cli.rs`](tests/cli.rs)) run the real binaries and
plug them into each other: generation, analysis, piping through standard input,
that same pipe closed from the other end, reading the last lines, time window
and thresholds over files with known values, reading a log compressed by the
system's `gzip`, spreading generated logs, JSON validity and exit codes.

Their number is deliberately not written down here. It used to be, in four
places at once, and every pull request that added a test had to find all four.

## What CI checks

| Job | Contents |
| --- | --- |
| `Tests · ubuntu-latest`, `Tests · macos-latest` | `cargo build --all-targets`, `cargo test`, release build, `refrain --version`, throughput guard |
| `Format and clippy` | `cargo fmt --check`, `cargo clippy -- -D warnings` |
| `Version` | the version does not move back below the latest release |

Both systems run on every pull request. While the repository was private they
did not — a macOS minute was billed ten times the Linux rate — and rotation
detection, which reads the inode, was only exercised after merging.

Everything CI checks runs locally, and faster than waiting for a runner:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

And if the change touches the hot path — the parser, the aggregation — the
benchmark says what it costs:

```bash
cargo run --release --bin bench
```

CI runs a reduced version of it as a guard. Its floor is deliberately very low:
GitHub runners are far too variable for a tight threshold, and the accident
worth catching is a factor of ten, not ten percent.

## Code that will be reread

- **Comments explain why**, not what. The code already says what it does; what
  it does not say is why this bound, why this ceiling, why this trade-off. The
  project is written that way end to end.
- **A fixed behaviour comes with its test.** Without it, nothing stops the
  regression from coming back. Test names state the rule they check:
  `the_route_ceiling_stops_the_table_without_stopping_the_counters`, not
  `test_routes`.
- **Memory stays bounded.** Every table indexed by a key coming from the logs
  has a ceiling (`src/stats.rs`): that is what lets refrain swallow 40 GB
  without moving. A ceiling that is reached says so rather than silently
  truncating what it shows.
- **One thread touches the state.** Concurrency goes through the `mpsc`
  channel, never through a lock.

## Remaking the README demo

The header GIF ages with every interface change. It is remade in one command,
from a versioned scenario:

```bash
brew install asciinema agg gifsicle   # expect ships with macOS
cargo build --release
./docs/demo.sh                        # writes docs/demo.gif
```

`docs/demo.exp` describes the key sequence, `docs/demo.sh` prepares the logs
and builds the GIF. The corpus is generated from a fixed seed: two takes give
the same figures on screen, and a diff reflects only what really changed.

The original issue called for `vhs`, the more usual tool for this. It was
dropped after trying: vhs captures its frames from the canvas layers of
xterm.js, served by ttyd and driven by a headless Chrome, and with the current
versions of those three it captures nothing at all — empty frame directory,
no GIF, not one error message. The chain kept here needs no browser.

The GIF weighs a few hundred kilobytes and lives in git history forever: if the
scenario grows, check its weight before committing.

## Publishing a version

**The version in `Cargo.toml` commands.** Publishing means raising it in a pull
request like any other change; the merge does the rest — tag, build of the
three targets, release, checksums.

```bash
git switch -c version-0.4.0 main
# raise `version` in Cargo.toml, then reflect it in Cargo.lock
cargo update --workspace
git commit -am "Version 0.4.0"
```

There is **no tag to push**: `gh release create` creates it itself on the merge
commit. The manual gesture that could be forgotten is gone, and with it the
drift between what the binary announces and what is published.

Three guards, each on a real failure mode:

| What could happen | What catches it |
| --- | --- |
| Raising the version without updating `Cargo.lock` | `cargo build --locked` in CI |
| Moving the version back below the latest release | the **Version** job, on the pull request |
| Pushing a tag that does not match `Cargo.toml` | the **Version to publish** job, before any build |

A merge that does not touch the version publishes nothing: the workflow sees
the tag already exists and stops without building. A hand-pushed tag is still
accepted — to republish — but it must match `Cargo.toml`.

The release job can also be dispatched by hand (`workflow_dispatch`): it builds
the three targets and drops the binaries as artifacts, publishing nothing. That
is how to exercise the matrix without committing to a version.
