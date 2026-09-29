//! `bench-client` — the load generator for the sabre benchmark suite.
//!
//! It exists because the previous Python harness was closed-loop only. Closed
//! loop answers "how fast is a response when N are in flight"; it cannot
//! answer "what happens at 400 requests per second", because offered load is
//! whatever the server allows. Finding a saturation knee needs requests to
//! leave on a schedule the server does not control, and needs latency measured
//! from when a request *should* have gone out rather than from when it did.
//!
//! Reads a plan on stdin or from a file, writes a result object to stdout.
//! Anything human-readable goes to stderr so stdout stays pipeable.

mod load;
mod plan;
mod stats;

use std::io::Read;
use std::time::Duration;

use plan::{Mode, Plan};

const USAGE: &str = "\
bench-client — load generator for the sabre benchmark suite

USAGE:
    bench-client [OPTIONS]

OPTIONS:
    --plan <FILE>        Plan to run, as JSON            [default: read stdin]
    --out <FILE>         Write the result here           [default: stdout]
    --rate <N>           Override the plan's open-loop rate
    --concurrency <N>    Override the plan's closed-loop concurrency
    --duration <SECS>    Override the plan's duration
    --quiet              No progress on stderr
    -h, --help           Print this help

The plan is a JSON object:

    {
      \"name\": \"tiles z=8\",
      \"base\": \"http://127.0.0.1:8787\",
      \"mode\": {\"closed\": {\"concurrency\": 8}},
      \"duration_s\": 30,
      \"warmup_s\": 5,
      \"targets\": [
        {\"path\": \"/tiles/8/135/126?url=...&colormap=viridis\"},
        {\"method\": \"POST\", \"path\": \"/cog/statistics?url=...\",
         \"headers\": {\"Content-Type\": \"application/json\"},
         \"body\": \"{...}\"}
      ]
    }

Use {\"open\": {\"rate\": 500}} to offer a fixed 500 req/s instead.
";

struct Args {
    plan: Option<String>,
    out: Option<String>,
    rate: Option<f64>,
    concurrency: Option<usize>,
    duration: Option<f64>,
    quiet: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        plan: None,
        out: None,
        rate: None,
        concurrency: None,
        duration: None,
        quiet: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--plan" => a.plan = Some(value("--plan")?),
            "--out" => a.out = Some(value("--out")?),
            "--rate" => {
                a.rate = Some(
                    value("--rate")?
                        .parse()
                        .map_err(|_| "--rate must be a number")?,
                )
            }
            "--concurrency" => {
                a.concurrency = Some(
                    value("--concurrency")?
                        .parse()
                        .map_err(|_| "--concurrency must be an integer")?,
                )
            }
            "--duration" => {
                a.duration = Some(
                    value("--duration")?
                        .parse()
                        .map_err(|_| "--duration must be a number")?,
                )
            }
            "--quiet" => a.quiet = true,
            other => return Err(format!("unrecognised argument: {other}")),
        }
    }
    Ok(a)
}

fn load_plan(args: &Args) -> Result<Plan, String> {
    let raw = match &args.plan {
        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?,
        None => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("reading stdin: {e}"))?;
            s
        }
    };
    let mut plan: Plan = serde_json::from_str(&raw).map_err(|e| format!("parsing plan: {e}"))?;

    // Overrides let one plan file drive a whole sweep without rewriting it.
    if let Some(rate) = args.rate {
        plan.mode = Mode::Open { rate };
    }
    if let Some(concurrency) = args.concurrency {
        plan.mode = Mode::Closed { concurrency };
    }
    if let Some(d) = args.duration {
        plan.duration_s = Some(d);
        plan.requests = None;
    }

    plan.validate()?;
    Ok(plan)
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bench-client: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let plan = match load_plan(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bench-client: {e}");
            std::process::exit(2);
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let summary = runtime.block_on(async move {
        let (mode_label, offered) = match plan.mode {
            Mode::Closed { concurrency } => (format!("closed/{concurrency}"), None),
            Mode::Open { rate } => (format!("open/{rate}"), Some(rate)),
        };

        // One pool, sized so that closed-loop workers are never queued behind
        // each other for a connection: that would cap concurrency below what
        // the plan asked for and look like server latency.
        let pool = match plan.mode {
            Mode::Closed { concurrency } => concurrency.max(1),
            Mode::Open { .. } => plan.max_inflight.min(10_000),
        };
        let client = reqwest::Client::builder()
            .http1_only()
            .pool_max_idle_per_host(pool)
            .timeout(Duration::from_millis(plan.timeout_ms))
            .build()
            .expect("http client");

        if !args.quiet {
            eprintln!(
                "bench-client: {} [{}] {} targets",
                plan.name,
                mode_label,
                plan.targets.len()
            );
        }

        load::warmup(&client, &plan).await;

        // Wall-clock, not the monotonic clock the latencies use: this window is
        // compared against timestamps written by a different process.
        let started_unix = unix_now();
        let outcome = load::run(&client, &plan).await;
        let finished_unix = unix_now();

        let summary = stats::summarise(
            &outcome.samples,
            stats::SummaryInput {
                name: &plan.name,
                mode: &mode_label,
                offered_rps: offered,
                wall_s: outcome.wall_s,
                window: [started_unix, finished_unix],
                open_loop: matches!(plan.mode, Mode::Open { .. }),
                overloaded: outcome.overloaded,
            },
        );

        if !args.quiet {
            eprintln!(
                "  {} ok, {} err in {:.2}s — {:.0} req/s, p50 {:.2}ms p99 {:.2}ms",
                summary.ok,
                summary.errors,
                summary.wall_s,
                summary.achieved_rps,
                summary.service_ms.p50,
                summary.service_ms.p99,
            );
            if summary.client_saturated {
                eprintln!(
                    "  WARNING: generator fell behind its schedule (lag p99 {:.2}ms). \
                           Server numbers are a lower bound.",
                    summary.lag_ms.as_ref().map(|l| l.p99).unwrap_or(f64::NAN)
                );
            }
            if summary.lag_exceeds_service {
                eprintln!(
                    "  note: scheduling jitter (lag p50 {:.2}ms) exceeds the response time. \
                           service_ms is sound; corrected_ms is mostly timer granularity.",
                    summary.lag_ms.as_ref().map(|l| l.p50).unwrap_or(f64::NAN)
                );
            }
            if summary.overloaded {
                eprintln!("  WARNING: hit max_inflight; scheduling stopped early.");
            }
        }
        summary
    });

    let json = serde_json::to_string_pretty(&summary).expect("serialise summary");
    match &args.out {
        Some(path) => {
            if let Err(e) = std::fs::write(path, json + "\n") {
                eprintln!("bench-client: writing {path}: {e}");
                std::process::exit(1);
            }
        }
        None => println!("{json}"),
    }
}
