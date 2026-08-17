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
pub const PROTOCOL_VERSION: u32 = 8;

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
    /// Who has bookmarked this line, and when. See `choufleur_core::script::Flag`.
    pub flags: Vec<choufleur_core::script::Flag>,
}

/// What a cue list is *for*, and the vocabulary it uses.
///
/// A cue sheet belongs to a department, and departments do not share tools. This one
/// reads blue as QLab and purple as the console because it is a sound sheet; a
/// lighting sheet would mean an Eos or a grandMA by the same marks, video something
/// else again. Hard-coding "QLab" into the editor was a sound engineer's accident that
/// happened to be the first sheet through the door.
///
/// So the categories travel with the list. Taken from an explicit `categories` array
/// when the sheet has one, and otherwise derived from the colours actually in use and
/// what the sheet's own `colourMeans` says they signify — which is the operator's
/// notation, already written down, and not ours to invent.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CueCategory {
    /// The value stored on a cue, historically a colour name.
    pub key: String,
    /// What it means to this department: "QLab", "console", "Eos", "followspot".
    pub label: String,
    /// What to paint it, as CSS.
    pub swatch: String,
}

/// One operator's cue list: their document, in their notation.
///
/// A first-class thing, and **deliberately not** what notation §6.2 and §9 describe.
/// The spec models this as a single shared cue array partitioned by a `cueTypes`
/// registry, with each operator filtering on type. From the chair: *"The cue lists are
/// independent. Each operator should take their notes as they prefer."*
///
/// They are right and the spec should be amended. A cue list is one person's working
/// document: its colours mean what they decided they mean, its wording is their
/// shorthand, and what even counts as a cue is their judgement. This production's sound
/// sheet says `2 Mute micros / lumière 11 / Musique` — a lighting state written into a
/// sound cue because that was useful *to the sound operator*. A shared array with a
/// shared vocabulary makes one operator's conventions structurally visible to another
/// and forces a common language on people who do not have one.
///
/// So: **the script and the position are shared; everything else belongs to a list.**
/// Two lists may both contain a `Q-0007` and never collide, because a cue id is only
/// ever resolved within its own sheet.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CueSheet {
    /// Stable, from the file's stem. How a client says which list it is showing.
    pub id: String,
    /// What the operator calls it: "Conduite SON", "LX".
    pub name: String,
    /// This list's own vocabulary. Personal by construction — renaming a target on one
    /// list can never touch another.
    pub categories: Vec<CueCategory>,
}

