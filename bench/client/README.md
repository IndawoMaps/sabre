# bench-client

The load generator for the sabre benchmark suite. Reads a plan as JSON, writes
a result as JSON, and knows nothing about tiles or COGs.

## Why it exists

The Python harness in `bench/run.py` is closed-loop: N threads, each sending
again the moment the last response lands. That answers *"how fast is a response
when 8 are in flight?"* — but it cannot answer *"what happens at 400 requests
per second?"*, because offered load is whatever the server permits. You get one
point per concurrency level, never a curve, and never a saturation knee.

Open-loop puts requests on the wire on a fixed schedule whether or not earlier
ones have returned, which is how real traffic arrives. Then the knee is visible:

    offered  achieved   p50 ms   p99 ms
        500       500     0.21     3.92
       2000      1997     0.32     6.31
       3000      2996     3.15    16.84   <- still keeping up
       3500      3290    83.53   278.03   <- knee
       5000      3262   596.69  2150.48   <- saturated, latency is queueing

Achieved rate flattens around 3,300 while latency grows without bound. That
number — what the server can actually carry — does not appear anywhere in a
closed-loop run.

## Use

    bench-client --plan plan.json --out result.json
    bench-client --plan plan.json --rate 2000 --duration 30    # override
    cat plan.json | bench-client > result.json

`--rate` switches the plan to open-loop, `--concurrency` to closed-loop, so one
plan file drives a whole sweep.

## The plan

```json
{
  "name": "tiles z=8",
  "base": "http://127.0.0.1:8787",
  "mode": {"closed": {"concurrency": 8}},
  "duration_s": 30,
  "warmup_s": 5,
  "targets": [
    {"path": "/tiles/8/135/126?url=...&colormap=viridis"},
    {"method": "POST", "path": "/cog/statistics?url=...",
     "headers": {"Content-Type": "application/json"},
     "body": "{\"type\":\"Feature\", ...}"}
  ]
}
```

`{"open": {"rate": 500}}` for open-loop. Either `duration_s` or `requests` must
be set. Targets cycle in order; a run longer than the list re-requests the same
URLs, which measures a warm server rather than an ever-growing working set.

Optional: `timeout_ms` (30000), `accept_statuses` (`[200]`), `max_inflight`
(50000 — open-loop scheduling stops there and flags `overloaded`, because past
that point you are measuring the generator's backlog).

## Reading the result

Three latency views, because they answer different questions:

- **`service_ms`** — time on the wire, from send to body read. Compare servers
  with this.
- **`corrected_ms`** — from when the request was *due* to go out. In open-loop
  the gap to `service_ms` is queueing that a naive measurement hides
  (coordinated omission). Identical to `service_ms` in closed-loop.
- **`lag_ms`** — how late the generator was. Open-loop only.

Failed requests are excluded from the latency distributions. A connection
refused in 0.1 ms is not a fast response, and letting it in makes a failing
server look quick. They are still counted in `errors` and `statuses`.

Two flags:

- **`client_saturated`** — the generator could not keep its own schedule, so
  the server numbers are a lower bound. Requires the p99 slip to exceed both
  10 ms and the server's median, because a general-purpose OS lands a
  millisecond or two late on every `sleep_until` regardless of load, and an
  earlier version of this check fired on that jitter at *low* rates while
  staying silent at genuine saturation.
- **`lag_exceeds_service`** — scheduling jitter is larger than the response
  being measured. Not a failure; `service_ms` is still sound. It means
  `corrected_ms` is mostly timer granularity and open-loop is the wrong tool
  for an endpoint this fast.

Percentiles are nearest-rank over raw samples, kept whole rather than folded
into a histogram — 3 million samples is about 70 MB, cheap enough to keep the
ability to re-derive a quantile afterwards. `cdf_service_ms` is a 50-point
thinning for plotting. `requests` is in the output so that a p99 drawn from a
handful of samples is visible rather than implied.

NaN serialises as JSON `null`, which is what you get for percentiles when
nothing was served.

## Known limitation

`sleep_until` on a general-purpose OS is accurate to a millisecond or two, so
above roughly 1,000 req/s the per-request schedule is approximate — the *rate*
is held accurately, the spacing is not. It does not affect knee detection, and
`lag_ms` reports it honestly, but sub-millisecond endpoints (sabre's `/info`
answers in 0.3 ms) cannot be characterised open-loop on this timer. Use
closed-loop for those.
