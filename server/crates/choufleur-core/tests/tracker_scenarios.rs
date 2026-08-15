//! Scripted scenarios for the position tracker.
//!
//! Every one of these is a situation the PRD names explicitly — skipped lines,
//! paraphrase, improvisation, scene change, simultaneous speakers, mic bleed —
//! exercised against a toy script with hand-fed transcript segments. No audio, no
//! ASR, no clock: the tracker is a pure function of the segment sequence, which is
//! what makes these assertions stable enough to be a regression net.

use choufleur_core::lang::{LangCode, NormalizerRegistry};
use choufleur_core::script::{Character, Hold, LineKind, PreparedScript, Script, ScriptLine};
use choufleur_core::tracker::{
    Confidence, PositionCause, RejectReason, Tracker, TrackerConfig, TrackerEvent,
};
use choufleur_core::types::{AsrQuality, TranscriptSegment};

const A: &str = "char-a"; // MARIE, channel 1
const B: &str = "char-b"; // JEAN, channel 2

/// A 16-line two-hander in French, two scenes, one explicit landmark.
/// Lines are deliberately distinctive except for the two "Oui." beats, which
/// exist to exercise the ambiguity margin.
fn toy_script() -> Script {
    let rows: &[(&str, &str, &str, u8)] = &[
        ("sc-1", A, "Tu ne devrais pas être ici.", 0),
        ("sc-1", B, "Je sais, mais je suis venu quand même.", 0),
        ("sc-1", A, "Alors pars avant qu'il ne revienne.", 0),
        ("sc-1", B, "Oui.", 0),
        ("sc-1", A, "Il rentre du théâtre dans une heure à peine.", 0),
        ("sc-1", B, "Ne me demande pas ça maintenant.", 0),
        (
            "sc-1",
            A,
            "Les cerisiers ont brûlé pendant la nuit entière.",
            3,
        ),
        ("sc-1", B, "Oui.", 0),
        ("sc-1", A, "Tu n'as jamais rien compris à cette maison.", 0),
        ("sc-1", B, "Peut-être. Mais toi non plus, ma chère.", 0),
        ("sc-1", A, "Va-t'en. Je ne veux plus entendre ta voix.", 0),
        ("sc-1", B, "Comme tu voudras. Adieu donc.", 0),
        (
            "sc-2",
            A,
            "La lampe s'est éteinte vers quatre heures du matin.",
            0,
        ),
        ("sc-2", B, "Personne n'est venu la rallumer depuis.", 0),
        (
            "sc-2",
            A,
            "C'est ainsi que les choses finissent toujours.",
            0,
        ),
        ("sc-2", B, "Nous partirons demain par le premier train.", 0),
    ];
    Script {
        format: "choufleur-script".into(),
        format_version: "0.1".into(),
        title: Some("Toy".into()),
        default_lang: vec![LangCode::new("fr")],
        acts: vec![],
        scenes: vec![],
        characters: vec![
            Character {
                id: A.into(),
                name: "MARIE".into(),
                lang: None,
                channels: vec![1],
            },
            Character {
                id: B.into(),
                name: "JEAN".into(),
                lang: None,
                channels: vec![2],
            },
        ],
        lines: rows
            .iter()
            .enumerate()
            .map(|(i, (scene, ch, text, lm))| ScriptLine {
                flag: false,
                spoken: None,
                cut: false,
                kind: LineKind::Dialogue,
                hold: None,
                hold_seconds: None,
                id: format!("L-{:04}", i + 1),
                act: "act-1".into(),
                scene: (*scene).into(),
                character: (*ch).into(),
                text: (*text).into(),
                lang: None,
                landmark: *lm,
                alternates: Vec::new(),
            })
            .collect(),
    }
}

fn prepared(script: &Script) -> PreparedScript {
    let mut reg = NormalizerRegistry::with_defaults();
    PreparedScript::build(script, &mut reg)
}

struct SegBuilder {
    t: f64,
}

impl SegBuilder {
    fn new() -> Self {
        SegBuilder { t: 0.0 }
    }
    /// Next segment on `character`'s channel, `dur` seconds long.
    fn say(&mut self, character: Option<&str>, text: &str, dur: f64) -> TranscriptSegment {
        let t_start = self.t;
        self.t += dur + 0.5;
        TranscriptSegment {
            channel: match character {
                Some(A) => 1,
                Some(B) => 2,
                _ => 9,
            },
            character: character.map(str::to_string),
            t_start,
            t_end: t_start + dur,
            text: text.into(),
            langs: vec![LangCode::new("fr")],
            quality: AsrQuality {
                avg_logprob: -0.25,
                no_speech_prob: 0.02,
            },
            forced_split: false,
            interim: false,
        }
    }
}