/// One cue, as the rail needs it.
///
/// A projection of the cue sheet, not the sheet itself: the page needs to know where a
/// cue sits, what it says and how to colour it, and nothing else. Provenance — which
/// page of the PDF it came from, what evidence placed it there, how confident the
/// extraction was — stays on disk where an argument about a misplaced cue can be
/// settled, and off a screen somebody is reading at a desk in the dark.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CueView {
    /// Stable across runs and across re-sorting; see `load_cues`.
    ///
    /// Unique within its sheet, not across sheets: see [`CueSheet`].
    pub id: String,
    /// Which list this cue belongs to.
    pub sheet: String,
    /// Position in the script. Cues arrive sorted by it.
    pub line_index: usize,
    pub line_id: String,
    /// What the operator has to do.
    pub cue: String,
    /// `"text"` — fires on a line being reached — or `"visual"`, which the system
    /// cannot detect and the operator must watch for. The distinction matters more
    /// than any other on this rail: one is a prompt, the other is a responsibility.
    pub trigger: String,
    /// The operator's own colour, and their own meaning for it. This production reads
    /// blue as QLab and purple as the console; another will not, so the meanings ride
    /// along with the sheet rather than being built in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub means: Option<String>,
    /// The exact phrase in the line that fires this cue, when the sheet's highlight
    /// identifies one. Absent for cues extracted from a page-level mark, where the
    /// honest answer is the whole line rather than a guessed fragment of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_text: Option<String>,
    /// Which occurrence of that phrase within the line — 0 for the first.
    ///
    /// A play repeats itself: "Comme ça." three times in one speech, and a cue on the
    /// third is not a cue on the first. Carried as a count rather than a character
    /// offset so it survives an edit elsewhere in the line, and so the two sides need
    /// not agree on what an index into a string means.
    pub trigger_nth: usize,
    /// Placed by the re-anchoring tool but not confidently. Shown as such.
    pub needs_review: bool,
    /// `"operator"` — the owner of this list presses go — or `"auto"`, meaning a
    /// machine does it and the human is on standby.
    ///
    /// Reported from the production: QLab sent OSC to the lighting desk so transitions
    /// stayed in sync, *"but the light operator was always on standby in case something
    /// didn't go right."* Nothing else in the system records who acts, so a cue that
    /// fires itself was indistinguishable from one waiting to be pressed.
    ///
    /// This does not break warn-only semantics (notation §2.4: *"Nothing in this
    /// notation can express 'trigger'"*). That rule forbids the format instructing
    /// hardware. This instructs nothing and reaches no machine — it records who acts in
    /// the room, which is exactly what a standby operator needs warning about. It
    /// changes the wording of a warning, never its nature.
    pub fired_by: String,
    /// How it fires, in the operator's words: "OSC from QLab". Free text, shown
    /// verbatim, never parsed — the moment this is parsed it has become show control.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
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
        /// The cue lists this show carries, so a client can offer the picker without a
        /// second fetch.
        sheets: Vec<CueSheet>,
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
    /// The cue sheet changed. The whole set is re-sent rather than a delta.
    ///
    /// A cue edit can move it to another line, which re-sorts the rail, or remove it,
    /// which renumbers nothing but invalidates every index the page was holding. At
    /// 133 cues the whole list is a few kilobytes and arrives once per edit — a delta
    /// protocol here would be three more message types and a class of bug for no gain.
    CuesChanged {
        /// Which list changed. A screen showing another one ignores this entirely —
        /// the lists are independent, so an edit to the lighting sheet is not news to
        /// the sound operator and must not redraw their rail.
        sheet: String,
        cues: Vec<CueView>,
        /// Sent alongside, always. The categories decide how every cue is painted, so
        /// a client holding one and not the other would draw the sheet in colours it
        /// no longer uses — and the two change together often enough that a separate
        /// message would only be a way for them to disagree.
        list: CueSheet,
    },
    /// A line was flagged or unflagged.
    ///
    /// Its own message rather than a field on `LineEdited`, because it is the one
    /// correction that has to be possible *during* a show — the moment you notice a
    /// line is wrong is exactly the moment you cannot stop to retype it — and because
    /// it changes no text, so nothing has to be re-read or re-matched.

    /// The cast changed — declared, renamed, grouped or merged.
    ///
    /// A merge rewrites every line that named the old speaker, so `reload` asks clients
    /// to re-read the script rather than the server sending one edit per line. It is a
    /// prep-time operation on a page that is not following anything, and a quarter of a
    /// megabyte is cheaper than nine hundred messages.
    /// The audio patch changed — a different input device, or a channel repatched.
    ///
    /// Broadcast so a second screen showing the patch does not go stale, and because
    /// noticing that somebody else just repatched an input is worth more during a fit-up
    /// than almost anything else this protocol carries.
    VenueChanged {
        venue: serde_json::Value,
    },
    /// Per-channel levels while capture is running.
    ///
    /// The PRD's `input_health`. A patched input carrying nothing and a patched input
    /// carrying the wrong thing look identical on paper; this is the difference, and it
    /// is the first thing capture can tell anybody — long before a word is recognised.
    InputHealth {
        channels: Vec<crate::capture::Health>,
    },
    /// Tracking suspended or resumed.
    Paused {
        paused: bool,
    },
    /// Something the operator asked for did not work, in words they can act on.
    Trouble {
        what: String,
    },
    CastChanged {
        cast: Vec<CharacterView>,
        reload: bool,
    },
   LineFlagged {
        line_index: usize,
        flags: Vec<choufleur_core::script::Flag>,
    },
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


