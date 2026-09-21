//! In-memory event hub for ONE agent run: a replay log plus live fan-out.
//!
//! Replaces the plain `tokio::broadcast` channel that backed the SSE stream.
//! broadcast only reaches subscribers that exist at publish time, so a
//! frontend refresh mid-run lost every event. The hub instead keeps a bounded
//! (delta-merged) log of everything the run published and lets new
//! subscribers atomically receive the full snapshot followed by live events —
//! reconnect + replay with no gap and no duplication.
//!
//! Memory-only by design: JSONL durability, event sequence numbers and
//! service-restart recovery are explicit non-goals. When the run ends, the
//! sessions entry is dropped, the last `Arc<EventHub>` goes away, the hub (and
//! with it every registered sender) is dropped, and every subscriber stream
//! reaches EOF — the same termination semantics the broadcast channel had.

use std::sync::Mutex;

use futures_util::StreamExt;
use futures_util::stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::AgentEvent;

/// Cap for a single merged `ToolOutputDelta` entry in the replay log, aligned
/// with the frontend's `LIVE_OUTPUT_MAX_CHARS` tail cap: a replayed call's
/// live output keeps at most this many chars (oldest trimmed from the front),
/// exactly like the live view it rebuilds.
const REPLAY_TOOL_OUTPUT_MAX_CHARS: usize = 32_768;

/// The stream returned by [`EventHub::subscribe`]: the log snapshot at
/// subscribe time, chained with this subscriber's live events.
pub type SubscribeStream = stream::Chain<
    stream::Iter<std::vec::IntoIter<AgentEvent>>,
    UnboundedReceiverStream<AgentEvent>,
>;

struct HubInner {
    /// Replay log of this run; deltas are merged into the tail to bound its
    /// size (live subscribers still receive every event individually).
    log: Vec<AgentEvent>,
    /// Live subscribers. A sender whose receiver is gone is dropped on the
    /// next publish (mpsc send failure), so disconnected SSE streams get
    /// garbage-collected for free.
    subs: Vec<mpsc::UnboundedSender<AgentEvent>>,
}