fn position_of(events: &[TrackerEvent]) -> Option<(usize, Confidence, PositionCause)> {
    events.iter().find_map(|e| match e {
        TrackerEvent::Position {
            line_index,
            confidence,
            cause,
            ..
        } => Some((*line_index, *confidence, *cause)),
        _ => None,
    })
}

fn rejection_of(events: &[TrackerEvent]) -> Option<RejectReason> {
    events.iter().find_map(|e| match e {
        TrackerEvent::Rejected { reason, .. } => Some(*reason),
        _ => None,
    })
}

#[test]
fn follows_a_clean_run_line_by_line() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    for (i, line) in script.lines.iter().enumerate() {
        let events = tracker.update(&sb.say(Some(&line.character), &line.text, 2.5));
        let (idx, conf, _) = position_of(&events)
            .unwrap_or_else(|| panic!("line {i} produced no position: {events:?}"));
        assert_eq!(idx, i, "line {i} landed at {idx}");
        // "Oui." is weak evidence: it confirms where we are, but one word can
        // never amount to a word-level match.
        let expected = if line.text.split_whitespace().count() < 3 {
            Confidence::Line
        } else {
            Confidence::Word
        };
        assert_eq!(conf, expected, "line {i}: {:?}", line.text);
    }
    assert_eq!(
        tracker.position(),
        script.lines.len() - 1,
        "should end on the last line"
    );
}

#[test]
fn a_paraphrase_still_advances_but_only_to_line_confidence() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    // The actor reshapes the line — the PRD's "paraphrased line".
    let events = tracker.update(&sb.say(
        Some(B),
        "je sais bien mais je suis quand même passé te voir",
        2.6,
    ));
    let (idx, conf, cause) = position_of(&events).expect("paraphrase should still match");
    assert_eq!(idx, 1);
    assert_eq!(cause, PositionCause::Follow);
    assert_eq!(
        conf,
        Confidence::Line,
        "a paraphrase is not a word-level match"
    );
}

#[test]
fn skip_tolerance_advances_past_material_that_was_never_heard() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    tracker.update(&sb.say(Some(B), "Je sais, mais je suis venu quand même.", 2.5));
    tracker.update(&sb.say(Some(A), "Alors pars avant qu'il ne revienne.", 2.5));
    assert_eq!(tracker.position(), 2);
    // Line 3 ("Oui.") is swallowed and line 4 arrives next.
    let events =
        tracker.update(&sb.say(Some(A), "Il rentre du théâtre dans une heure à peine.", 3.0));
    let (idx, _, cause) = position_of(&events).expect("later material should be believed");
    assert_eq!(idx, 4);
    assert_eq!(
        cause,
        PositionCause::Skip,
        "a gap of 2 is skip tolerance, not a jump"
    );
}

#[test]
fn a_large_jump_is_believed_only_on_the_second_sighting() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    // The director restarts eight lines later. First sighting: held, not obeyed.
    let first =
        tracker.update(&sb.say(Some(A), "Tu n'as jamais rien compris à cette maison.", 3.0));
    assert_eq!(rejection_of(&first), Some(RejectReason::JumpPending));
    assert_eq!(
        tracker.position(),
        0,
        "position must not move on one distant match"
    );

    let second =
        tracker.update(&sb.say(Some(A), "Tu n'as jamais rien compris à cette maison.", 3.0));
    let (idx, _, cause) = position_of(&second).expect("a corroborated jump should commit");
    assert_eq!(idx, 8);
    assert_eq!(cause, PositionCause::Jump);
}

#[test]
fn position_never_moves_backward_on_its_own() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    for line in script.lines.iter().take(6) {
        tracker.update(&sb.say(Some(&line.character), &line.text, 2.5));
    }
    assert_eq!(tracker.position(), 5);

    // An actor repeats an early line. Forward-only means we do not follow.
    let events = tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    assert!(
        position_of(&events).is_none(),
        "backward match should not move position"
    );
    assert_eq!(tracker.position(), 5);
}

