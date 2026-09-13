# What refrain needs from your application

refrain reads what Monolog already writes, and nothing here is required to get
a dashboard: errors, channels, volumes and traffic peaks work out of the box.
Two things do need a hand — measuring how long a request took, and detecting
N+1 queries — because Monolog writes neither on its own. Four others —
deprecations, outbound HTTP calls, messages on the bus and cache misses —
Symfony does write; the question is whether your handlers let them through.

[Back to the README](../README.md).

## Recognised formats

Detection happens **line by line**, with no option to pass:

- Symfony's line format (`LineFormatter`):
  `[2026-09-09T10:23:45.123456+02:00] request.CRITICAL: … {"exception":"…"} []`
- the JSON format (`JsonFormatter`), one object per line.

Lines starting with neither `[` nor `{` — typically a multi-line stack trace —
are attached to the previous entry instead of being counted as noise.

A log is not always valid UTF-8: a latin-1 byte from an old library, a binary
blob in an exception message, a character cut in two by a rotation. Lines are
read as bytes and converted without ever failing — a faulty byte costs only the
character it occupies, never the rest of the file.

Errors are grouped by **signature**: the exception class followed by the
normalised message (digits replaced with `#`, quoted strings with `"…"`).
"Product 42 not found" and "Product 1337 not found" therefore count as one and
the same error.

A quoted string that itself contains quotes folds whole. Symfony writes an
exception as `… Exception Foo: "<message>" at <file> line <n>`, and the message
quotes the part that varies:

```
Uncaught PHP Exception NotFoundHttpException: "No route found for "GET https://host/sw.js"" at RouterListener.php line 156
```

Everything from the first quote to the one that closes it is one value, however
many quotes sit inside — otherwise the sentence folds away and the varying path
becomes the key, which is how one missing route came out as one row per URL. A
quote is read as closing its string when what follows it is the end of the
message or a separator; anything else opens a string nested inside. Two values
side by side — `Command "app:import" exited with code "1"` — are not nesting,
and keep the sentence between them.

## Measuring durations

Monolog writes **no duration** by default. refrain knows two ways to get one;
the Endpoints tab always shows which is in use.

### 1. A duration field in the context (recommended)

The most reliable. A subscriber on `kernel.terminate` is enough:

```php
// src/EventSubscriber/RequestDurationSubscriber.php
namespace App\EventSubscriber;

use Psr\Log\LoggerInterface;
use Symfony\Component\EventDispatcher\EventSubscriberInterface;
use Symfony\Component\HttpKernel\Event\RequestEvent;
use Symfony\Component\HttpKernel\Event\TerminateEvent;
use Symfony\Component\HttpKernel\KernelEvents;

final class RequestDurationSubscriber implements EventSubscriberInterface
{
    private float $start = 0.0;

    public function __construct(private readonly LoggerInterface $logger) {}

    public static function getSubscribedEvents(): array
    {
        return [
            KernelEvents::REQUEST => ['onRequest', 4096],
            KernelEvents::TERMINATE => 'onTerminate',
        ];
    }

    public function onRequest(RequestEvent $event): void
    {
        if ($event->isMainRequest()) {
            $this->start = microtime(true);
        }
    }

    public function onTerminate(TerminateEvent $event): void
    {
        $this->logger->info('Request finished', [
            'route' => $event->getRequest()->attributes->get('_route'),
            'method' => $event->getRequest()->getMethod(),
            'status' => $event->getResponse()->getStatusCode(),
            'duration_ms' => round((microtime(true) - $this->start) * 1000, 1),
        ]);
    }
}
```

refrain finds the usual keys on its own: `duration_ms`, `duration`, `elapsed_ms`,
`elapsed`, `response_time`, `execution_time`, `exec_time`, `runtime`,
`request_time`. For a key of your own: `--duration-key total_time`.

