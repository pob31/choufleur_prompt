//! The live spike's wire protocol and its broadcast plumbing.
//!
//! **This is not M2.2.** The real server is a separate `choufleur-server` axum binary
//! speaking the full typed protocol over show files, with auth, run controls and a
//! journal. This is a deliberately thin slice bolted to the replay harness so the
//! display can be *watched* — which is the one thing every Phase 0 measurement cannot
//! tell us — months before Phase 2 lands. It should be deleted at M2.2.
//!
//! Two rules are kept from the real protocol so this is a prefix of it rather than a
//! detour: messages carry **typed events and parameters, never display prose** (the
//! client renders its own words, so nothing localized crosses the wire), and every
//! connection opens with a full-state `hello` so a reconnecting client resyncs the
//! same way it started.
//!
//! Position is **state, not a log**. A slow or reconnecting client wants to know where
//! the show is now, never where it has been, so the channel is lossy on purpose and a
//! late joiner is caught up by `hello` rather than by replay.

use std::sync::{Arc, Mutex};

use choufleur_core::tracker::Confidence;
use serde::Serialize;

use crate::engine::Consumer;
use crate::formats::SegmentRecord;

pub const PROTOCOL_VERSION: u32 = 0;

/// One script line, as the client needs it. Sent once, in bulk.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineView {
    pub id: String,
    pub character: String,
    pub text: String,
    pub scene: String,
    pub cut: bool,
}

/// Where the show is. `None` before the first fix.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub seq: u64,
    pub line_index: usize,
    pub confidence: Conf,
    pub t_audio: f64,
}

/// The tracker's ladder, on the wire. Lower-case so the client can use it as a class
/// name directly, and an enum rather than a number because a percentage would be a
/// claim about calibration that has not been measured.
#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Conf {
    Lost,
    Scene,
    Block,
    Line,
    Word,
}

impl From<Confidence> for Conf {
    fn from(c: Confidence) -> Self {
        match c {
            Confidence::Lost => Conf::Lost,
            Confidence::Scene => Conf::Scene,
            Confidence::Block => Conf::Block,
            Confidence::Line => Conf::Line,
            Confidence::Word => Conf::Word,
        }
    }
}

/// `rename_all` on an enum renames the *variants*; the fields inside them need
/// `rename_all_fields`. Without the second one `hello` went out carrying `line_count`
/// while the page read `lineCount`, and every client would have joined believing the
/// show had no lines — silently, since a missing field is just `undefined`.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Update {
    /// Full state, on connect and on reconnect.
    Hello {
        protocol: u32,
        title: Option<String>,
        line_count: usize,
        position: Option<Position>,
    },
    PositionUpdate(Position),
    /// A line was corrected; every client should update its copy.
    LineEdited {
        line_index: usize,
        text: String,
        character: Option<String>,
        cut: bool,
    },
    /// What the recogniser actually heard, and what became of it.
    ///
    /// Asked for by the operator on seeing the tracker lost: *"is it possible to
    /// display the text it finds, to see how the analysis understands the text?"* It
    /// is the difference between a display that is silently wrong and one whose
    /// reasoning can be read — on a bad channel the transcript is visibly garbled, and
    /// no amount of staring at a position can tell you that.
    Heard {
        text: String,
        t_audio: f64,
        matched: bool,
        interim: bool,
    },
    /// Heartbeat for the footer, and the only way the page can tell that a run which
    /// has gone quiet is still running rather than wedged.
    RunState { t_audio: f64, running: bool },
}

/// What the HTTP side needs to answer a new connection.
pub struct LiveState {
    pub title: Option<String>,
    /// Mutable because the operator can correct them from the page.
    ///
    /// A script imported from a document carries the importer's mistakes — an
    /// attribution read as dialogue, a stage direction that became a line — and they
    /// are invisible until somebody watches the show against them. The person
    /// watching is exactly the person who can fix them, so they can, and the
    /// correction is written back to the script on disk rather than living in a
    /// browser tab.
    pub lines: Mutex<Vec<LineView>>,
    /// Where to write those corrections.
    pub script_path: std::path::PathBuf,
    pub latest: Mutex<Option<Position>>,
    pub t_audio: Mutex<f64>,
    pub running: Mutex<bool>,
    pub tx: tokio::sync::broadcast::Sender<Update>,
    /// How far into the show the speaker has actually got, in audio seconds.
    ///
    /// The recogniser is handed a block a fraction of a second before the same block
    /// reaches the ear, so without this the screen can announce a line *before* it is
    /// audible — which was the first thing an operator noticed, and is the one error
    /// that makes the display feel wrong even when the tracking is right. An update is
    /// therefore held until its audio has been heard. It may lag; it may never lead.
    ///
    /// `INFINITY` when nothing is playing, which releases everything immediately.
    pub audible_until: Mutex<f64>,
    /// Corrections from an operator, waiting for the engine to pick them up.
    ///
    /// The GPS does not argue with the driver. When the tracker is lost — and finding
    /// R says that when it is wrong it is usually *badly* wrong rather than nearly
    /// right — the person in the room can see the page and knows the answer, and
    /// waiting for the matcher to rediscover it is the worst option available. So the
    /// operator can put a finger on the line, and the tracker takes their word for it.
    ///
    /// A queue rather than a direct call because the tracker belongs to the engine
    /// thread, which must never be interrupted mid-decode.
    pub steer: Mutex<Vec<usize>>,
}

