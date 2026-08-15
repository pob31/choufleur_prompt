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

/// Bumped whenever the page needs something this binary might not have.
///
/// The page is served from disk and reloads in a second; the binary is not and does
/// not. That gap is useful — it is what makes the display iterable during a two-hour
/// run — and it is also a trap, because a page can quietly be newer than the server it
/// is talking to. Observed: a flag control that rendered perfectly, sent `flag_line`
/// into a server built before the message existed, and did nothing at all, with the
/// page giving no hint that anything was wrong.
///
/// So the page carries the version it was written against and says so on screen when
/// they disagree. A silent no-op is the one failure mode worth spending a field on.
pub const PROTOCOL_VERSION: u32 = 1;

/// One script line, as the client needs it. Sent once, in bulk.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineView {
    pub id: String,
    pub character: String,
    pub text: String,
    pub scene: String,
    pub cut: bool,
    /// `"dialogue"` or `"stage"`.
    pub kind: String,
    /// Whether anyone says it out loud — which for a stage direction is a real
    /// question, not a formality. See `ScriptLine::spoken`.
    pub spoken: bool,
    /// `"silence"`, `"music"`, `"adlib"`, or absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold: Option<String>,
    /// Bookmarked by the operator. See `ScriptLine::flag`.
    pub flag: bool,
}

/// Where the show is. `None` before the first fix.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub seq: u64,
    pub line_index: usize,
    pub confidence: Conf,
    pub t_audio: f64,
    /// Which tracker said so. `"a"` is the one steering recognition.
    ///
    /// Two matchers on one recognition stream cost almost nothing — matching is
    /// microseconds, the audio is the expensive part and is shared — and a table of
    /// totals cannot show what an operator wants to know: not which config wins over
    /// two hours, but which one is *ahead at this moment*, and whether the other is
    /// merely slower or actually elsewhere.
    pub lane: &'static str,
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
        /// No audio, no engine — the page is here to be edited, not watched.
        prep: bool,
    },
    PositionUpdate(Position),
    /// A line was corrected; every client should update its copy.
    LineEdited {
        line_index: usize,
        text: String,
        character: Option<String>,
        cut: bool,
        kind: String,
        spoken: bool,
        hold: Option<String>,
    },
    /// A line was added. Every index at or after `line_index` has moved by one, so
    /// the client rebuilds rather than patching — an off-by-one here puts the whole
    /// second half of the page on the wrong text.
    LineInserted {
        line_index: usize,
        line: LineView,
    },
    /// A line was flagged or unflagged.
    ///
    /// Its own message rather than a field on `LineEdited`, because it is the one
    /// correction that has to be possible *during* a show — the moment you notice a
    /// line is wrong is exactly the moment you cannot stop to retype it — and because
    /// it changes no text, so nothing has to be re-read or re-matched.
    LineFlagged { line_index: usize, flag: bool },
    /// A line was removed for good — an import artefact, not an editing choice. Like
    /// an insert, every later index has moved, so the client rebuilds.
    LineDeleted { line_index: usize },
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
        /// Why the hallucination filter threw this away, if it did.
        ///
        /// Distinct from "the tracker did not match it", and conflating the two makes
        /// the panel unreadable at the moment it matters most: a recogniser reciting
        /// the prompt in a loop and a recogniser being ignored look identical unless
        /// the display says which happened.
        filtered: Option<String>,
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
    /// Prep mode: the script is served for editing and nothing is running.
    ///
    /// Everything the operator has to set — which lines are cut, which are stage
    /// directions, which of those are read aloud, where tracking should wait for
    /// music — is script preparation, and preparation does not happen during a show.
    /// It happens at a table, days earlier, with no sound card and no two-hour wait
    /// to see the passage you are working on.
    pub prep: bool,
}

