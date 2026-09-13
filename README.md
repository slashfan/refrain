# refrain

[![CI](https://github.com/slashfan/refrain/actions/workflows/ci.yml/badge.svg)](https://github.com/slashfan/refrain/actions/workflows/ci.yml)

Real-time **Symfony / Monolog** log analyser for your terminal.

It follows one or more log files the way `tail -f` does, parses them on the fly
and shows a dashboard: errors grouped by type, slowest endpoints, traffic peaks.

Your logs have a refrain: the same error, the same SQL query, over and over.
That is what it looks for.

> **A weekend project — not to be used in production, by any means.**
>
> One person writes this, on weekends, for the pleasure of it. There is no
> support, no compatibility promise from one version to the next, and nobody has
> run it at scale but its author.
>
> refrain reads logs and never writes to them, so it will not break your
> application. The risk is elsewhere: trusting a figure it prints for a decision
> that matters, or wiring it into a job something depends on. Everything below —
> the throughput, the exit codes, the JSON — is measured and tested, and that is
> still not the same thing as being production software.
>
> Run it on a copy of your logs, or on a laptop beside production. Not inside it.

![refrain: the dashboard, following one endpoint, its N+1 patterns and searching the stream](docs/demo.gif)

That demo is not a recording someone made once: `./docs/demo.sh` replays a
versioned scenario over a fixed-seed corpus, so it can be remade the day the
interface changes.

## What a diagnosis looks like

A p95 goes wrong. You open the **Endpoints** tab and the slowest route is right
there, with the number of SQL queries it runs per request beside it. `Enter`
puts that route under watch, and the other tabs narrow to it: its **N+1**
patterns, which usually explain the p95 on their own; its **errors**, grouped by
signature; its raw **lines**, filtered. `w` writes the whole thing to a file you
can paste into a ticket — the full stack trace, not the three lines the screen
had room for.

That is the entire tool: four keystrokes from a symptom to something you can
hand to someone else, without retyping a filter.

It reads what Monolog already writes. Errors, channels, volumes and traffic
peaks need nothing from you; durations and N+1 detection need a subscriber and a
processor, and deprecations, outbound HTTP calls and messages on the bus need a
handler that lets `INFO` through — all described in [what refrain needs from
your application](docs/symfony.md).

## Installing

**Unix only** — Linux or macOS. Rotation detection relies on the inode, and
that is a deliberate choice rather than an omission; on Windows, WSL gives you
Linux, where everything works.

Binaries are published with every version:
[Releases](https://github.com/slashfan/refrain/releases).

```bash
# Linux x86_64 — static (musl), no system dependency: it starts on a server with
# an ancient glibc, where a conventional binary would refuse to.
curl -sSL https://github.com/slashfan/refrain/releases/latest/download/refrain-linux-x86_64.tar.gz | tar xz
```

```bash
# macOS Apple Silicon (refrain-macos-x86_64.tar.gz for Intel Macs)
curl -sSL https://github.com/slashfan/refrain/releases/latest/download/refrain-macos-arm64.tar.gz | tar xz
```

The macOS binaries are unsigned. Fetched with `curl` they run without fuss;
downloaded from a browser, quarantine has to be lifted with
`xattr -d com.apple.quarantine refrain`.

Every release carries a `SHA256SUMS` file, checkable with `shasum -c`.

## Building it yourself

Rust 1.88 or newer.

```bash
cargo build --release
```

With a toolchain at hand and no wish to keep the sources around, one command
builds it and puts it on your PATH — no registry involved, since refrain is not
published on crates.io:

```bash
cargo install --git https://github.com/slashfan/refrain --bin refrain --bin genlogs
```

The two `--bin` are worth the typing: without them cargo also installs `bench`,
the throughput benchmark, which is a development tool — the release archives
leave it out for the same reason. Drop `--bin genlogs` too if you only want the
analyser and never the log generator.

With no logs at hand, the `genlogs` binary makes realistic ones:

```bash
cargo run --release --bin genlogs -- --rate 300 var/log/prod.log
```

`--spread 200` dates the requests over the last two hundred seconds instead of
writing them all at this instant: enough to fill the graphs, and enough to try
`--since` out.

And in another terminal:

```bash
cargo run --release -- var/log/prod.log
```

A text summary, no dashboard, for a cron job or CI:

```bash
cargo run --release -- --summary var/log/prod.log
```

`-n` limits it to the tail of the file, without re-reading the forty gigabytes
that precede it:

```bash
cargo run --release -- --summary -n 100000 var/log/prod.log
```

## The eight tabs

| Tab | What it shows |
| --- | --- |
| **Overview** | volume and errors per second (sparklines), breakdown by level and by response class, chattiest channels, top errors |
| **Errors** | errors grouped by signature, with the latest occurrence in full (exception, endpoint, JSON context) |
| **Endpoints** | requests, p50, p95, max, SQL queries and outbound calls per request, 5xx and error rate per route |
| **SQL** | N+1 patterns: the same SQL query repeated within a single HTTP request |
| **Outbound** | calls to third parties grouped by provider: their latency, their statuses, and how many one request makes |
| **Messenger** | messages on the bus grouped by class: dispatched against handled, what is still waiting, failures and lag |
| **Deprecations** | deprecations grouped by message and origin, with the route that triggered each last |
| **Stream** | the latest entries, filterable by level, by pattern and by endpoint |

### Shortcuts

| Key | Effect |
| --- | --- |
| `q` | quit |
| `Esc` | drop the current filter; otherwise quit |
| `Tab`, `←` `→`, `1`–`8` | switch tab |
| `↑` `↓`, `j` `k` | move · `PgUp` `PgDn` by 10 · `g` / `G` start / end |
| `Enter` | follow the endpoint of the selected row (Endpoints, SQL, Outbound, Messenger, Deprecations) |
| `/` | search the stream · `Enter` confirms · `Esc` clears |
| `space` | freeze or resume the stream |
| `s` | change the endpoint sort (p95 → max → requests → errors) |
| `+` / `-` | raise / lower the minimum stream level |
| `w` / `y` | export the selection: to a file / to the clipboard |
| `r` | reset the counters |
| `?` | help |

`/` searches the **message**, the **channel** and the **route** at once, ignoring
case: `doctrine` isolates the SQL queries, `app_login` everything touching that
endpoint, `Connection refused` the incident itself. The pattern stays in the tab
header while it is active, so a forgotten filter never looks like logs having
gone quiet.

### Following an endpoint

`Enter` on a row of the **Endpoints** tab — or on an N+1 pattern in the **SQL**
tab, on an outbound call, on a message class, or on a deprecation — puts that
endpoint under watch: the Errors, SQL, Deprecations and Stream tabs then show
only what concerns it. The Outbound and Messenger tabs are not narrowed, since
a provider is called and a message dispatched from several routes; `Enter`
there follows the endpoint that does it most within one request. The endpoint table itself keeps everyone, since that is
where you choose; the one being followed is marked with a `▸`, and recalled in
the top banner from any tab.

That is the usual path of a diagnosis: a p95 going wrong in Endpoints, its N+1
patterns in SQL, the third party it waits on in Outbound, its errors in Errors,
its raw lines in Stream — without ever retyping a filter.

The attachment goes further than the text of the lines. A Doctrine SQL query
names no route, and neither does an uncaught exception; it is the token they
share with the `Matched route` line that ties them together, and the stream
remembers it. Following `app_orders` therefore brings up its SQL queries, which
nothing in their text tied to it. Without a correlation token, only the lines
carrying a route of their own are kept.

`Enter` again on the same endpoint releases the watch, and so does `Esc`. Esc in
fact peels the filters off one after another — the search pattern first, then
the followed endpoint — and only quits when there is nothing left to peel.

### Exporting what you found

Once you hold the error, you want to paste it into a ticket. `w` writes the
selection to a file in the current directory, `y` puts it on the clipboard:

```
refrain-error-ProductNotFound-20260909-231205.txt
refrain-endpoint-api_orders_list-20260909-231240.txt
refrain-nplus1-api_orders_list-20260909-231302.txt
refrain-outbound-POST-api-payments-test-v2-charges-20260913-094411.txt
refrain-message-IndexEntityMessage-20260913-101902.txt
refrain-deprecation-Request-php-20260912-101512.txt
```

The report stands on its own: what you were looking at, when, from which files,
then the detail. For an error that means **the whole stack trace** — the screen
shows only its first three lines — along with its JSON context. For an endpoint,
its quantiles and the N+1 patterns that most often explain its p95. From the
Overview or the Stream, where nothing is selected, it is the full summary.

`y` goes through the OSC 52 escape sequence: the **terminal** is asked to do the
copying, so the clipboard is the one on the machine you are sitting at, not the
one on the server refrain runs on. It is the only way through an `ssh`, and it
adds no dependency. Not every terminal honours it — Terminal.app ignores it,
tmux wants `set -g set-clipboard on` — hence `w`, which depends on nobody.

## Beyond the dashboard

The same reading chain feeds three other modes: a text summary at the end of a
cron job, a JSON snapshot for a collector, and a threshold that fails a build.

```bash
refrain --summary var/log/prod.log                      # read to the end, then summarise
refrain --summary --since 15m -n 100000 prod.log        # the last quarter of an hour
refrain --json --every 30 prod.log                      # one NDJSON object per interval
refrain --summary --fail-if 'p95>1s' prod.log           # exit 3 when it is crossed
```

Rotated `.gz` logs are read as they are, several files merge into one analysis,
and the exit codes are a contract a cron job can rely on. All of it —
thresholds and their grammar, the JSON schema, time windows, the full option
list — is in [reports, monitoring and thresholds](docs/reports.md).

## How fast, and how that is measured

On a development machine: **≈ 1.9 million lines/s** — 237 MB analysed in
0.64 s — for a few dozen megabytes of memory. That memory is **capped by
design**: a bounded-error histogram for the quantiles, a ring buffer for the
time axis, and a ceiling on every table (routes, error signatures, SQL shapes,
outbound call shapes, message classes, open requests). It therefore varies with what the logs contain, never with the
size of the file: 10 MB or 40 GB, it is the same order of magnitude. **Every one
of those ceilings is covered by a test**: the table stops growing without ever
stopping counting what it already knows.

That figure is meant to be replayed rather than believed:

```bash
cargo run --release --bin bench
```

```
corpus    : 1,208,100 lines, 237.6 MB — /tmp/refrain-bench-100000-g1.log
            (fixed seed: two runs compare)
parser    :    2,527,274 lines/s   (478 ms)
+ aggregate:    1,581,385 lines/s   (764 ms)
```

The benchmark generates its corpus with `genlogs` from a fixed seed, then
measures two things: parsing one line, then parsing **and** aggregating. Reading
the file is deliberately off the clock — it is the CPU being measured, not the
disk. The headline figure is the real binary's, reading included: it beats the
single-threaded measurement because parsing runs in the reading thread while
aggregation runs in the main one.

Measured on an Apple M5 Pro, rustc 1.98.1, `release` profile. On another machine
the numbers will differ; the method will not.

They also move with the tool, not only with the machine. v0.7.0 reads a status,
counts a request and records into a histogram on every line: that costs the
single-threaded measurement some 5 % against v0.6.0, and costs the real binary
nothing — the added work sits on the main thread, alongside a parser that is
busy in the reading one. Two figures moving in opposite directions is the
architecture showing through.

## When not to use it

refrain answers a question you have right now, in a terminal, about logs you can
reach. It keeps nothing: close it and the counters are gone.

If you want to compare this week with last month, alert a team at three in the
morning, or search logs from forty machines at once, you want a log platform —
Kibana, Loki, Datadog — and refrain is not a cheaper one. It is what you run on
the box, or on a copy of the file, when going through that platform would take
longer than reading the log.

And it only knows Symfony and Monolog — the line format and the JSON one, both
described in [recognised formats](docs/symfony.md#recognised-formats). Nginx
access logs, or an application logging in some other shape: refrain counts those
lines as skipped, and says how many.

## Known limits

- A compressed file is not followed: it is read once, in full. That is what it
  is — a closed log.
- Quantiles come from a histogram, not from a sorted sample: they cover
  **everything read** — or everything since `r` in the dashboard — and are exact
  to **±1.6 %**. Each octave is cut into 32 slices, so that bound holds at 1 ms
  as at 10 s; 672 counters per endpoint cover 0.06 ms to 131 s in 2.6 KB.
  `max` is not an estimate: it is tracked exactly.
- Beyond 4096 distinct routes, error signatures or deprecations, new keys are
  no longer recorded (counters already known keep going). Same principle for
  N+1 patterns (1024), retained SQL query shapes (2048), outbound call shapes
  (2048) and message classes (2048). An error whose signature no longer
  fits is still counted in the total: refrain stops detailing, never counting.
  And it says so: `capped: routes` in the banner, a `capped` line in the summary,
  a `capped` list in the JSON. A table that has stopped detailing makes its own
  listing partial, and a route missing from it would otherwise read as a route
  with no traffic.
- A line is cut at one megabyte: its head is parsed, the rest is discarded up
  to the next newline. No Monolog line comes near that — a serialised exception
  with its trace weighs a few hundred kilobytes at the very worst — but a binary
  file handed over by mistake, or a log that has lost its newlines, used to be
  loaded whole. An entry's message is further kept to its first 4,000 characters.
- SQL queries and outbound call shapes are identified by a 64-bit fingerprint
  rather than by their text, so as not to duplicate it in every open request. A
  collision remains theoretically possible, but negligible at this scale.
- A message is counted as handled on the worker's acknowledgement, not on
  `Message … handled by …`, which fires once per handler. Its **lag** needs an
  identifier on dispatch, which core Symfony does not write — with only the
  core lines the counts are exact and the lag is blank. An application running
  an audit middleware as well writes two lines for one dispatch; refrain keeps
  the two vocabularies apart and takes the larger, so a queue never looks twice
  as deep as it is. `waiting` covers the window read, not eternity.
- An outbound call is read from the **response** line Symfony's HttpClient
  writes, and its shape never carries a query string — that is where an API key
  lives. Its verb and its duration come from the info array when the
  application logs one; without it the shape has no verb and no latency, which
  the tab says rather than showing a zero. The raw line in the Stream tab is
  still the raw line: refrain shows logs as they are, and rewrites none.
- A line dated in the future is brought back to the current time for the time
  axis, so that a skewed clock does not empty the graphs.
- Two runs over the same files produce the same report, down to the order of
  the rows: ties are broken by name, never left to the hash table.

## Contributing

Issue, branch, pull request, green CI, rebase merge — the procedure and the
reasons behind it are in [CONTRIBUTING.md](CONTRIBUTING.md), along with the
map of the source files and what the tests cover.

## Licence

[MIT](LICENSE) — © 2026 Nicolas Cabot.

The copyright notice travels with the published binaries: every release archive
contains the `LICENSE` file, as the licence requires.