The unit is inferred from the suffix (`_ms`, `_s`, `_us`), then from magnitude —
a float below 30 is read as seconds, because that is what `microtime()` gives.
When in doubt: `--duration-unit ms`.

### 2. By correlating a token (without touching your code)

If every line carries a request identifier, refrain measures the gap between the
first and the last line of one request. Monolog's `UidProcessor` is enough to
switch it on:

```yaml
# config/services.yaml
services:
    Monolog\Processor\UidProcessor:
        tags: [monolog.processor]
```

Keys recognised out of the box: `token`, `uid`, `request_id`, `x-request-id`,
`trace_id`. Otherwise: `--correlate-key my_key`.

> **Worth knowing.** This method measures from the first to the last *log line*,
> not from the start to the end of the *request*: since nothing is logged after
> the last line, it **underestimates** the real duration. The ranking of
> endpoints stays right, the absolute values should be read as a floor. For
> exact figures, go through method 1.

With neither of the two, the Endpoints tab remains usable: requests are counted
thanks to the `Matched route` line of the `request` channel, and the error rate
per endpoint stays exact.

## Detecting N+1 queries

An N+1 is the same SQL query run dozens of times within a single HTTP request —
the loop that reloads a related entity on every iteration. The Symfony profiler
shows it in development; in production, nobody sees it go by.

refrain leans on a convenient fact: **Doctrine logs prepared statements**, with
the parameters kept apart in `params`. Two executions of the same N+1 therefore
produce *exactly* the same string — no SQL normalisation to write, equality is
enough.

Two things are needed on the Symfony side:

**1. That Doctrine logs.** Queries land on the `doctrine` channel at `DEBUG`
level, with a `context.sql` field. In production that level is often filtered
out — and that is precisely where N+1s hide. A dedicated handler is enough to
make them visible without drowning `prod.log`:

```yaml
# config/packages/monolog.yaml
monolog:
    handlers:
        doctrine:
            type: stream
            path: '%kernel.logs_dir%/doctrine.log'
            level: debug
            channels: [doctrine]
```

Then hand both files to refrain, which merges them:

```bash
refrain var/log/prod.log var/log/doctrine.log
```

