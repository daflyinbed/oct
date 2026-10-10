//! `Layer::enabled` is global narrowing: a `false` from any layer on the
//! stack short-circuits the callsite for EVERY layer. Before this was
//! understood, `WireLogLayer::enabled` answered `false` for every non-wire
//! callsite — as the outermost layer it silently killed ALL console output
//! of a stack like `registry().with(fmt).with(WireLogLayer)` (main.rs).
//! This binary pins the layer contract:
//!
//! - non-wire events reach inner layers in BOTH sink states;
//! - wire events reach inner layers (the stderr-mirror shape) only while
//!   the sink is on — that gate also powers the adapters' serialization
//!   probe, so an off sink must answer `false` for wire callsites.

use std::sync::{Arc, Mutex};

use oct_llm_provider::wire::{self, TARGET};

/// Records every event that survives the global filter, as
/// `target:message` pairs. Cheap-clone handle so the same recordings are
/// visible to the assertion side.
#[derive(Clone, Default)]
struct Recorder(Arc<RecorderInner>);

#[derive(Default)]
struct RecorderInner {
    seen: Mutex<Vec<String>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorder {
    fn on_event(&self, event: &tracing::Event<'_>, _cx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = StringRecorder::default();
        event.record(&mut visitor);
        self.0.seen.lock().unwrap().push(format!(
            "{}:{}",
            event.metadata().target(),
            visitor.0.unwrap_or_default()
        ));
    }
}

#[derive(Default)]
struct StringRecorder(Option<String>);

impl tracing::field::Visit for StringRecorder {
    fn record_str(&mut self, _field: &tracing::field::Field, value: &str) {
        self.0 = Some(value.to_string());
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if self.0.is_none() {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

#[test]
fn wire_layer_gates_only_its_own_target() {
    let dir = std::env::temp_dir().join(format!("oct-wire-layer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let control = wire::init(dir.clone(), true);

    let recorder = Recorder::default();
    use tracing_subscriber::prelude::*;
    // Thread-scoped default: no global subscriber install, so this binary
    // stays composable even if more tests join it later.
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(recorder.clone())
            .with(wire::WireLogLayer),
    );

    // Non-wire events must flow through regardless of the sink state —
    // this is the regression: the old `enabled` returned false here and
    // strace-verified servers never wrote a single byte to stdout.
    control.set_enabled(true);
    tracing::info!(target: "oct_agent", "console while on");
    control.set_enabled(false);
    tracing::info!(target: "oct_agent", "console while off");
    assert_eq!(
        *recorder.0.seen.lock().unwrap(),
        vec![
            "oct_agent:console while on".to_string(),
            "oct_agent:console while off".to_string(),
        ],
    );

    // Wire events: globally enabled (and mirrored to inner layers) while
    // the sink is on, globally gated off while it is off — the same
    // answer the adapters' `tracing::enabled!` serialization probe sees.
    control.set_enabled(true);
    tracing::trace!(target: TARGET, kind = "mirrored", conversation_id = Some("filter-probe"));
    control.set_enabled(false);
    tracing::trace!(target: TARGET, kind = "gated", conversation_id = Some("filter-probe"));
    let seen = recorder.0.seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "one mirror line, no line while off");
    assert!(
        seen[2].starts_with(&format!("{TARGET}:")),
        "mirror line is the wire event"
    );
}