#[test]
fn improvisation_decays_confidence_to_block_then_lost() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    assert_eq!(tracker.confidence(), Confidence::Word);

    let improv = [
        "attends une seconde je crois qu'on a perdu la lumière côté jardin",
        "quelqu'un peut aller voir ce qui se passe avec le projecteur",
        "non non recommence depuis le début de la réplique s'il te plaît",
        "on reprend tout le monde en place pour la scène suivante",
    ];
    let mut levels = vec![];
    for text in improv {
        tracker.update(&sb.say(Some(B), text, 6.0));
        levels.push(tracker.confidence());
    }
    assert!(
        levels.contains(&Confidence::Block),
        "should pass through block: {levels:?}"
    );
    assert_eq!(
        *levels.last().unwrap(),
        Confidence::Lost,
        "sustained improv should be honest"
    );
    assert_eq!(
        tracker.position(),
        0,
        "confidence falls; position does not wander"
    );
}

#[test]
fn silence_alone_never_decays_confidence() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    sb.t += 600.0; // a ten-minute hold for notes
    let events = tracker.update(&sb.say(Some(B), "Je sais, mais je suis venu quand même.", 2.5));
    assert_eq!(position_of(&events).map(|(i, ..)| i), Some(1));
    assert_eq!(tracker.confidence(), Confidence::Word);
}

#[test]
fn a_landmark_re_anchors_after_tracking_is_lost() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    for _ in 0..4 {
        tracker.update(&sb.say(
            Some(B),
            "on ne sait plus du tout où on en est ici ce soir",
            6.0,
        ));
    }
    assert_eq!(tracker.confidence(), Confidence::Lost);

    // The weight-3 landmark at line 6 is unmistakable and far outside the window.
    let events = tracker.update(&sb.say(
        Some(A),
        "Les cerisiers ont brûlé pendant la nuit entière.",
        3.5,
    ));
    let (idx, _, cause) = position_of(&events).expect("landmark should re-anchor");
    assert_eq!(idx, 6);
    assert_eq!(cause, PositionCause::Reanchor);
    assert!(tracker.confidence() >= Confidence::Line);
}

#[test]
fn repeated_lines_are_reported_as_ambiguous_rather_than_guessed() {
    let script = toy_script();
    let p = prepared(&script);
    // Widen the window so both "Oui." lines are live candidates at once.
    let cfg = TrackerConfig {
        window_ahead: 12,
        ..TrackerConfig::default()
    };
    let mut tracker = Tracker::new(&p, cfg);
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    tracker.update(&sb.say(Some(B), "Je sais, mais je suis venu quand même.", 2.5));
    tracker.update(&sb.say(Some(A), "Alors pars avant qu'il ne revienne.", 2.5));
    let events = tracker.update(&sb.say(Some(B), "Oui.", 0.6));
    // Either it is confidently the near "Oui." or it is honestly ambiguous —
    // what it must never be is a silent guess at the far one.
    match (position_of(&events), rejection_of(&events)) {
        (Some((idx, ..)), _) => assert_eq!(idx, 3, "if it commits, it must be the near one"),
        (None, Some(reason)) => {
            assert!(matches!(
                reason,
                RejectReason::Ambiguous | RejectReason::WeakEvidence
            ))
        }
        (None, None) => panic!("expected either a position or a stated rejection"),
    }
    assert!(tracker.position() < 7, "never the far duplicate");
}

#[test]
fn a_zone_channel_matches_any_speaker() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    // An ambient mic: no character identity at all (PRD, ambient/area mics).
    let events = tracker.update(&sb.say(None, "Tu ne devrais pas être ici.", 2.0));
    assert_eq!(position_of(&events).map(|(i, ..)| i), Some(0));
    let events = tracker.update(&sb.say(None, "Je sais, mais je suis venu quand même.", 2.5));
    assert_eq!(position_of(&events).map(|(i, ..)| i), Some(1));
}

#[test]
fn mic_bleed_cannot_advance_the_position() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    let seg = sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0);
    tracker.update(&seg);
    // The same words arrive a moment later on JEAN's mic — spill, not delivery.
    // Per-channel identity handles this structurally: MARIE's lines are simply
    // not candidates for JEAN's channel, so the spill has nowhere to land. The
    // only route by which another character's line stays reachable is a landmark
    // span, and there the character-mismatch penalty puts it below threshold.
    let mut bleed = sb.say(Some(B), "Tu ne devrais pas être ici.", 2.0);
    bleed.t_start = seg.t_start + 0.1;
    bleed.t_end = seg.t_end + 0.1;
    let events = tracker.update(&bleed);
    assert!(
        position_of(&events).is_none(),
        "bleed must not move position: {events:?}"
    );
    assert_eq!(tracker.position(), 0);
}

