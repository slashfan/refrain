# What refrain needs from your application

refrain reads what Monolog already writes, and nothing here is required to get
a dashboard: errors, channels, volumes and traffic peaks work out of the box.
Two things do need a hand — measuring how long a request took, and detecting
N+1 queries — because Monolog writes neither on its own. A third, deprecations,
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
Endpoint            Requests  SQL/req  p50      p95      max      5xx  Err.
api_orders_list     73        29.1     912 ms   3.35 s   4.49 s   7    9.6%
app_search          95        2.0      230 ms   900 ms   1.08 s   0    5.3%
```

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
