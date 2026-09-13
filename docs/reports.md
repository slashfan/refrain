# Reports, monitoring and thresholds

The dashboard is for the incident you are in. This page is for everything else:
a summary at the end of a cron job, a JSON snapshot for a collector, a threshold
that fails a build, a window cut around the minute that interests you.

[Back to the README](../README.md).

## Rotated logs

As soon as you go back to yesterday — the very case of a post-mortem — the file
is called `prod.log.1.gz`. refrain reads those as they are, decompressed on the
fly, with no temporary file:

```bash
refrain --summary var/log/prod.log.2.gz var/log/prod.log.1.gz var/log/prod.log
```

Detection is done on **the header, not the extension**: a gzipped `.log` is
recognised, a `.gz` that is not one is read as plain text. Multi-member archives
— what a `cat a.gz b.gz` produces — are read to the end.

A compressed file is closed by nature: there is nothing to follow, and no
rotation to watch for. It is read in full and then the source ends, while the
others keep being followed. `-n` is still honoured: full decompression is
unavoidable, but only the last N lines are kept, and memory stays bounded.

A pipe given as FILE — a FIFO, a process substitution, `/dev/stdin` — is read
as a stream, like `-`: to its end, gzipped or not, with `-a` and `-n` having
nothing to seek to. `refrain --summary <(zcat a.gz b.gz)` and
`cat prod.log.1.gz | refrain -` both work.

## Bounding the analysis in time

In a post-mortem the question is never "the last hundred thousand lines", it is
"since 2:30pm". `--since` and `--until` bound what gets **counted**:

```bash
refrain --summary --since 15m                  var/log/prod.log
refrain --summary --since 14:30 --until 15:00  var/log/prod.log
refrain --json    --since 2026-09-09T14:30:00  var/log/prod.log
```

Both accept a duration counted back from start-up (`30s`, `15m`, `2h`, `3d`) or
a date: `2026-09-09T14:30:00+02:00` with its offset, `2026-09-09 14:30` or
`2026-09-09` without — the machine's is then used — and `14:30` for today, which
is what you type in the middle of an incident.

A line outside the window weighs **nowhere**: not in the totals, not on the time
axis, not in the quantiles. Otherwise `--since 15m` would return a p95 computed
over the whole day. It is not counted as "skipped" either — `skipped` is there to
spot a format problem, not a filter doing its job. Reports say how many lines
were dropped, which keeps you from mistaking too narrow a window for an
application at rest:

```
0 entries analysed (0 skipped), 0 errors
window   : 23,116 lines dropped outside the bounds
```

`--since` implies reading the file from the start — following from the end would
show nothing until a new line arrives. On a forty-gigabyte `prod.log`, `-n`
remains the cost guard: `--since 15m -n 100000` re-reads only the tail of the
file, then keeps only the requested quarter of an hour.

## Responses, not log levels