**2. A correlation token**, to know which lines belong to the same HTTP request
— the `UidProcessor` from [Measuring durations](#measuring-durations).

The threshold is set with `--nplus1 N`: an SQL query repeated at least N times
within one HTTP request is reported. 10 by default, `0` disables it.

The **Endpoints** tab gains an `SQL/req` column along the way, the average number
of queries per HTTP request. It is often the first culprit behind a p95 going
wrong:

```
Endpoint            Requests  SQL/req  HTTP/req  p50      p95      max      5xx  Err.
api_orders_list     73        29.1     6.0       912 ms   3.35 s   4.49 s   7    9.6%
app_search          95        2.0      0.9       230 ms   900 ms   1.08 s   0    5.3%
```

## Outbound HTTP calls

Symfony's HttpClient logs every call your application makes to somebody else.
They cost ten to a hundred times what an SQL query does: an endpoint calling a
third-party API four times per request is the N+1 no index will fix, and a
provider whose p95 moved is the explanation for a p95 of your own that moved
with it.

**Getting them into a file.** The calls land on the `http_client` channel at
`INFO` level — filtered out in production like everything else at that level,
so give the channel a handler of its own:

```yaml
# config/packages/monolog.yaml
monolog:
    handlers:
        http_client:
            type: stream
            path: '%kernel.logs_dir%/http_client.log'
            level: info
            channels: [http_client]
```

Then hand the file over with the others, as for `doctrine.log`:

```bash
refrain var/log/prod.log var/log/http_client.log
```

Symfony writes two lines per call — the announcement, then the response:

```
[2026-09-12T10:23:45.123456+02:00] http_client.INFO: Request: "GET https://api.example.com/v1/geocode?q=12+rue&key=sk_live_9f3c2a" [] []
[2026-09-12T10:23:45.338238+02:00] http_client.INFO: Response: "200 https://api.example.com/v1/geocode?q=12+rue&key=sk_live_9f3c2a" 0.214782 seconds [] []
```

Only the **response** counts as a call: it is the one carrying the status, and
counting both would double every figure.

**Getting a duration and a verb.** The bare lines above already give the
provider, its status and how many times one request called it. The duration and
the verb come from the info array `ResponseInterface::getInfo()` hands back —
`total_time` in seconds, `http_method`, `http_code` — which a subscriber can
put in the context:

```php
// src/EventSubscriber/HttpClientSubscriber.php
$info = $response->getInfo();
$this->logger->info(sprintf('Response: "%d %s"', $info['http_code'], $info['url']), [
    'http_method' => $info['http_method'],
    'http_code' => $info['http_code'],
    'total_time' => $info['total_time'],
    'url' => $info['url'],
]);
```

`total_time` is read as **seconds** and nothing else: it is curl's, and curl
measures in seconds — there is no unit to infer here, unlike a duration field
of your own choosing. Without it, the **Outbound** tab still shows the calls,
their statuses and their repetition, with `—` where a latency would be.

### The query string never comes out

A third party's URL is where an API key sits in plain sight — the line above
holds one. refrain groups calls under a **shape**, and that shape is the verb,
the host and the path, with the query string **dropped whole**:

```
GET api.example.com/v1/geocode
```

Dropped and not folded, because folding leaves behind whatever it did not
recognise. The credentials before the host — `https://user:secret@host/…` — go
the same way, and so does the fragment. What is left is folded the way an error
signature is: a path segment that identifies one record rather than naming a
kind of record becomes `#`, so that one customer does not become one row.

```
/v1/customers/4711/orders          →  /v1/customers/#/orders
/users/f47ac10b-58cc-…-0e02b2c3d479 →  /users/#
/catalogue/f47ac10b-….json         →  /catalogue/#.json
/v2/geocode                        →  /v2/geocode      (a version is a name)
```

That shape is all refrain derives from the URL: it is what the tab shows, what
the JSON carries, what `--fail-if` names and what `w` writes to a file. The raw
line itself, in the **Stream** tab, is still the raw line — refrain shows logs
as they are, and never rewrites one.

**What the Endpoints tab gains.** An `HTTP/req` figure beside `SQL/req`, in the
JSON as `http_calls_avg` and `http_calls_max`: the average number of outbound
calls per HTTP request, which needs the same correlation token as the N+1
detection. The **Outbound** tab shows the other direction — per provider, its
latency, its statuses, and the endpoint that calls it most within one request:

```
Call                                        Calls  Worst/req  p50      p95      max       4xx  5xx
POST api.payments.test/v2/charges           162    11 ×       536 ms   2.08 s   16.07 s   3    2
GET api.geocoder.test/v1/geocode            255    12 ×       198 ms   776 ms   6.14 s    7    2
GET api.inventory.test/v1/products/#/stock  67     2 ×        105 ms   300 ms   429 ms    2    1
```

`Worst/req` is the **worst** repetition within a single request, not an average
— an average over every request that called the provider once would bury the
one that called it eleven times. The average is in the detail pane below, and
in the JSON as `avg_per_request`.

The threshold that goes with it is `http-client-p95`; see
[reports.md](reports.md#failing-a-build-on-a-slow-provider).

## Messages on the bus

Symfony Messenger writes a line for every message handed to a transport and
every message a worker takes off one. The gap between the two counts is the
thing no dashboard tells you: **89,338 dispatched, 374 handled** is a consumer
that died on Friday evening, and the first person to notice is usually a user.

**Getting them into a file.** The lines land on the `messenger` channel at
`INFO` — filtered out in production like everything else at that level:

```yaml
# config/packages/monolog.yaml
monolog:
    handlers:
        messenger:
            type: stream
            path: '%kernel.logs_dir%/messenger.log'
            level: info
            channels: [messenger]
```

**Hand the worker's log over too.** A dispatch is written where the request
runs; the handling is written where the worker runs, which is another process
and often another machine. With only the application's file, every message
looks unhandled:

```bash
refrain var/log/prod.log var/log/messenger.log var/log/worker.messenger.log
```

### What each line says

| Line | Counted as |
| --- | --- |
| `Sending message {class} with {alias} sender using {sender}` | dispatched |
| `{class} was handled successfully (acknowledging to transport).` | handled |
| `Message {class} handled by {handler}` | a handler run |
| `No handler for message {class}` | no handler |
| `Error thrown while handling message {class}. Sending for retry #{n}…` | retried |
| `Error thrown while handling message {class}. Removing from transport after {n} retries.` | failed |
| `Rejected message {class} will be sent to the failure transport {transport}.` | failed |

Two of those need a word. **Handled** is counted on the acknowledgement and not
on `handled by`, because a message with two handlers writes `handled by` twice
for one message off the queue; the handler runs are kept beside it, in the
detail. And **retried** is not **failed**: both lines begin
`Error thrown while handling message`, and reading the first as the second
would report an outage every time a transient error was retried.

The templates are matched on the part that does not vary, never on `{class}`:
Monolog leaves the placeholder in place unless `PsrLogMessageProcessor` is
configured, and refrain reads both forms the same. The class itself comes from
`context.class`, which Messenger writes on every line.

### The lag, and the identifier Symfony withholds

Measuring how long a message waited means pairing a dispatch with its
handling, and **core Symfony gives no identifier on the dispatch side** — it
writes `message_id` on the worker's lines only. Without one, the counts and
the backlog are exact and the lag is simply unknown, which the tab says with a
dash rather than a zero.

An audit middleware supplies it, and that is the widespread pattern:

```
[1a2b3c4d5e6f7] Sent App\Message\IndexEntityMessage {"id":"1a2b3c4d5e6f7","class":"App\Message\IndexEntityMessage"}
[1a2b3c4d5e6f7] Received App\Message\IndexEntityMessage
```

`Sent` and `Received` between an identifier in brackets, on a channel of your
own — `messenger_audit` is the usual name; refrain reads any channel whose name
starts with `messenger`. With it, the **Messenger** tab gains a `Lag p95`
column and the JSON its `lag_*` fields.

> **One dispatch, two lines.** An application running that middleware *and*
> Symfony's own logging writes both for the same message. refrain counts the
> two vocabularies apart and keeps the larger, so the dispatch counts once —
> adding them would make every queue look twice as deep as it is.

### The N+1 on the bus

The same shape as the SQL one, one layer up, and usually the costlier of the
two since every message is a job someone will have to run: 78,348 dispatches of
a single class in one run, one per entity, in a loop. refrain counts dispatches
per HTTP request through the same correlation token as the N+1
detection ([Measuring durations](#measuring-durations)), and the row names the
endpoint that dispatches the most within one request:

```
Message class           Dispatched  Handled  Waiting  Failed  Lag p95   Worst/req  Dispatched from
IndexEntityMessage         78,348       112   78,236       0  —         412 ×      app_product_list
SendInvoiceMessage          9,871       201    9,670      12  1.20 s      2 ×      app_checkout
```

`Waiting` is `dispatched − handled` **over the window read**, not for ever: a
message dispatched in the file's last second is in flight, not lost, and only a
read that ends well after the dispatch tells the two apart. A worker's own
handling is charged to no endpoint — it runs outside any HTTP request.

The thresholds that go with it are `messages-waiting` and `messages-failed`;
see [reports.md](reports.md#failing-a-build-on-a-queue-that-is-not-draining).

## Cache misses

Symfony's cache writes a line when it **computes** an item, and nothing at all
when it serves one from the cache:

```
[2026-09-12T10:23:45.123456+02:00] cache.INFO: Lock acquired, now computing item "nav_menu" {"key":"nav_menu"} []
```

Every one of those lines is therefore a miss, and there is no hit to read: a
key that turns up on nearly every request is a cache that is not working. That
is usually a five-minute fix, and it is invisible until something counts them.

The lines land on the `cache` channel at `INFO`, filtered out in production
like everything else at that level:

```yaml
# config/packages/monolog.yaml
monolog:
    handlers:
        cache:
            type: stream
            path: '%kernel.logs_dir%/cache.log'
            level: info
            channels: [cache]
```

`refrain --summary var/log/prod.log var/log/cache.log` then reports:

```
Cache misses (469 misses, 4 keys)
      275 × nav_menu
          on 275 of 300 requests · 10 waited on another process computing it
       75 × product_#_detail
          on 75 of 300 requests · 2 × within one request, from app_product_list
```

`on 275 of 300 requests` is the whole point. The **Overview** tab carries the
same list beside the levels and the channels, and the JSON a `cache` block with
a `cache_keys` list.

### What the lines mean

| Line | Counted as |
| --- | --- |
| `Lock acquired, now computing item "{key}"` | a miss this process paid for |
| `Lock not supported, now computing item "{key}"` | the same, where locking is unavailable |
| `Item "{key}" is locked, waiting for it to be released` | a miss another process is paying for |

The two lines that follow a wait — `retrieved after lock was released`,
`not found while lock was released, now retrying` — are its outcome, not a
second miss, and counting them would double the contention. As everywhere
else, the templates are matched on the part that does not vary rather than on
`{key}`, which Monolog leaves in place unless `PsrLogMessageProcessor` is
configured; the key itself comes from `context.key`.

### Keys are folded like signatures

`product_42_detail` and `product_1337_detail` are one cache entry family, not
two hundred rows, so a run of digits becomes `#` — the same normaliser that
groups errors. What is left is the key you would go and look for.

A key computed **twice within one request** is that item computed twice over,
and the row says so with the endpoint that did it. Counting per request needs
the same correlation token as the N+1 detection
([Measuring durations](#measuring-durations)); without one the totals are
still exact.

## Tracking deprecations

Symfony's `ErrorHandler` turns every deprecation — a `trigger_deprecation()`
in a vendor, PHP's own `Deprecated:` notices — into a log line at `INFO`:

```
[2026-09-12T10:23:45.123456+02:00] php.INFO: User Deprecated: Since symfony/http-foundation 6.2: Calling "Symfony\Component\HttpFoundation\Request::getContentType()" is deprecated, use "getContentTypeFormat()" instead. {"exception":"[object] (ErrorException(code: 0): User Deprecated: … at /var/www/vendor/symfony/http-foundation/Request.php:1290)"} []
```

refrain recognises the `User Deprecated: ` / `Deprecated: ` prefix, whatever
the channel, and reads the origin — the deprecated code itself, not your call
site — from the exception in the context. The **Deprecations** tab groups them
by message and origin, with the route that triggered each last, and
`--fail-if 'deprecations>0'` fails a build on the first one; see
[deprecations](reports.md#deprecations).

What needs a hand is getting them into a file. Nothing to do in `dev`, where
the default handler writes everything from `DEBUG` up. In `prod`, the
`fingers_crossed` handler only flushes its buffer on an error, so deprecations
never reach `prod.log`. The Monolog recipe already declares a `deprecation`
channel for them; give it a file handler of its own:

```yaml
# config/packages/monolog.yaml
monolog:
    channels: [deprecation]
    handlers:
        deprecation:
            type: stream
            path: '%kernel.logs_dir%/%kernel.environment%.deprecations.log'
            channels: [deprecation]
```

With that channel declared, Symfony routes deprecations to it instead of
`php`; without it, they go through `php` and follow the fate of its handler.
Either way the line looks the same to refrain. The file this produces is the
one to hand over before an upgrade, or to a `--fail-if` in the test suite.