#[test]
fn a_landmark_spoken_on_the_wrong_channel_does_not_re_anchor() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    // MARIE's weight-3 landmark, heard on JEAN's channel. A landmark is a strong
    // anchor but not strong enough to override who is holding the microphone.
    let events = tracker.update(&sb.say(
        Some(B),
        "Les cerisiers ont brûlé pendant la nuit entière.",
        3.5,
    ));
    assert!(
        position_of(&events).is_none(),
        "wrong channel must not re-anchor: {events:?}"
    );
}

#[test]
fn a_grunt_is_weak_evidence_and_never_divergence() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    let events = tracker.update(&sb.say(Some(A), "Hm.", 0.3));
    assert_eq!(rejection_of(&events), Some(RejectReason::WeakEvidence));
    // Crucially it must not count toward confidence decay either: an act full of
    // grunts is not an act in which tracking has been lost.
    for _ in 0..30 {
        tracker.update(&sb.say(Some(A), "Hm.", 0.3));
    }
    assert_eq!(
        tracker.confidence(),
        Confidence::Scene,
        "grunts are not divergence"
    );
}

#[test]
fn a_one_word_line_confirms_the_next_line_but_cannot_relocate_the_show() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    tracker.update(&sb.say(Some(B), "Je sais, mais je suis venu quand même.", 2.5));
    tracker.update(&sb.say(Some(A), "Alors pars avant qu'il ne revienne.", 2.5));
    assert_eq!(tracker.position(), 2);

    // Line 3 is "Oui." — one token, and the same word appears again at line 7.
    let events = tracker.update(&sb.say(Some(B), "Oui.", 0.5));
    let (idx, conf, _) = position_of(&events).expect("the next line should be confirmable");
    assert_eq!(idx, 3, "the near one, never the far duplicate");
    assert_eq!(conf, Confidence::Line, "one word is not a word-level match");
}

#[test]
fn a_distant_match_makes_the_tracker_uncertain_even_before_it_moves() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    assert_eq!(tracker.confidence(), Confidence::Word);

    // Something convincing was heard eight lines away. We do not follow it on one
    // sighting — but continuing to assert the old position confidently is how a
    // confident-wrong event is manufactured.
    let events =
        tracker.update(&sb.say(Some(A), "Tu n'as jamais rien compris à cette maison.", 3.0));
    assert_eq!(rejection_of(&events), Some(RejectReason::JumpPending));
    assert_eq!(tracker.position(), 0, "position holds");
    assert!(
        tracker.confidence() <= Confidence::Block,
        "but confidence must not"
    );
}

#[test]
fn the_same_segments_always_produce_the_same_events() {
    let script = toy_script();
    let p = prepared(&script);
    let segments: Vec<TranscriptSegment> = {
        let mut sb = SegBuilder::new();
        let mut v = Vec::new();
        for (i, line) in script.lines.iter().enumerate() {
            v.push(sb.say(Some(&line.character), &line.text, 2.5));
            if i == 4 {
                v.push(sb.say(
                    Some(B),
                    "et là on improvise complètement pendant un moment",
                    5.0,
                ));
            }
        }
        v
    };

    let run = || {
        let mut tracker = Tracker::new(&p, TrackerConfig::default());
        segments
            .iter()
            .flat_map(|s| tracker.update(s))
            .collect::<Vec<_>>()
    };
    let a = run();
    let b = run();
    assert_eq!(a, b, "tracker must be deterministic");
    assert!(
        a.len() > script.lines.len(),
        "sanity: events were actually produced"
    );
}

#[test]
fn a_scene_change_is_an_implicit_landmark() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
    for _ in 0..4 {
        tracker.update(&sb.say(Some(B), "personne ne sait plus quelle scène on répète", 6.0));
    }
    assert_eq!(tracker.confidence(), Confidence::Lost);

    // Line 12 opens scene 2 — a weight-3 landmark with no explicit tag.
    let events = tracker.update(&sb.say(
        Some(A),
        "La lampe s'est éteinte vers quatre heures du matin.",
        3.5,
    ));
    let (idx, _, cause) = position_of(&events).expect("scene opening should re-anchor");
    assert_eq!(idx, 12);
    assert_eq!(cause, PositionCause::Reanchor);
}

