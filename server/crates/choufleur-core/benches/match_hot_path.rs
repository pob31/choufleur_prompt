//! The match hot path: one transcript segment scored against a full candidate
//! window. This runs once per closed VAD segment on every active channel, so it
//! shares the ≤1.5 s end-to-end budget with Whisper — which will dominate it by
//! orders of magnitude. The bench exists to notice if that ever stops being true.

use choufleur_core::lang::{LangCode, NormalizerRegistry};
use choufleur_core::script::{Character, PreparedScript, Script, ScriptLine};
use choufleur_core::tracker::{Tracker, TrackerConfig};
use choufleur_core::types::{AsrQuality, TranscriptSegment};
use criterion::{criterion_group, criterion_main, Criterion};

const LINES: &[&str] = &[
    "Tu ne devrais pas être ici, pas maintenant, pas après tout ce qui s'est passé.",
    "Je sais. Mais je ne pouvais pas rester là-bas une minute de plus.",
    "Alors pars. Va-t'en avant qu'il ne revienne et qu'il te trouve ici.",
    "Ne me demande pas ça. Tu sais très bien que je ne peux pas partir comme ça.",
    "Il y a des choses qu'on ne dit pas, et celle-là en fait partie depuis longtemps.",
];

fn build_script(n: usize) -> Script {
    let lines = (0..n)
        .map(|i| ScriptLine {
            flags: Vec::new(),
            spoken: None,
            kind: Default::default(),
            hold: None,
            hold_seconds: None,
            cut: false,
            id: format!("L-{i:04}"),
            act: "act-1".into(),
            scene: format!("sc-{}", i / 40),
            character: if i % 2 == 0 {
                "char-a".into()
            } else {
                "char-b".into()
            },
            text: LINES[i % LINES.len()].to_string(),
            lang: None,
            landmark: if i % 25 == 0 { 3 } else { 0 },
            alternates: Vec::new(),
        })
        .collect();
    Script {
        format: "choufleur-script".into(),
        format_version: "0.1".into(),
        title: Some("Bench".into()),
        default_lang: vec![LangCode::new("fr")],
        acts: vec![],
        scenes: vec![],
        characters: vec![
            Character {
                id: "char-a".into(),
                name: "A".into(),
                lang: None,
                channels: vec![1],
            members: Vec::new(),
            },
            Character {
                id: "char-b".into(),
                name: "B".into(),
                lang: None,
                channels: vec![2],
            members: Vec::new(),
            },
        ],
        lines,
    }
}

fn bench(c: &mut Criterion) {
    let script = build_script(600);
    let mut reg = NormalizerRegistry::with_defaults();
    let prepared = PreparedScript::build(&script, &mut reg);

    let seg = TranscriptSegment {
        channel: 1,
        character: Some("char-a".into()),
        t_start: 10.0,
        t_end: 13.5,
        // A realistically imperfect hearing of line 2.
        text: "alors part va t'en avant qu'il revienne et qu'il te trouve".into(),
        langs: vec![LangCode::new("fr")],
        quality: AsrQuality {
            avg_logprob: -0.3,
            no_speech_prob: 0.02,
        },
        forced_split: false,
        interim: false,
    };

    c.bench_function("tracker_update_600_line_script", |b| {
        b.iter(|| {
            let mut tracker = Tracker::new(&prepared, TrackerConfig::default());
            tracker.update(std::hint::black_box(&seg))
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