pub struct EventHub {
    inner: Mutex<HubInner>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HubInner {
                log: Vec::new(),
                subs: Vec::new(),
            }),
        }
    }

    /// Publish one event: append (or merge) it into the replay log and fan it
    /// out to every live subscriber. Synchronous — the log is up to date the
    /// moment this returns, which is what lets tests subscribe afterwards and
    /// read the merged snapshot deterministically.
    pub fn publish(&self, ev: AgentEvent) {
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::merge_into_log(&mut inner.log, &ev);
        // Fan out to live subscribers; a failed send means the receiver was
        // dropped (disconnected client) — remove it.
        inner.subs.retain(|tx| tx.send(ev.clone()).is_ok());
    }

    /// Append `ev` to the replay log, merging consecutive same-type deltas so
    /// the log stays proportional to the run's content, not its event count:
    /// `TextDelta`/`ReasoningDelta` merge into a trailing entry of the same
    /// type; `ToolOutputDelta` merges only when call_id AND stream match the
    /// tail, and the merged entry is capped at
    /// [`REPLAY_TOOL_OUTPUT_MAX_CHARS`] chars (trimmed from the front).
    /// Everything else pushes as-is.
    fn merge_into_log(log: &mut Vec<AgentEvent>, ev: &AgentEvent) {
        match ev {
            AgentEvent::TextDelta(delta) => {
                if let Some(AgentEvent::TextDelta(tail)) = log.last_mut() {
                    tail.push_str(delta);
                    return;
                }
                log.push(ev.clone());
            }
            AgentEvent::ReasoningDelta(delta) => {
                if let Some(AgentEvent::ReasoningDelta(tail)) = log.last_mut() {
                    tail.push_str(delta);
                    return;
                }
                log.push(ev.clone());
            }
            AgentEvent::ToolOutputDelta {
                call_id,
                stream,
                delta,
            } => {
                let merges = matches!(
                    log.last(),
                    Some(AgentEvent::ToolOutputDelta {
                        call_id: tail_call,
                        stream: tail_stream,
                        ..
                    }) if tail_call == call_id && tail_stream == stream
                );
                if merges {
                    if let Some(AgentEvent::ToolOutputDelta { delta: tail, .. }) = log.last_mut() {
                        tail.push_str(delta);
                        // Trim from the front, landing on a char boundary so
                        // the String stays valid UTF-8.
                        let mut excess = tail.len().saturating_sub(REPLAY_TOOL_OUTPUT_MAX_CHARS);
                        while excess < tail.len() && !tail.is_char_boundary(excess) {
                            excess += 1;
                        }
                        if excess > 0 {
                            tail.drain(..excess);
                        }
                        return;
                    }
                }
                log.push(ev.clone());
            }
            _ => log.push(ev.clone()),
        }
    }

    /// Subscribe to this run's events: the full replay log snapshot followed
    /// by live events. The snapshot clone and the sender registration happen
    /// atomically under the same lock `publish` takes, so the
    /// replay-to-increment handover neither drops nor duplicates an event.
    ///
    /// The returned stream does NOT keep the hub alive: when the run ends and
    /// the last hub reference drops, the stream reaches EOF.
    pub fn subscribe(&self) -> SubscribeStream {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let snapshot = inner.log.clone();
        inner.subs.push(tx);
        stream::iter(snapshot).chain(UnboundedReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every publisher's payload, in arrival order.
    fn payloads(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn snapshot_then_live_handover_is_exact() {
        // Deltas MERGE in the log, so a subscriber's view is [merged snapshot
        // entries][raw live events]. This pins the handover boundary exactly:
        // publish a batch, subscribe mid-flight, publish another batch.
        let hub = std::sync::Arc::new(EventHub::new());

        // Ground truth: joined before any publish, so it observes the raw
        // unmerged stream the snapshots are checked against.
        let live = hub.subscribe();

        let publish = |name: &str, n: usize| {
            for i in 0..n {
                hub.publish(AgentEvent::TextDelta(format!("{name}-{i}")));
            }
        };
        let payload = |name: &str, i: usize| format!("{name}-{i}");
        let batch = |name: &str, n: usize| {
            (0..n).map(|i| payload(name, i)).collect::<String>()
        };

        publish("pre", 3);
        publish("p1", 10);
        let mid = hub.subscribe(); // mid-flight: snapshot = pre + p1
        publish("p2", 10);
        let late = hub.subscribe(); // after everything: whole run merged
        drop(hub);

        let live_events: Vec<AgentEvent> = live.collect().await;
        let truth = payloads(&live_events);
        let mut expected_truth = Vec::new();
        for (name, n) in [("pre", 3), ("p1", 10), ("p2", 10)] {
            for i in 0..n {
                expected_truth.push(payload(name, i));
            }
        }
        assert_eq!(truth, expected_truth, "live view is raw and lossless");

        // Mid-flight subscriber: ONE merged snapshot entry covering pre+p1,
        // then the p2 batch raw (it was published after the subscribe).
        let mid_events: Vec<AgentEvent> = mid.collect().await;
        assert_eq!(
            payloads(&mid_events),
            {
                let mut v = vec![batch("pre", 3) + &batch("p1", 10)];
                v.extend((0..10).map(|i| payload("p2", i)));
                v
            },
            "snapshot covers exactly pre+p1; p2 flows live, raw"
        );

        // Late subscriber: the whole run as a single merged entry.
        let late_events: Vec<AgentEvent> = late.collect().await;
        assert_eq!(payloads(&late_events), vec![truth.concat()]);
    }

    #[tokio::test]
    async fn concurrent_publishers_are_lossless_for_live_and_snapshot_views() {
        let hub = std::sync::Arc::new(EventHub::new());

        // Raw ground truth, subscribed before the racing publishers start.
        let live = hub.subscribe();

        // Two publishers release together and interleave arbitrarily; the
        // log's merged tail must still equal the live view's arrival order.
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for name in ["p1", "p2"] {
            let hub = hub.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                for i in 0..10 {
                    hub.publish(AgentEvent::TextDelta(format!("{name}-{i}")));
                }
            }));
        }
        barrier.wait().await;
        for h in handles {
            h.await.expect("publisher task");
        }

        let late = hub.subscribe();
        drop(hub);

        let live_events: Vec<AgentEvent> = live.collect().await;
        let truth = payloads(&live_events);
        assert_eq!(truth.len(), 20, "no duplicates, no gaps in the live view");
        for name in ["p1", "p2"] {
            let seq: Vec<String> = truth
                .iter()
                .filter(|p| p.starts_with(name))
                .cloned()
                .collect();
            let expected: Vec<String> = (0..10).map(|i| format!("{name}-{i}")).collect();
            assert_eq!(seq, expected, "{name} events must be complete and in order");
        }

        let late_events: Vec<AgentEvent> = late.collect().await;
        assert_eq!(payloads(&late_events), vec![truth.concat()]);
    }

    #[tokio::test]
    async fn log_merges_consecutive_deltas_but_not_across_other_events() {
        let hub = EventHub::new();
        hub.publish(AgentEvent::TextDelta("a".into()));
        hub.publish(AgentEvent::TextDelta("b".into()));
        hub.publish(AgentEvent::Finish);
        hub.publish(AgentEvent::TextDelta("c".into()));
        hub.publish(AgentEvent::ReasoningDelta("r1".into()));
        hub.publish(AgentEvent::ReasoningDelta("r2".into()));

        let sub = hub.subscribe();
        drop(hub);
        let events: Vec<AgentEvent> = sub.collect().await;
        // "b" merges into "a"; Finish breaks the run so "c" starts a new
        // entry; reasoning deltas merge separately.
        assert_eq!(
            events,
            vec![
                AgentEvent::TextDelta("ab".into()),
                AgentEvent::Finish,
                AgentEvent::TextDelta("c".into()),
                AgentEvent::ReasoningDelta("r1r2".into()),
            ]
        );
    }

    #[tokio::test]
    async fn tool_output_deltas_merge_only_for_same_call_and_stream() {
        use crate::tools::OutputStream;

        let delta = |call_id: &str, stream: OutputStream, text: &str| {
            AgentEvent::ToolOutputDelta {
                call_id: call_id.to_string(),
                stream,
                delta: text.to_string(),
            }
        };

        let hub = EventHub::new();
        hub.publish(delta("call-1", OutputStream::Stdout, "abc"));
        hub.publish(delta("call-1", OutputStream::Stdout, "def")); // same call+stream → merge
        hub.publish(delta("call-1", OutputStream::Stderr, "e")); // different stream → new entry
        hub.publish(delta("call-2", OutputStream::Stdout, "x")); // different call → new entry
        hub.publish(delta("call-1", OutputStream::Stdout, "y")); // tail is call-2 → no merge

        let sub = hub.subscribe();
        drop(hub);
        let events: Vec<AgentEvent> = sub.collect().await;
        let texts: Vec<(OutputStream, String)> = events
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::ToolOutputDelta { delta, stream, .. } => Some((stream, delta)),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                (OutputStream::Stdout, "abcdef".into()),
                (OutputStream::Stderr, "e".into()),
                (OutputStream::Stdout, "x".into()),
                (OutputStream::Stdout, "y".into()),
            ]
        );
    }

    #[tokio::test]
    async fn merged_tool_output_entry_is_capped_from_the_front() {
        use crate::tools::OutputStream;

        let hub = EventHub::new();
        let head = "a".repeat(30_000);
        let tail = "b".repeat(5_000);
        let publish = |text: String| AgentEvent::ToolOutputDelta {
            call_id: "call-1".to_string(),
            stream: OutputStream::Stdout,
            delta: text,
        };
        hub.publish(publish(head));
        hub.publish(publish(tail));

        let sub = hub.subscribe();
        drop(hub);
        let events: Vec<AgentEvent> = sub.collect().await;
        let AgentEvent::ToolOutputDelta { delta, .. } = &events[0] else {
            panic!("expected a single merged ToolOutputDelta");
        };
        assert_eq!(delta.len(), REPLAY_TOOL_OUTPUT_MAX_CHARS, "capped entry");
        assert!(
            delta.ends_with(&"b".repeat(5_000)),
            "newest output is retained"
        );
        assert!(
            delta.starts_with(&"a".repeat(30_000 - 2_232)),
            "oldest output beyond the cap is trimmed from the front"
        );
    }

    #[tokio::test]
    async fn live_subscribers_receive_every_event_unmerged() {
        // The merge only compacts the replay log; live subscribers still see
        // the raw per-event stream.
        let hub = EventHub::new();
        let sub = hub.subscribe();
        hub.publish(AgentEvent::TextDelta("a".into()));
        hub.publish(AgentEvent::TextDelta("b".into()));
        drop(hub);
        let events: Vec<AgentEvent> = sub.collect().await;
        assert_eq!(
            events,
            vec![
                AgentEvent::TextDelta("a".into()),
                AgentEvent::TextDelta("b".into()),
            ]
        );
    }

    #[tokio::test]
    async fn multiple_subscribers_are_independent_and_dead_ones_are_dropped() {
        let hub = std::sync::Arc::new(EventHub::new());
        hub.publish(AgentEvent::TextDelta("e1".into()));

        let early = hub.subscribe(); // live for both events
        let late = hub.subscribe(); // snapshot for e1, live for e2

        // A subscriber that immediately goes away must be cleaned up on the
        // next publish without disturbing the others.
        drop(hub.subscribe());

        hub.publish(AgentEvent::TextDelta("e2".into()));
        drop(hub);

        let early: Vec<AgentEvent> = early.collect().await;
        let late: Vec<AgentEvent> = late.collect().await;
        let expected = vec![
            AgentEvent::TextDelta("e1".into()),
            AgentEvent::TextDelta("e2".into()),
        ];
        assert_eq!(early, expected);
        assert_eq!(late, expected);
    }
}