`ERROR` and above is a **logging** decision, not the fate of a request. An
exception caught and logged at `info` still returned a 500; a hundred 404s on
`/favicon.ico` are not an outage. So when the application logs a status —
the `kernel.terminate` subscriber in [what refrain needs from your
application](symfony.md#1-a-duration-field-in-the-context-recommended) already
does — refrain counts responses
by class:

```
status   : 2xx 4,102 · 4xx 91 · 5xx 58 — 1.36 % 5xx
```

The **Endpoints** tab gains a `5xx` column beside `Err.`, Overview a
2xx/3xx/4xx/5xx block, and the JSON a `status` object plus `status_4xx` /
`status_5xx` per endpoint. The threshold follows:

```bash
refrain --summary --fail-if '5xx-rate>1%' --fail-if '5xx-rate:api_orders_list>0.5%' var/log/prod.log
```

Recognised keys: `status`, `status_code`, `http_status`, `response_code` — as a
number or as a string. A value outside 100–599 is some other field of the same
name, and is ignored.

The denominator is the number of lines **carrying a status**, not the number of
requests: that is the population the information exists for. With no status ever
read, the block does not appear, and the threshold stays silent rather than
reporting a reassuring zero.

## Deprecations

Symfony's `ErrorHandler` logs every deprecation on the `php` channel at `INFO`
— `User Deprecated: Since symfony/http-foundation 6.2: Calling "…" is
deprecated` — with an `ErrorException` in the context pointing at the
deprecated code. A log is the one place they all show up before an upgrade,
where the profiler shows them one request at a time. refrain groups them:

```
Deprecations (312 lines, 2 distinct)
      241 × Since symfony/http-foundation #.#: Calling "…" is deprecated, use "…" instead.
          /var/www/vendor/symfony/http-foundation/Request.php:1290 — last from app_search
       71 × The "…" template is deprecated, extend "…" instead.
          /var/www/var/cache/prod/twig/3f/3f8c0e2b7a1d9c4e5f6a7b8c9d0e1f2a.php:58 — last from app_checkout
```

The key is the message **and** the origin, both normalised the way an error
signature is — digits to `#`, quoted strings to `"…"`. The route is not in it:
one deprecated call reached from twenty routes is one thing to fix, not twenty,
and the row shows the route that triggered it last as a hint about where to
look. The origin is what tells two deprecated classes apart once their names
are folded, and its line number folds with the rest, so a deployment in the
middle of the file does not split a row in two. The **Deprecations** tab shows
the same table, with the latest message as it was written, identifiers
included; the JSON carries a `deprecations` list and a `totals.deprecations`
count.

Getting them into the file at all is a Monolog matter — in production the
handler usually never lets `INFO` through — see [tracking
deprecations](symfony.md#tracking-deprecations).

## Failing a job on a threshold

A report from cron or CI is worthless if you have to read it to learn that
things are going badly. `--fail-if` returns **3** as soon as a threshold is
crossed:

```bash
refrain --summary \
  --fail-if 'error-rate>2%' \
  --fail-if 'p95>1s' \
  --fail-if 'p95:api_orders_list>800ms' \
  var/log/prod.log
```

```
refrain: threshold crossed — error-rate = 8.60 % > 2.00 %
refrain: threshold crossed — p95 (api_orders_list) = 3.35 s > 1.00 s
```

Crossed thresholds go to standard error, one per line: the report itself stays
usable through a pipe.

The grammar is deliberately narrow — `metric comparator value`:

| | |
| --- | --- |
| **Metrics** | `error-rate`, `request-error-rate`, `5xx-rate`, `errors`, `deprecations`, `entries`, `nplus1`, `p50`, `p95`, `p99`, `max` |
| **Comparators** | `>`, `>=`, `<`, `<=` |
| **Units** | `%` for a rate, `ms` or `s` for a duration; with no unit, a duration is in milliseconds and a rate is a fraction (`0.02` = `2%`) |

### Which error rate

`error-rate` is the share of **lines** that are errors. It therefore depends on
what you hand refrain to read — and this page tells you to hand it more: add
`doctrine.log` so N+1 patterns can be detected, and dozens of DEBUG lines per
HTTP request join the denominator. On the same 400 requests:

```
error-rate         = 0.56 %      ← a 2% threshold stays silent
request-error-rate = 6.75 %      ← same errors, divided by requests
```

`request-error-rate` divides errors by HTTP requests instead — the definition
the `Err.` column already uses — so it does not move when a file is added. That
is the one to hold on to in CI; `error-rate` answers a different question, "how
noisy is this log", and remains what it always was.

The two do not count the same errors. `error-rate` counts every error line,
because every one of them is noise in the file. `request-error-rate` leaves out
what no request could have raised — a failing command logs on the `console`
channel, and a nightly job going wrong has nothing to say about your endpoints.
The JSON gives that numerator as `totals.request_errors`, beside
`totals.errors`. A command's errors are set aside from the denominator, not
from the report: they stay in the error table, in the summary and in the JSON.

Both count error **lines**: a request that logs three errors weighs three, and
the same exception written at `ERROR` then at `CRITICAL` weighs two. So
`request-error-rate` can pass 100 % — more error lines than requests is a fact
about the log, not a defect of the figure. And when no request was seen at all
— no `Matched route`, no duration field — `request-error-rate` stays silent
rather than reporting a reassuring zero; so does `error-rate` when no line was
analysed at all.

`5xx-rate` takes an endpoint like a quantile does — `5xx-rate:api_orders_list`
— since it is counted per route. With no endpoint it is global, like the other
two rates.

A quantile with no endpoint applies to **the worst of them all**: "no route may
go over one second at p95" is what you mean in CI, and the message names the
culprit. `p95:api_orders_list` targets one precise route; if it does not appear
in the logs, the threshold stays silent rather than inventing a zero that would
make it look respected.

### Failing a build on an N+1

The N+1 detector is only useful where someone looks at the SQL tab, and in CI
nobody does — which is precisely where an N+1 is cheapest to catch. Run the
functional test suite with Doctrine logging to a file, as [detecting N+1
queries](symfony.md#detecting-n1-queries) sets it up, then:

```bash
refrain --summary --fail-if 'nplus1>0' var/log/test.log
```

```
refrain: threshold crossed — nplus1 = 2 > 0
```

`nplus1` counts the patterns detected — the rows of the SQL tab, one per
endpoint and query — and the summary above the message lists them. With an
endpoint, `nplus1:api_orders_list>0`, it counts that route's patterns only;
a route that was seen and has none answers zero, a route never seen answers
nothing. `--nplus1 N` still sets what counts as a pattern, so the two options
agree: raise it to 20 and a query run twelve times is no longer one.

With no SQL query read at all — `doctrine.log` not handed over, Doctrine not
logging — the threshold stays silent rather than passing the build on a
reassuring zero, the same rule as `5xx-rate` with no status. And `--nplus1 0`
switches the detection off, which no threshold can then cross: the two
together are refused at start-up as a faulty command line.

### Failing a build on a deprecation

Run the test suite with deprecations logged to a file, then:

```bash
refrain --summary --fail-if 'deprecations>0' var/log/test.deprecations.log
```

```
refrain: threshold crossed — deprecations = 312 > 0
```

`deprecations` counts **lines**, the way `errors` does and the way Symfony's
PHPUnit bridge counts its `max[total]` — not distinct notices; the summary
above the message lists those. Like `errors`, a log with none in it answers
zero: that is the very thing the threshold is there to certify. It takes no
endpoint.

A malformed threshold is refused **at start-up**, not after reading forty
gigabytes — and with exit code 2, which sets it apart from a threshold genuinely
crossed. `--fail-if` only makes sense on a report that ends: it is refused with
`--every`, and in the dashboard.

## JSON output (monitoring)

`--json` replaces the dashboard with a JSON object, to plug refrain into a
metrics pipeline.

A one-shot reading, typically from cron or CI:

```bash
refrain --json var/log/prod.log > /var/lib/metrics/refrain.json
```

A continuous stream, one object per line (NDJSON), never re-reading the file
from the start:

```bash
refrain --json --every 30 var/log/prod.log
```

The `totals` and `levels` counters are **cumulative** since start-up, the way a
Prometheus counter is: it is up to the collector to take the differences from
one reading to the next. `throughput` additionally provides sliding-window
rates, usable without keeping any state.

`--top N` limits the `errors`, `deprecations` and `endpoints` lists — 25 by
default, `0` for all of them.

Exit codes tell the causes apart, so a job knows what it is dealing with:

| Code | Cause |
| --- | --- |
| **0** | all is well |
| **1** | a source could not be read |
| **2** | the command line is at fault |
| **3** | a `--fail-if` threshold was crossed |

The **1** keeps a cron job from letting a zeroed snapshot pass for "all is
well". And if a source is missing, it takes precedence over the thresholds:
incomplete figures let you assert nothing.

A consumer that goes away is none of those: `| head -1`, a collector that
restarts. refrain stops writing, says nothing, and exits **0** — the logs were
read, there is simply nobody left to tell.

<details>
<summary>Shape of a snapshot</summary>

```json
{
  "generated_at": "2026-09-09T00:52:11.482913+02:00",
  "window": { "first_seen": "…", "last_seen": "…", "span_seconds": 12.418 },
  "totals": {
    "entries": 4600, "skipped": 0, "errors": 58, "error_rate": 0.0126,
    "requests": 400, "request_errors": 58, "request_error_rate": 0.145,
    "deprecations": 43, "out_of_window": 0
  },
  "levels": { "debug": 2826, "info": 1600, "warning": 46, "critical": 58, "…": 0 },
  "status": {
    "responses": 4263, "1xx": 0, "2xx": 4102, "3xx": 12, "4xx": 91, "5xx": 58,
    "rate_5xx": 0.0136
  },
  "throughput": {
    "peak_per_second": 907,
    "peak_at": "2026-09-09T00:52:05+02:00",
    "last_5s_per_second": 517.0,
    "last_60s_per_second": 76.7
  },
  "duration_source": { "kind": "field", "key": "duration_ms" },
  "open_requests": 12,
  "capped": [],
  "sql": { "shapes": 6, "nplus1_threshold": 10 },
  "channels": [{ "channel": "doctrine", "count": 2026, "errors": 0 }],
  "errors": [
    {
      "signature": "ConnectionLost: Uncaught PHP Exception …",
      "count": 13,
      "level": "critical",
      "channel": "request",
      "exception": "Doctrine\\DBAL\\Exception\\ConnectionLost",
      "endpoint": "app_login",
      "first_seen": "…",
      "last_seen": "…",
      "message": "Uncaught PHP Exception …"
    }
  ],
  "deprecations": [
    {
      "signature": "Since symfony/http-foundation #.#: Calling \"…\" is deprecated, use \"…\" instead.",
      "count": 32,
      "channel": "php",
      "origin": "/var/www/vendor/symfony/http-foundation/Request.php:1290",
      "endpoint": "app_search",
      "first_seen": "…",
      "last_seen": "…",
      "message": "User Deprecated: Since symfony/http-foundation 6.2: Calling \"Symfony\\Component\\HttpFoundation\\Request::getContentType()\" is deprecated, use \"getContentTypeFormat()\" instead."
    }
  ],
  "endpoints": [
    {
      "endpoint": "api_orders_list",
      "requests": 117,
      "errors": 10,
      "error_rate": 0.0855,
      "responses": 117,
      "status_4xx": 0,
      "status_5xx": 10,
      "timed": 117,
      "p50_ms": 881.8,
      "p95_ms": 2908.7,
      "p99_ms": 3728.9,
      "max_ms": 11800.8,
      "avg_ms": 1291.88,
      "queries_avg": 29.1,
      "queries_max": 57
    }
  ],
  "nplus1": [
    {
      "endpoint": "api_orders_list",
      "sql": "SELECT t0.id, t0.email FROM customer t0 WHERE t0.id = ?",
      "requests_affected": 14,
      "max_per_request": 57,
      "avg_per_request": 30.7,
      "last_seen": "…"
    }
  ]
}
```

`duration_source.kind` is `field`, `correlation` or `none`: the collector then
knows whether the latencies are exact or merely a floor — see [Measuring
durations](symfony.md#measuring-durations).

`status.rate_5xx` is `null` when no status was read at all — the same rule as
`request_error_rate`.

`capped` lists the tables that have stopped taking new keys — `routes`,
`errors`, `deprecations`, `channels`, `sql shapes`, `n+1 patterns`, `open
requests`. Empty
means everything below is complete; a name in it means that list is a subset,
and the counters above it are still exact.

`request_error_rate` is `null` when no HTTP request was seen: nothing to divide
by, and a zero would read as good news (see above).

`peak_per_second` is the busiest second of **everything read**, not of some
recent window: on a file covering a whole day, the peak of that day — and the
same number whether those lines sit in one file or in several, since each
second is counted exactly, over a span of up to 48 days. The
sliding rates next to it — `last_5s_per_second`, `last_60s_per_second` — are
the ones that describe the present. In the dashboard, `r` resets the peak along
with the rest of the counters.

</details>

## Options

```
refrain [OPTIONS] <FILE>...

  <FILE>...                 files to follow; ".gz" read as is, "-" reads
                            standard input, a pipe is read as a stream
  -a, --from-start          read from the start rather than from the end
  -n, --lines <N>           re-read the last N lines on start-up
  -l, --min-level <LEVEL>   initial stream level [default: debug]
      --since <WHEN>        only count from there on (15m, 14:30, a date)
      --until <WHEN>        only count up to there
      --summary             no dashboard: read to the end, then summarise
      --json                JSON output instead of the dashboard
      --every <SEC>         with --json: one NDJSON snapshot every SEC seconds
      --fail-if <THRESHOLD> fail (code 3) if the threshold is crossed; repeatable
      --top <N>             errors, deprecations, endpoints in JSON [25; 0 = all]
      --nplus1 <N>          N+1 detection threshold [10; 0 disables]
      --duration-key <KEY>  key carrying the duration
      --duration-unit <U>   auto | ms | s | us [default: auto]
      --correlate-key <KEY> key identifying one request
      --no-correlate        disable correlation
      --correlate-timeout <SEC>  idle time before a request is closed [5]
      --tick-ms <MS>        refresh period [250]
      --scrollback <N>      entries kept in the stream [2000]
```

Several files at once, each on its own thread:

```bash
refrain var/log/prod.log var/log/worker.log
```

From a remote machine, without installing anything there:

```bash
ssh prod 'tail -f /srv/app/var/log/prod.log' | refrain -
```