#[test]
fn one_segment_may_cover_two_of_its_speakers_lines() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.set_position(11, Confidence::Line);
    // A single VAD segment catches both of MARIE's scene-2 lines, with JEAN's
    // short line between them landing on his own channel.
    let events = tracker.update(&sb.say(
        Some(A),
        "La lampe s'est éteinte vers quatre heures du matin. C'est ainsi que les choses finissent toujours.",
        6.0,
    ));
    let (idx, _, _) = position_of(&events).expect("a two-line span should be matchable");
    assert_eq!(idx, 14, "position should land on the last line of the span");
}

#[test]
fn a_span_is_not_extended_over_a_line_that_was_not_heard() {
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.set_position(11, Confidence::Line);
    // Only the first of MARIE's two scene-2 lines was spoken.
    let events = tracker.update(&sb.say(
        Some(A),
        "La lampe s'est éteinte vers quatre heures du matin.",
        3.5,
    ));
    let (idx, _, _) = position_of(&events).expect("the line that was said should match");
    assert_eq!(idx, 12, "must not run ahead to line 14 on unheard material");
}

#[test]
fn stacked_turns_do_not_leave_the_tracker_confidently_wrong() {
    // From a real night. An actor with a photographic memory lost his place, went
    // upstage to read the script, and while he was gone his partner delivered all
    // four of her paragraphs; he then returned and delivered all four of his. The
    // scene was performed complete and in order per speaker, but the *interleaving*
    // was gone — B B B B A A A A where the script says A B A B A B A B.
    //
    // This is the PRD's "inverted lines" compound failure, at scale: skip tolerance
    // carries the position forward through B's block, and forward-only then refuses
    // to go back for A's. What must not happen is the tracker asserting a confident
    // position through several minutes of dialogue it has entirely misplaced.
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), &script.lines[0].text, 2.5));
    tracker.update(&sb.say(Some(B), &script.lines[1].text, 2.5));

    // B's block first: lines 5, 7, 9 — hers out of the coming exchange.
    let mut worst_confident_error = 0usize;
    for i in [5usize, 9, 11] {
        let events = tracker.update(&sb.say(Some(B), &script.lines[i].text, 2.5));
        for e in &events {
            if let TrackerEvent::Position {
                line_index,
                confidence,
                ..
            } = e
            {
                if *confidence >= Confidence::Line {
                    worst_confident_error = worst_confident_error.max(line_index.abs_diff(i));
                }
            }
        }
    }
    // Then his: lines 4, 6, 8 — all of them now behind the position.
    for i in [4usize, 6, 8] {
        let events = tracker.update(&sb.say(Some(A), &script.lines[i].text, 2.5));
        for e in &events {
            if let TrackerEvent::Position {
                line_index,
                confidence,
                ..
            } = e
            {
                if *confidence >= Confidence::Line {
                    worst_confident_error = worst_confident_error.max(line_index.abs_diff(i));
                }
            }
        }
    }
    assert!(
        worst_confident_error <= 5,
        "claimed a confident position {worst_confident_error} lines from the speaker"
    );

    // What it actually does, and it is the right thing: it holds its last known
    // position at `Block` confidence throughout the disruption rather than chasing
    // the stacked delivery, then re-acquires at the landmark. The operator's page
    // goes stale for the duration — but it is *labelled* stale, which is the whole
    // distinction the confidence levels exist to draw, and in a live run this is
    // what raises the divergence warning and, if it persists, the help request.

    // And the scene resumes normally. Whatever happened during the stack, the
    // tracker has to come back — this is the part an operator would actually notice.
    for i in 12..16 {
        tracker.update(&sb.say(Some(&script.lines[i].character), &script.lines[i].text, 2.5));
    }
    assert!(
        tracker.position() >= 14,
        "never recovered after the stack: left at line {}",
        tracker.position()
    );
}