/// The cast, as the cast panel needs it.
///
/// A character is *declared* when it appears here. That is not bookkeeping: only a
/// declared character can be put on a microphone, and only a declared one can be named
/// in another's `members`. A line whose speaker is not in this list still shows and
/// still matches — it simply cannot be patched, which is why Hécube runs perfectly well
/// with 81 lines pointing at 23 speakers it never declares.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterView {
    pub id: String,
    pub name: String,
    /// The characters this one is made of, when it is a group. See
    /// [`choufleur_core::script::Character::members`].
    pub members: Vec<String>,
    /// Which logical channels carry this performer.
    ///
    /// Its absence was a good bug: the panel sent an assignment, the server saved it,
    /// and the reply came back without it — so the field emptied itself a moment after
    /// being filled and the patch looked like it had not taken.
    pub channels: Vec<u16>,
}

// What the HTTP side needs to answer a new connection.
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
    /// The cue sheet, projected for the rail. Empty when there is none.
    pub cues: Mutex<Vec<CueView>>,
    /// Where each list's edits are written, by sheet id. Resolved per edit rather than
    /// held as one path: writing a lighting correction into the sound sheet is this
    /// feature's characteristic failure.
    pub sheet_paths: Mutex<std::collections::HashMap<String, std::path::PathBuf>>,
    /// Every cue list this show carries, each owning its own file. See [`CueSheet`].
    pub sheets: Mutex<Vec<CueSheet>>,
    /// The declared cast. See [`CharacterView`].
    pub cast: Mutex<Vec<CharacterView>>,
    /// Tracking suspended, without ending the run.
    ///
    /// Shared with the engine rather than read from it, because the engine is one
    /// blocking thread that cannot be reached from the socket — the same arrangement as
    /// `steer`, for the same reason.
    pub paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The input stream, while somebody is listening. `None` means nothing is open —
    /// which is most of the time, because holding an interface open is rude to whatever
    /// else the operator has running.
    pub capture: Mutex<Option<crate::capture::Capture>>,
    /// Where the patch lives. The venue belongs to the machine, not to the show — see
    /// [`crate::audio`] — so it is found beside the library rather than in the show.
    pub library: std::path::PathBuf,
    /// The microphones this show has, as the manifest describes them — index, the note
    /// the corpus author left, and the file. What the cast panel offers to assign.
    pub channels: Vec<serde_json::Value>,
    /// The one gate every write to this show passes through.
    ///
    /// Held here rather than reached for per-write because its whole value is being
    /// shared: it is what serialises two operators' edits, remembers what the files
    /// looked like when we read them, and takes the session's snapshot before the
    /// first change. A second `Store` over the same show would defeat all three.
    pub store: choufleur_server::Store,
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
            sheets: self.sheets.lock().unwrap().clone(),
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
                flags: Vec::new(),
            }]),
            latest: Mutex::new(None),
            t_audio: Mutex::new(0.0),
            running: Mutex::new(true),
            tx,
            audible_until: Mutex::new(f64::INFINITY),
            steer: Mutex::new(Vec::new()),
            cues: Mutex::new(Vec::new()),
            sheet_paths: Mutex::new(std::collections::HashMap::new()),
            cast: Mutex::new(Vec::new()),
            channels: Vec::new(),
            library: std::path::PathBuf::new(),
            capture: Mutex::new(None),
            paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: choufleur_server::Store::new(".", Vec::new()),
            sheets: Mutex::new(Vec::new()),
            prep: false,
        };
        let s = serde_json::to_string(&state.hello()).unwrap();
        assert!(s.contains(r#""type":"hello""#), "{s}");
        assert!(s.contains(r#""lineCount":1"#), "{s}");
        assert!(s.contains(r#""position":null"#), "{s}");
        assert!(s.contains(r#""prep":false"#), "{s}");
        assert!(s.contains(r#""sheets":[]"#), "{s}");
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
            flags: vec![choufleur_core::script::Flag {
                by: "Conduite SON".into(),
                at: "2026-08-16T22:14:00Z".into(),
            }],
        })
        .unwrap();
        assert!(s.contains(r#""type":"line_flagged""#), "{s}");
        assert!(s.contains(r#""lineIndex":411"#), "{s}");
        // Attributed to the *list*, which is the position — no operator identity needed.
        assert!(s.contains(r#""by":"Conduite SON""#), "{s}");
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