impl LiveState {
    pub fn hello(&self) -> Update {
        Update::Hello {
            protocol: PROTOCOL_VERSION,
            title: self.title.clone(),
            line_count: self.lines.lock().unwrap().len(),
            position: *self.latest.lock().unwrap(),
        }
    }
}

/// A consumer that also knows where the show is.
///
/// `Consumer` reports segments, not positions, so publishing needs one extra thing
/// from the tracking consumer. A supertrait rather than a second trait object,
/// because `dyn Consumer + PositionSource` is not a thing Rust will build.
pub trait PositionSource: Consumer {
    fn position(&self) -> (usize, Confidence);
    /// Place the position by hand. The operator is the authority here, so this is
    /// accepted at `Line` confidence: they can see the page.
    fn steer_to(&mut self, line_index: usize);
}

/// Wraps the tracking consumer and publishes what it decides.
///
/// A pure passthrough by construction: every call is forwarded and nothing is
/// altered, so a run with this inserted must produce byte-identical output to one
/// without it. That is asserted in the tests, because the whole value of the spike
/// depends on it showing the tracker's real behaviour rather than a variant of it.
pub struct Broadcast<'a> {
    pub inner: &'a mut dyn PositionSource,
    pub state: Arc<LiveState>,
    pub seq: u64,
}

impl Consumer for Broadcast<'_> {
    fn decode_hint(
        &mut self,
        channel: u16,
        character: Option<&str>,
    ) -> crate::engine::DecodeHint {
        self.inner.decode_hint(channel, character)
    }

    fn on_segment(&mut self, record: &SegmentRecord) {
        let before = self.inner.position().0;
        // Applied before the segment, so a correction takes effect on the very next
        // thing said rather than after one more mismatch has been scored against the
        // place the operator has just told us we are not.
        let pending: Vec<usize> = std::mem::take(&mut *self.state.steer.lock().unwrap());
        for line in pending {
            self.inner.steer_to(line);
        }
        self.inner.on_segment(record);

        *self.state.t_audio.lock().unwrap() = record.t_end;
        let (line_index, confidence) = self.inner.position();
        let pos = Position {
            seq: self.seq,
            line_index,
            confidence: confidence.into(),
            t_audio: record.t_end,
        };
        self.seq += 1;
        *self.state.latest.lock().unwrap() = Some(pos);
        if !record.text.trim().is_empty() {
            let _ = self.state.tx.send(Update::Heard {
                text: record.text.clone(),
                t_audio: record.t_end,
                matched: line_index != before,
                interim: record.interim,
            });
        }
        // Lossy on purpose, and a send with no subscribers is not an error: the run
        // must never wait on, or fail because of, whether anyone is watching.
        let _ = self.state.tx.send(Update::PositionUpdate(pos));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_serializes_with_a_type_tag_and_no_prose() {
        let u = Update::PositionUpdate(Position {
            seq: 7,
            line_index: 42,
            confidence: Conf::Line,
            t_audio: 12.5,
        });
        let s = serde_json::to_string(&u).unwrap();
        assert!(s.contains(r#""type":"position_update""#), "{s}");
        assert!(s.contains(r#""lineIndex":42"#), "{s}");
        assert!(s.contains(r#""confidence":"line""#), "{s}");
    }

    #[test]
    fn hello_carries_full_state() {
        let (tx, _rx) = tokio::sync::broadcast::channel(4);
        let state = LiveState {
            title: Some("Hécube".into()),
            script_path: std::path::PathBuf::from("/dev/null"),
            lines: Mutex::new(vec![LineView {
                id: "L-0001".into(),
                character: "char-eric".into(),
                text: "Silence, mes amies.".into(),
                scene: "sc-1".into(),
                cut: false,
            }]),
            latest: Mutex::new(None),
            t_audio: Mutex::new(0.0),
            running: Mutex::new(true),
            tx,
            audible_until: Mutex::new(f64::INFINITY),
            steer: Mutex::new(Vec::new()),
        };
        let s = serde_json::to_string(&state.hello()).unwrap();
        assert!(s.contains(r#""type":"hello""#), "{s}");
        assert!(s.contains(r#""lineCount":1"#), "{s}");
        assert!(s.contains(r#""position":null"#), "{s}");
    }

    #[test]
    fn every_confidence_maps_to_a_lower_case_name() {
        for (c, want) in [
            (Confidence::Lost, "lost"),
            (Confidence::Scene, "scene"),
            (Confidence::Block, "block"),
            (Confidence::Line, "line"),
            (Confidence::Word, "word"),
        ] {
            let got = serde_json::to_string(&Conf::from(c)).unwrap();
            assert_eq!(got, format!("\"{want}\""));
        }
    }
}