impl LiveState {
    pub fn hello(&self) -> Update {
        Update::Hello {
            protocol: PROTOCOL_VERSION,
            title: self.title.clone(),
            line_count: self.lines.lock().unwrap().len(),
            position: *self.latest.lock().unwrap(),
            prep: self.prep,
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
    /// An optional second matcher, fed the same segments and reported alongside.
    ///
    /// It never answers `decode_hint`: recognition can only be steered by one of
    /// them, and letting the rival influence the audio it is being judged on would
    /// make the comparison meaningless.
    pub rival: Option<&'a mut dyn PositionSource>,
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
            if let Some(r) = self.rival.as_mut() {
                // An operator correction is about the show, not about a matcher, so
                // both are told. Racing them from different positions would compare
                // recovery from a handicap rather than the thing under test.
                r.steer_to(line);
            }
        }
        self.inner.on_segment(record);
        if let Some(r) = self.rival.as_mut() {
            r.on_segment(record);
        }

        *self.state.t_audio.lock().unwrap() = record.t_end;
        let (line_index, confidence) = self.inner.position();
        let pos = Position {
            seq: self.seq,
            line_index,
            confidence: confidence.into(),
            t_audio: record.t_end,
            lane: "a",
        };
        self.seq += 1;
        *self.state.latest.lock().unwrap() = Some(pos);
        if let Some(r) = self.rival.as_ref() {
            let (ri, rc) = r.position();
            let _ = self.state.tx.send(Update::PositionUpdate(Position {
                seq: self.seq,
                line_index: ri,
                confidence: rc.into(),
                t_audio: record.t_end,
                lane: "b",
            }));
        }
        if !record.text.trim().is_empty() {
            let _ = self.state.tx.send(Update::Heard {
                text: record.text.clone(),
                t_audio: record.t_end,
                matched: line_index != before,
                interim: record.interim,
                filtered: record.filtered.clone(),
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
            lane: "a",
        });
        let s = serde_json::to_string(&u).unwrap();
        assert!(s.contains(r#""lane":"a""#), "{s}");
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
                kind: "dialogue".into(),
                spoken: true,
                hold: None,
                flag: false,
            }]),
            latest: Mutex::new(None),
            t_audio: Mutex::new(0.0),
            running: Mutex::new(true),
            tx,
            audible_until: Mutex::new(f64::INFINITY),
            steer: Mutex::new(Vec::new()),
            prep: false,
        };
        let s = serde_json::to_string(&state.hello()).unwrap();
        assert!(s.contains(r#""type":"hello""#), "{s}");
        assert!(s.contains(r#""lineCount":1"#), "{s}");
        assert!(s.contains(r#""position":null"#), "{s}");
        assert!(s.contains(r#""prep":false"#), "{s}");
    }

    #[test]
    fn a_stage_direction_crosses_the_wire_with_its_voicing_and_its_hold() {
        // Three separate facts about one line, and this show is why they cannot be
        // collapsed: a didascalie may be read aloud (Euripides', which Séphora
        // performs) or never voiced (Rodrigues'), and either may or may not be a
        // point where tracking should wait.
        let u = Update::LineEdited {
            line_index: 3,
            text: "Chorégraphie sur la musique d'Otis Redding.".into(),
            character: None,
            cut: false,
            kind: "stage".into(),
            spoken: false,
            hold: Some("music".into()),
        };
        let s = serde_json::to_string(&u).unwrap();
        assert!(s.contains(r#""kind":"stage""#), "{s}");
        assert!(s.contains(r#""spoken":false"#), "{s}");
        assert!(s.contains(r#""hold":"music""#), "{s}");
    }

    #[test]
    fn a_flag_is_its_own_message_so_it_can_be_set_during_a_show() {
        // Not a field on LineEdited: flagging has to work mid-run, from a page whose
        // copy of the text may be seconds stale, and writing the whole line back to
        // record a bookmark is how a stale tab overwrites a correction.
        let s = serde_json::to_string(&Update::LineFlagged {
            line_index: 411,
            flag: true,
        })
        .unwrap();
        assert!(s.contains(r#""type":"line_flagged""#), "{s}");
        assert!(s.contains(r#""lineIndex":411"#), "{s}");
        assert!(s.contains(r#""flag":true"#), "{s}");
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