#[test]
fn one_wrong_word_in_a_proper_noun_is_absorbed() {
    // Also from a real night: "Polymédor" for "Polymestor", and several minutes of
    // a company trying not to laugh. The slip itself must cost nothing — a single
    // mangled proper noun is exactly what fuzzy matching is for — and the silence
    // that follows must not decay confidence, since decay counts speech, not time.
    let script = toy_script();
    let p = prepared(&script);
    let mut tracker = Tracker::new(&p, TrackerConfig::default());
    let mut sb = SegBuilder::new();

    tracker.update(&sb.say(Some(A), &script.lines[0].text, 2.5));
    let events = tracker.update(&sb.say(
        Some(B),
        "Je sais, mais je suis venu quand mème.", // one word mangled
        2.5,
    ));
    let (idx, conf, _) = position_of(&events).expect("a single wrong word must not lose the line");
    assert_eq!(idx, 1);
    assert!(conf >= Confidence::Line);

    // Four minutes of the company recovering: no speech, so no decay.
    sb.t += 240.0;
    assert_eq!(
        tracker.confidence(),
        conf,
        "silence must not decay confidence"
    );
    let events = tracker.update(&sb.say(Some(A), &script.lines[2].text, 2.5));
    assert_eq!(
        position_of(&events).map(|(i, ..)| i),
        Some(2),
        "should resume normally"
    );
}

/// A long script of unmistakable lines, for the questions that only appear at
/// distance. The toy script above is sixteen lines — every relocation in it is a
/// short one, so it cannot say anything about what a *long* one should cost.
fn long_script(n: usize) -> Script {
    // Every word of every line is invented and used exactly once in the whole script,
    // so two lines share nothing at all. Anything less — even a shared connective
    // skeleton like "traverse la place et rejoint" — scores every line against every
    // other at nearly 1.0, and the test ends up measuring the ambiguity margin
    // instead of the distance rule. (Observed: the first attempt matched all 320
    // lines to line 0 at score 1.09.)
    const CONS: &[&str] = &["b", "d", "f", "g", "k", "l", "m", "n", "p", "r", "s", "t", "v", "z"];
    const VOWS: &[&str] = &["a", "e", "i", "o", "ou", "u"];
    const PER_LINE: usize = 6;
    let word = |k: usize| {
        format!(
            "{}{}{}{}n",
            CONS[k % 14],
            VOWS[(k / 14) % 6],
            CONS[(k / 84) % 14],
            VOWS[(k / 1176) % 6]
        )
    };
    assert!(n * PER_LINE <= 14 * 6 * 14 * 6, "fixture would repeat a word");
    let lines: Vec<ScriptLine> = (0..n)
        .map(|i| ScriptLine {
            flag: false,
            spoken: None,
            cut: false,
            kind: LineKind::Dialogue,
            hold: None,
            hold_seconds: None,
            id: format!("L-{:04}", i + 1),
            act: "act-1".into(),
            scene: "sc-1".into(),
            character: A.into(),
            text: (0..PER_LINE)
                .map(|j| word(i * PER_LINE + j))
                .collect::<Vec<_>>()
                .join(" "),
            lang: None,
            landmark: 0,
            alternates: Vec::new(),
        })
        .collect();
    Script {
        format: "choufleur-script".into(),
        format_version: "0.1".into(),
        title: Some("Long".into()),
        default_lang: vec![LangCode::new("fr")],
        acts: vec![],
        scenes: vec![],
        characters: vec![Character {
            id: A.into(),
            name: "MARIE".into(),
            lang: None,
            channels: vec![1],
        }],
        lines,
    }
}

/// Confirming segments the challenger needs before it may move the show to `target`.
///
/// The tracker is settled on the opening lines first, deliberately: from a standing
/// start it sits at `Scene` confidence and searches the whole script on the *ordinary*
/// path, which reaches any line in two sightings and never consults the challenger at
/// all. Only once a position is held does a distant claim have to go through the
/// challenger — which is exactly the situation this rule governs.
fn segments_to_relocate(script: &Script, cfg: TrackerConfig, target: usize) -> Option<usize> {
    let prepared = prepared(script);
    let mut tracker = Tracker::new(&prepared, cfg);
    let mut segs = SegBuilder::new();
    for i in 0..3 {
        tracker.update(&segs.say(Some(A), &script.lines[i].text, 3.0));
    }
    assert_eq!(tracker.position(), 2, "settled on the opening");
    assert!(tracker.confidence() >= Confidence::Line, "and sure of it");

    for k in 0..16 {
        let text = script.lines[target + k].text.clone();
        let events = tracker.update(&segs.say(Some(A), &text, 3.0));
        if tracker.position() >= target {
            assert_eq!(
                position_of(&events).map(|(.., c)| c),
                Some(PositionCause::Reanchor),
                "the challenger is what adopted it"
            );
            return Some(k + 1);
        }
    }
    None
}

