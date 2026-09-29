//! `core` has to keep working in a browser, and the way it stops is quiet.
//!
//! `std::time::Instant::now()` compiles for `wasm32-unknown-unknown` and
//! panics when it runs. A `cargo check --target wasm32-unknown-unknown` says
//! nothing, the browser build ships, and the first tile traps with
//! `RuntimeError: unreachable` somewhere inside a future — which is how this
//! was actually found, after the clock had supposedly been abstracted away.
//!
//! So the rule is checked where it can be seen: `timing.rs` owns the clock,
//! and nothing else in the crate reads one.

use std::fs;
use std::path::Path;

/// Things that work natively and trap or misbehave on wasm.
const FORBIDDEN: &[(&str, &str)] = &[
    ("Instant::now", "use Timings::start/since or Timings::time — Instant panics on wasm"),
    ("SystemTime::now", "no wall clock on wasm; take the time from the host"),
    ("std::thread::spawn", "no threads on wasm32-unknown-unknown"),
];

#[test]
fn nothing_outside_timing_reads_a_clock() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offences = Vec::new();

    for entry in fs::read_dir(&src).expect("core/src") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // timing.rs is where the clock lives, behind a cfg that gives wasm
        // none at all.
        if name == "timing.rs" {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read source");
        for (line_no, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for (needle, why) in FORBIDDEN {
                if code.contains(needle) {
                    offences.push(format!("{name}:{}: {needle} — {why}", line_no + 1));
                }
            }
        }
    }

    assert!(offences.is_empty(),
            "core must stay usable from a browser:\n  {}", offences.join("\n  "));
}
