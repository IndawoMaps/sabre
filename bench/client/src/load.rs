//! The two load models.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::plan::{Mode, Plan, Target};
use crate::stats::Sample;

pub struct Outcome {
    pub samples: Vec<Sample>,
    pub wall_s: f64,
    pub overloaded: bool,
}

/// One request, start to body-read. The body is read to completion and
/// discarded: stopping at the headers would leave the server's write and the
/// transfer out of the measurement entirely.
async fn send_one(
    client: &reqwest::Client,
    base: &str,
    target: &Target,
    accept: &[u16],
    intended: Instant,
) -> Sample {
    let url = format!("{base}{}", target.path);
    let method =
        reqwest::Method::from_bytes(target.method.as_bytes()).unwrap_or(reqwest::Method::GET);

    let mut req = client.request(method, &url);
    for (k, v) in &target.headers {
        req = req.header(k, v);
    }
    if let Some(body) = &target.body {
        req = req.body(body.clone());
    }

    let sent = Instant::now();
    let lag_ms = sent.saturating_duration_since(intended).as_secs_f64() * 1e3;

    let (status, bytes, ok, timing) = match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            // Read before the body: consuming the body moves the response.
            let timing = resp
                .headers()
                .get("server-timing")
                .and_then(|v| v.to_str().ok())
                .map(crate::stats::parse_server_timing)
                .unwrap_or_else(crate::stats::no_phases);
            match resp.bytes().await {
                Ok(b) => (status, b.len() as u64, accept.contains(&status), timing),
                // Headers arrived, body did not. Not a served request.
                Err(_) => (status, 0, false, timing),
            }
        }
        // Transport failure: no status exists, so it is recorded as 0 and the
        // status map will show it rather than silently dropping the request.
        Err(_) => (0u16, 0u64, false, crate::stats::no_phases()),
    };

    let done = Instant::now();
    Sample {
        service_ms: done.saturating_duration_since(sent).as_secs_f64() * 1e3,
        corrected_ms: done.saturating_duration_since(intended).as_secs_f64() * 1e3,
        lag_ms,
        status,
        bytes,
        ok,
        timing,
    }
}

fn should_stop(plan: &Plan, started: Instant, issued: usize) -> bool {
    if let Some(d) = plan.duration_s {
        if started.elapsed().as_secs_f64() >= d {
            return true;
        }
    }
    if let Some(n) = plan.requests {
        if issued >= n {
            return true;
        }
    }
    false
}

/// `concurrency` workers in lockstep with the server: each sends again as soon
/// as its previous request returns.
pub async fn closed(
    client: &reqwest::Client,
    plan: &Plan,
    concurrency: usize,
    record: bool,
) -> Outcome {
    let cursor = Arc::new(AtomicUsize::new(0));
    let issued = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();

    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let client = client.clone();
        let plan = plan.clone();
        let cursor = cursor.clone();
        let issued = issued.clone();
        workers.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            loop {
                let n = issued.fetch_add(1, Ordering::Relaxed);
                if should_stop(&plan, start, n) {
                    break;
                }
                let i = cursor.fetch_add(1, Ordering::Relaxed) % plan.targets.len();
                // In closed-loop the intended time *is* now: there is no
                // schedule to fall behind, so corrected == service by design.
                let now = Instant::now();
                let s = send_one(
                    &client,
                    &plan.base,
                    &plan.targets[i],
                    &plan.accept_statuses,
                    now,
                )
                .await;
                if record {
                    mine.push(s);
                }
            }
            mine
        }));
    }

    let mut samples = Vec::new();
    for w in workers {
        if let Ok(mut m) = w.await {
            samples.append(&mut m);
        }
    }

    Outcome {
        samples,
        wall_s: start.elapsed().as_secs_f64(),
        overloaded: false,
    }
}

/// Requests leave on a fixed schedule whether or not earlier ones have come
/// back. Slots are computed from the start time rather than by sleeping for a
/// fixed interval, so a slow iteration does not push every later slot back and
/// silently reduce the offered rate.
pub async fn open(client: &reqwest::Client, plan: &Plan, rate: f64, record: bool) -> Outcome {
    let (tx, mut rx) = mpsc::unbounded_channel::<Sample>();
    let inflight = Arc::new(AtomicUsize::new(0));
    let overloaded = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let period = Duration::from_secs_f64(1.0 / rate);

    let mut issued = 0usize;
    loop {
        if should_stop(plan, start, issued) {
            break;
        }
        if inflight.load(Ordering::Relaxed) >= plan.max_inflight {
            overloaded.store(true, Ordering::Relaxed);
            break;
        }

        let intended = start + period.mul_f64(issued as f64);
        tokio::time::sleep_until(intended).await;

        let client = client.clone();
        let plan_base = plan.base.clone();
        let target = plan.targets[issued % plan.targets.len()].clone();
        let accept = plan.accept_statuses.clone();
        let tx = tx.clone();
        let inflight = inflight.clone();

        inflight.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let s = send_one(&client, &plan_base, &target, &accept, intended).await;
            inflight.fetch_sub(1, Ordering::Relaxed);
            if record {
                let _ = tx.send(s);
            }
        });

        issued += 1;
    }
    drop(tx);

    // Let requests already on the wire finish, so the tail of the run is not
    // cut off in a way that would flatter the percentiles.
    let drain_deadline = Instant::now() + Duration::from_millis(plan.timeout_ms + 1_000);
    let mut samples = Vec::new();
    loop {
        match tokio::time::timeout_at(drain_deadline, rx.recv()).await {
            Ok(Some(s)) => samples.push(s),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    Outcome {
        samples,
        wall_s: start.elapsed().as_secs_f64(),
        overloaded: overloaded.load(Ordering::Relaxed),
    }
}

pub async fn run(client: &reqwest::Client, plan: &Plan) -> Outcome {
    match plan.mode {
        Mode::Closed { concurrency } => closed(client, plan, concurrency, true).await,
        Mode::Open { rate } => open(client, plan, rate, true).await,
    }
}

/// Same load model, results discarded.
pub async fn warmup(client: &reqwest::Client, plan: &Plan) {
    if plan.warmup_s <= 0.0 {
        return;
    }
    let mut p = plan.clone();
    p.duration_s = Some(plan.warmup_s);
    p.requests = None;
    match p.mode {
        Mode::Closed { concurrency } => {
            closed(client, &p, concurrency, false).await;
        }
        Mode::Open { rate } => {
            open(client, &p, rate, false).await;
        }
    }
}
