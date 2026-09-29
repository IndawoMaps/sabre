//! The work description a run is driven from.
//!
//! The client deliberately knows nothing about tiles, zoom levels or COGs: it
//! is handed a list of fully-formed requests and a load model. Keeping the
//! tile arithmetic out of here means the same binary drives sabre, titiler or
//! anything else, and that a plan can be archived alongside its results as an
//! exact record of what was asked.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Plan {
    /// Label carried through to the results, e.g. "tiles z=8".
    pub name: String,

    /// Prefixed to every target path.
    pub base: String,

    /// Cycled in order, restarting when exhausted. A run longer than the list
    /// therefore re-requests the same URLs, which is usually what you want:
    /// it measures a warm server rather than an ever-growing working set.
    pub targets: Vec<Target>,

    pub mode: Mode,

    /// Stop after this long. Either this or `requests` must be set.
    #[serde(default)]
    pub duration_s: Option<f64>,

    /// Stop after this many requests.
    #[serde(default)]
    pub requests: Option<usize>,

    /// Untimed requests sent first, under the same load model, so that header
    /// caches and block caches are paid for before the clock starts.
    #[serde(default)]
    pub warmup_s: f64,

    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Statuses counted as served. Everything seen is reported either way.
    #[serde(default = "default_accept")]
    pub accept_statuses: Vec<u16>,

    /// Open-loop safety valve. Scheduling stops once this many requests are in
    /// flight at once, and the result is flagged `overloaded`: past this point
    /// the generator is measuring its own backlog, not the server.
    #[serde(default = "default_max_inflight")]
    pub max_inflight: usize,
}

fn default_timeout_ms() -> u64 {
    30_000
}
fn default_accept() -> Vec<u16> {
    vec![200]
}
fn default_max_inflight() -> usize {
    50_000
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// `concurrency` workers, each sending the next request as soon as the
    /// previous one returns. Measures service time at a fixed in-flight count.
    /// Offered load is whatever the server allows, so this cannot produce a
    /// latency-versus-load curve -- use `open` for that.
    Closed { concurrency: usize },

    /// Requests are issued on a fixed schedule regardless of whether earlier
    /// ones have returned, which is how real traffic arrives. This is the mode
    /// that can find a saturation knee.
    Open { rate: f64 },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Target {
    #[serde(default = "default_method")]
    pub method: String,

    /// Appended to `Plan::base`, query string included.
    pub path: String,

    #[serde(default)]
    pub body: Option<String>,

    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn default_method() -> String {
    "GET".into()
}

impl Plan {
    pub fn validate(&self) -> Result<(), String> {
        if self.targets.is_empty() {
            return Err("plan has no targets".into());
        }
        if self.duration_s.is_none() && self.requests.is_none() {
            return Err("plan sets neither duration_s nor requests".into());
        }
        if let Mode::Closed { concurrency } = self.mode {
            if concurrency == 0 {
                return Err("closed-loop concurrency must be at least 1".into());
            }
        }
        if let Mode::Open { rate } = self.mode {
            if !(rate.is_finite() && rate > 0.0) {
                return Err("open-loop rate must be a positive number".into());
            }
        }
        Ok(())
    }
}