#[test]
fn a_longer_relocation_needs_more_confirming_segments() {
    let script = long_script(320);
    let cfg = TrackerConfig::default();
    assert_eq!(cfg.challenger_extra_hit_lines, 20, "the rule is switched on");

    let near = segments_to_relocate(&script, cfg.clone(), 40).expect("near relocation happens");
    let far = segments_to_relocate(&script, cfg.clone(), 300).expect("far relocation happens");

    // Both must still be reachable — this charges evidence, it does not wall off the
    // far half of the script.
    assert!(
        far > near,
        "300 lines away should cost more than 40: near {near}, far {far}"
    );
    // Segments consumed, not sightings collected — a few early ones are spent
    // decaying the incumbent before any rival can out-argue it — so this only checks
    // that the ceiling keeps a distant relocation to a bounded wait rather than
    // walling it off.
    assert!(
        far <= cfg.challenger_max_hits + 3,
        "a far relocation must stay reachable: {far} segments"
    );
}

#[test]
fn switching_the_distance_scaling_off_charges_every_relocation_the_same() {
    let script = long_script(320);
    let cfg = TrackerConfig {
        challenger_extra_hit_lines: 0,
        ..TrackerConfig::default()
    };
    let near = segments_to_relocate(&script, cfg.clone(), 40).expect("near relocation happens");
    let far = segments_to_relocate(&script, cfg, 300).expect("far relocation happens");
    assert_eq!(
        near, far,
        "with the rule off, distance is free — which is what let a coincidence \
         three hundred lines away move the show on the same evidence as its neighbour"
    );
}

#[test]
fn audio_nothing_in_the_script_explains_can_be_made_to_cost_the_position_nothing() {
    // `noise_floor` is off by default (it was measured and did not pay), so this
    // pins the mechanism rather than the default: with it raised, a burst of
    // gibberish leaves confidence exactly where it was.
    let script = toy_script();
    let prepared = prepared(&script);
    let quiet = TrackerConfig {
        noise_floor: 0.5,
        ..TrackerConfig::default()
    };
    for (cfg, expect_decay) in [(TrackerConfig::default(), true), (quiet, false)] {
        let mut tracker = Tracker::new(&prepared, cfg);
        let mut segs = SegBuilder::new();
        tracker.update(&segs.say(Some(A), "Tu ne devrais pas être ici.", 2.0));
        let before = tracker.confidence();
        assert!(before >= Confidence::Line, "placed to begin with");
        for _ in 0..12 {
            tracker.update(&segs.say(
                Some(A),
                "brrrraaah wooooo tchak tchak nnnngh aaaaah ouille",
                3.0,
            ));
        }
        let after = tracker.confidence();
        if expect_decay {
            assert!(after < before, "today: grunting counts against the position");
        } else {
            assert_eq!(
                after, before,
                "raised floor: absence of evidence is not evidence of absence"
            );
        }
    }
}

#[test]
fn a_marked_hold_stops_the_clock_instead_of_merely_hiding_the_page() {
    // Otis Redding plays, Whisper writes it down as dialogue, and none of it fits.
    // Without the marker that counts as evidence against the position; with it, the
    // passage is simply not evidence at all.
    let mut script = toy_script();
    script.lines[3].kind = LineKind::Stage;
    script.lines[3].text = "Chorégraphie sur la musique d'Otis Redding.".into();
    script.lines[3].hold = Some(Hold::Music);
    let prepared = prepared(&script);

    let mut tracker = Tracker::new(&prepared, TrackerConfig::default());
    let mut segs = SegBuilder::new();
    for i in 0..4 {
        tracker.update(&segs.say(Some(if i % 2 == 0 { A } else { B }), &script.lines[i].text, 3.0));
    }
    assert_eq!(tracker.position(), 3, "reached the music");
    assert_eq!(tracker.hold(), Some(Hold::Music), "and knows why it is waiting");
    let held = tracker.confidence();

    // Two minutes of a recogniser's opinion of a soul record.
    for _ in 0..30 {
        tracker.update(&segs.say(Some(A), "Ouh, loving you, oh yeah, sittin' on the dock", 3.5));
    }
    assert_eq!(tracker.position(), 3, "the music never moved the show");
    assert_eq!(
        tracker.confidence(),
        held,
        "and never counted against it: a hold is absence of evidence, not evidence"
    );

    // The company comes back in, and the page comes back with it.
    let events = tracker.update(&segs.say(Some(A), &script.lines[4].text, 3.0));
    assert_eq!(position_of(&events).map(|(i, ..)| i), Some(4), "released by being heard");
    assert_eq!(tracker.hold(), None);
}

