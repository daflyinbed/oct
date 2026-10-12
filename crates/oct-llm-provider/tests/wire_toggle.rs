//! Off-start sink semantics. This binary MUST stay separate from the other
//! wire tests: the `Interest::sometimes` regression it pins is per-callsite
//! and per-process — in a shared binary some other test would pre-register
//! the callsites while the sink is enabled, and the poisoned-`never`
//! scenario this test reproduces could never occur.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use oct_llm_provider::wire::{self, TARGET, WireLogControl};

/// One emission point, deliberately a single function: every call shares
/// the same tracing callsite, whose FIRST evaluation happens while the sink
/// is off — exactly the registration the default `register_callsite` would
/// cache as `Interest::never`.
fn emit_probe(kind: &str) {
    tracing::trace!(target: TARGET, kind = kind, conversation_id = Some("toggle-probe"));
}

/// Install the sink OFF, then the subscriber, once for the process.
fn install_off_start() -> (WireLogControl, PathBuf) {
    static INSTALLED: OnceLock<(WireLogControl, PathBuf)> = OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("oct-wire-toggle-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let control = wire::init(dir.clone(), false);
            use tracing_subscriber::prelude::*;
            let _ = tracing_subscriber::registry()
                .with(wire::WireLogLayer)
                .try_init();
            (control, dir)
        })
        .clone()
}

/// Bounded poll for the writer thread (same rationale as the other wire
/// tests: a plain std thread + mpsc, nothing to await).
fn wait_for_lines(
    dir: &Path,
    pred: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(dir.join("toggle-probe.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("valid JSONL"))
            .collect();
        if pred(&lines) {
            return lines;
        }
        assert!(Instant::now() < deadline, "wire lines never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn callsite_first_seen_off_recovers_after_enable_and_hard_disable_pins() {
    let (control, dir) = install_off_start();

    // First registration of the probe callsite happens with the sink OFF.
    assert!(!control.enabled());
    emit_probe("while-off");

    // Flipping on must resurrect that same callsite: under the default
    // `register_callsite` (cached `never`) the second emission stays dead.
    control.set_enabled(true);
    assert!(control.enabled());
    emit_probe("while-on");
    let lines = wait_for_lines(&dir, |ls| ls.len() == 1);
    assert_eq!(
        lines[0]["kind"], "while-on",
        "only the post-enable line exists"
    );

    // Hard-disable pins the sink off for the rest of the process: later
    // set_enabled calls are rejected outright. The disabled event is
    // dropped at the layer's enabled() gate — it never reaches the writer,
    // so no waiting is needed for the absence assertion below.
    control.hard_disable();
    control.set_enabled(true);
    assert!(!control.enabled());
    emit_probe("after-hard-off");
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(dir.join("toggle-probe.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSONL"))
        .collect();
    assert_eq!(lines.len(), 1, "hard-off pins the sink: no new lines");
    // Routing/level contract sanity while the sink was live.
    assert_eq!(lines[0]["level"], "TRACE");
    assert_eq!(lines[0]["target"], TARGET);
    assert_eq!(lines[0]["conversation_id"], "toggle-probe");
}