#[test]
fn without_the_marker_the_same_music_loses_the_show() {
    // The control. Same audio, same script, marker removed — this is what the operator
    // was watching when they said silence and music "expand the detection".
    let mut script = toy_script();
    script.lines[3].text = "Chorégraphie sur la musique d'Otis Redding.".into();
    let prepared = prepared(&script);
    let mut tracker = Tracker::new(&prepared, TrackerConfig::default());
    let mut segs = SegBuilder::new();
    for i in 0..4 {
        tracker.update(&segs.say(Some(if i % 2 == 0 { A } else { B }), &script.lines[i].text, 3.0));
    }
    let before = tracker.confidence();
    for _ in 0..30 {
        tracker.update(&segs.say(Some(A), "Ouh, loving you, oh yeah, sittin' on the dock", 3.5));
    }
    assert!(
        tracker.confidence() < before,
        "unmarked, the music is read as the show having left us behind"
    );
}

#[test]
fn a_stage_direction_nobody_says_is_never_offered_as_a_match() {
    // *Hécube, pas Hécube* stages a play within a play, so its didascalies come from
    // two works and behave oppositely: Rodrigues' own are never voiced, while
    // Euripides' — quoted in guillemets — are read aloud by Séphora as part of the
    // performance. One flag, both directions, stated per line.
    let mut script = toy_script();
    script.lines[2].kind = LineKind::Stage;
    script.lines[2].text = "Nadia enfile son manteau, aidée par d'autres comédiens.".into();
    script.lines[2].spoken = Some(false);
    script.lines[6].kind = LineKind::Stage;
    script.lines[6].spoken = Some(true); // read aloud, so still matchable
    // Line 3 is "Oui." in the toy script, and a one-word segment is weak evidence that
    // may only confirm the very next line — which would make this test measure that
    // rule instead of this one.
    script.lines[3].text = "Oui, je comprends très bien ce que tu veux dire.".into();
    let prepared = prepared(&script);

    assert!(!prepared.lines[2].matchable, "unvoiced direction is invisible to the matcher");
    assert!(prepared.lines[6].matchable, "a direction that is read aloud is not");

    let mut tracker = Tracker::new(&prepared, TrackerConfig::default());
    let mut segs = SegBuilder::new();
    tracker.update(&segs.say(Some(A), &script.lines[0].text, 3.0));
    tracker.update(&segs.say(Some(B), &script.lines[1].text, 3.0));
    // The company goes straight from line 1 to line 3 — nobody reads the direction out.
    let events = tracker.update(&segs.say(Some(B), &script.lines[3].text, 2.0));
    assert_eq!(
        position_of(&events).map(|(i, ..)| i),
        Some(3),
        "the span steps over the direction rather than stalling on it"
    );

    // And it can never be *reached*, however much the text happens to resemble it.
    let events = tracker.update(&segs.say(Some(A), &script.lines[2].text, 3.0));
    assert_ne!(position_of(&events).map(|(i, ..)| i), Some(2));
}

#[test]
fn a_cut_line_no_longer_competes_for_the_match() {
    // Marked, still shown to the operator, and struck from the production. Until now
    // it was also still a candidate — text the audience will never hear, offered to
    // the matcher for the whole run.
    let mut script = toy_script();
    script.lines[2].cut = true;
    let prepared = prepared(&script);
    assert!(!prepared.lines[2].matchable);

    let mut tracker = Tracker::new(&prepared, TrackerConfig::default());
    let mut segs = SegBuilder::new();
    tracker.update(&segs.say(Some(A), &script.lines[0].text, 3.0));
    let events = tracker.update(&segs.say(Some(A), &script.lines[2].text, 3.0));
    assert_ne!(
        position_of(&events).map(|(i, ..)| i),
        Some(2),
        "a cut line cannot be matched even when its exact text is heard"
    );
}
