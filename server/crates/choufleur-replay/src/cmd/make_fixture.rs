//! `make-fixture` — a synthetic corpus from macOS speech synthesis.
//!
//! The real corpus is a rehearsal recording that has to be exported, aligned and
//! hand-corrected. Until that exists, the pipeline still needs something to run
//! against end to end, with ground truth that is *exact* rather than approximate:
//! here every line's onset is known to the sample, because this code placed it.
//!
//! What this proves: that WAV reading, VAD segmentation, decoding, matching,
//! tracking and scoring fit together and stay deterministic. What it emphatically
//! does **not** prove: anything about the go/no-go gate. Synthetic speech has
//! perfect diction, no reverb, no bleed, no overlap and no audience — it is the
//! easiest audio that will ever reach this system. Thresholds tuned against it
//! would be meaningless.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use choufleur_core::lang::LangCode;
use choufleur_core::script::{Character, Script, ScriptLine};

use crate::formats::{write_json, write_jsonl, GroundTruthLine};
use crate::manifest::{sha256_file, AudioFile, ChannelSpec, Manifest};
use crate::wav_stream::{MonoWavWriter, WavBlockReader};

const SAMPLE_RATE: u32 = 48_000;
const LEAD_IN_S: f64 = 1.0;
const TAIL_S: f64 = 1.0;
const GAP_MIN_MS: u64 = 300;
const GAP_MAX_MS: u64 = 1200;

// ---------------------------------------------------------------------------
// Synthesis
// ---------------------------------------------------------------------------

/// Anything that can turn text into 48 kHz mono audio. Abstracted so the timeline
/// arithmetic is testable without spawning `say` — the tests must not depend on
/// which voices happen to be installed.
pub trait Synth {
    fn speak(&self, text: &str, voice: &str, wpm: u32) -> Result<Vec<f32>>;
}

/// macOS `say`, via a temporary file.
pub struct SaySynth {
    tmp: PathBuf,
    /// Whether `say` can write a 48 kHz WAV directly, probed once at construction.
    /// Older releases only reliably emit AIFF, so the fallback goes via `afconvert`.
    direct_wav: bool,
}

impl SaySynth {
    pub fn new(tmp: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&tmp)?;
        let mut synth = SaySynth {
            tmp,
            direct_wav: false,
        };
        synth.direct_wav = synth.probe_direct_wav();
        Ok(synth)
    }

    fn probe_direct_wav(&self) -> bool {
        let probe = self.tmp.join("probe.wav");
        let _ = std::fs::remove_file(&probe);
        let ok = Command::new("say")
            .args(["--data-format=LEI16@48000", "-o"])
            .arg(&probe)
            .arg("test")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let usable = ok && WavBlockReader::open(&probe).is_ok();
        let _ = std::fs::remove_file(&probe);
        usable
    }

    /// Voice names `say` knows about on this machine.
    pub fn available_voices() -> Result<Vec<String>> {
        let out = Command::new("say")
            .args(["-v", "?"])
            .output()
            .context("running `say -v '?'` — is this macOS?")?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect())
    }
}

impl Synth for SaySynth {
    fn speak(&self, text: &str, voice: &str, wpm: u32) -> Result<Vec<f32>> {
        let wav = self.tmp.join("line.wav");
        let _ = std::fs::remove_file(&wav);

        if self.direct_wav {
            let status = Command::new("say")
                .args([
                    "-v",
                    voice,
                    "-r",
                    &wpm.to_string(),
                    "--data-format=LEI16@48000",
                    "-o",
                ])
                .arg(&wav)
                .arg(text)
                .status()
                .context("running `say`")?;
            if !status.success() {
                bail!("`say -v {voice}` failed on {text:?}");
            }
        } else {
            let aiff = self.tmp.join("line.aiff");
            let _ = std::fs::remove_file(&aiff);
            let status = Command::new("say")
                .args(["-v", voice, "-r", &wpm.to_string(), "-o"])
                .arg(&aiff)
                .arg(text)
                .status()
                .context("running `say`")?;
            if !status.success() {
                bail!("`say -v {voice}` failed on {text:?}");
            }
            let status = Command::new("afconvert")
                .args(["-f", "WAVE", "-d", "LEI16@48000", "-c", "1"])
                .arg(&aiff)
                .arg(&wav)
                .status()
                .context("running `afconvert`")?;
            if !status.success() {
                bail!("afconvert failed converting {}", aiff.display());
            }
        }

        let mut reader = WavBlockReader::open(&wav)?;
        if reader.sample_rate != SAMPLE_RATE {
            bail!(
                "say produced {} Hz, expected {SAMPLE_RATE}",
                reader.sample_rate
            );
        }
        let mut all = Vec::new();
        let mut buf = Vec::new();
        while reader.read_block(&mut buf, 48_000)? > 0 {
            all.extend_from_slice(&buf);
        }
        Ok(all)
    }
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// xorshift64*, seeded explicitly.
///
/// Deliberately hand-rolled rather than pulled from `rand`: the fixture's gaps must
/// be identical across machines and across dependency upgrades, and `rand`'s
/// generators make no such promise between versions.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `[lo, hi]`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }
    /// Uniform in `[-1, 1)`.
    pub fn noise(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / 8_388_608.0 - 1.0
    }
}

/// Where each line sits on the timeline, in frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub onset: u64,
    pub frames: u64,
}

/// Lay lines end to end with randomized gaps. Pure, so the arithmetic can be
/// tested without synthesizing anything.
pub fn plan_timeline(durations: &[u64], rng: &mut Rng, sample_rate: u32) -> Vec<Placement> {
    let ms = |m: u64| m * sample_rate as u64 / 1000;
    let mut cursor = (LEAD_IN_S * sample_rate as f64) as u64;
    let mut out = Vec::with_capacity(durations.len());
    for &frames in durations {
        out.push(Placement {
            onset: cursor,
            frames,
        });
        cursor += frames + ms(rng.range(GAP_MIN_MS, GAP_MAX_MS));
    }
    out
}

/// How lines are laid out in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// One speaker at a time, gaps between lines. What a scene sounds like, and
    /// what the accuracy fixtures use.
    Sequential,
    /// Every channel talking at once, continuously. Nothing like a play — this is
    /// a **load test**, built to answer the devplan's compute criterion, which is
    /// stated in *concurrent active channels* and therefore cannot be measured on
    /// dialogue that politely takes turns.
    Concurrent,
}

/// Lay each character out on their own timeline, so every channel is busy at once.
///
/// Gaps are short and the channels independent, so with N characters this keeps
/// roughly N channels in speech simultaneously for the whole recording — the worst
/// case the ASR stage will ever see, and one a real performance never produces.
/// The resulting ground truth overlaps heavily, which is deliberate: it also
/// exercises the eval's latest-onset-wins rule.
pub fn plan_timeline_concurrent(
    durations: &[u64],
    channel_of: &[usize],
    n_channels: usize,
    rng: &mut Rng,
    sample_rate: u32,
) -> Vec<Placement> {
    let ms = |m: u64| m * sample_rate as u64 / 1000;
    let lead = (LEAD_IN_S * sample_rate as f64) as u64;
    let mut cursors = vec![lead; n_channels.max(1)];
    let mut out = Vec::with_capacity(durations.len());
    for (&frames, &ch) in durations.iter().zip(channel_of) {
        let last = cursors.len() - 1;
        let cursor = &mut cursors[ch.min(last)];
        out.push(Placement {
            onset: *cursor,
            frames,
        });
        // Short breaths only: the point is to keep every channel speaking.
        *cursor += frames + ms(rng.range(100, 350));
    }
    out
}

// ---------------------------------------------------------------------------
// The built-in script
// ---------------------------------------------------------------------------

/// One row of the built-in fixture script: scene, character, text, explicit
/// language tags (`None` inherits per notation §8.2), landmark weight.
type FixtureRow = (
    &'static str,
    &'static str,
    &'static str,
    Option<&'static [&'static str]>,
    u8,
);

/// A synthetic script with `n` characters, for the concurrent load test.
///
/// The content is irrelevant to what this measures — what matters is that the
/// lines are of realistic length, so each decode costs what a real one costs, and
/// that the characters alternate languages, so the load includes the per-segment
/// language switching a bilingual production imposes.
pub fn load_test_script(n_characters: usize) -> Script {
    const LINES_FR: &[&str] = &[
        "Tu ne devrais pas être ici, pas ce soir, pas après tout cela.",
        "Alors pars avant qu'il ne rentre du théâtre et qu'il te trouve.",
        "Les cerisiers ont brûlé pendant la nuit entière sans que personne bouge.",
        "Personne ne viendra les rallumer maintenant, tu le sais très bien.",
        "Va-t'en. Je ne veux plus jamais entendre ta voix dans cette maison.",
        "C'est ainsi que les choses finissent toujours, sans un mot de plus.",
        "Je ne t'accompagnerai pas jusqu'à la gare demain matin, c'est décidé.",
        "La lampe s'est éteinte vers quatre heures du matin et rien n'a changé.",
    ];
    const LINES_EN: &[&str] = &[
        "I came as soon as I heard about the fire at the far end of the orchard.",
        "I saw the smoke from the station platform before the train had even stopped.",
        "Nobody has slept in this house since Tuesday and nobody intends to tonight.",
        "The lamps were still burning at four in the morning when I walked past.",
        "The first train leaves a little after six o'clock, if you mean to catch it.",
        "She never says what she actually means, and you have never once noticed.",
        "I stopped expecting her to explain herself a very long time ago indeed.",
        "Then let them finish quietly, for once, without another word from anyone.",
    ];

    let n = n_characters.max(1);
    let characters: Vec<Character> = (0..n)
        .map(|i| Character {
            id: format!("char-{i}"),
            name: format!("SPEAKER{i}"),
            lang: Some(vec![LangCode::new(if i % 2 == 0 { "fr" } else { "en" })]),
            channels: vec![(i + 1) as u16],
        })
        .collect();

    let mut lines = Vec::new();
    let mut seq = 1usize;
    for (i, c) in characters.iter().enumerate() {
        let pool = if i % 2 == 0 { LINES_FR } else { LINES_EN };
        for text in pool {
            lines.push(ScriptLine {
                id: format!("L-{seq:04}"),
                act: "act-1".into(),
                scene: "sc-1".into(),
                character: c.id.clone(),
                text: (*text).into(),
                lang: None,
                landmark: 0,
                alternates: Vec::new(),
            });
            seq += 1;
        }
    }

    Script {
        format: "choufleur-script".into(),
        format_version: "0.1".into(),
        title: Some(format!("Load test — {n} concurrent channels")),
        default_lang: vec![LangCode::new("fr")],
        acts: vec![],
        scenes: vec![],
        characters,
        lines,
    }
}

/// A bilingual two-scene fixture, built to exercise the awkward cases rather than
/// to read well: a weight-3 landmark, two consecutive lines by the same speaker
/// (so one VAD segment can cover both), a repeated one-word line, and a bilingual
/// line tagged with two languages.
pub fn default_script() -> Script {
    let rows: &[FixtureRow] = &[
        (
            "sc-1",
            "char-marie",
            "Tu ne devrais pas être ici, pas ce soir.",
            None,
            0,
        ),
        (
            "sc-1",
            "char-john",
            "I came as soon as I heard about the fire.",
            None,
            0,
        ),
        (
            "sc-1",
            "char-marie",
            "Alors pars avant qu'il ne rentre du théâtre.",
            None,
            0,
        ),
        ("sc-1", "char-john", "No.", None, 0),
        (
            "sc-1",
            "char-marie",
            "Les cerisiers ont brûlé pendant la nuit entière.",
            None,
            3,
        ),
        (
            "sc-1",
            "char-john",
            "I saw the smoke from the station platform.",
            None,
            0,
        ),
        (
            "sc-1",
            "char-sarah",
            "Nobody has slept in this house since Tuesday.",
            None,
            0,
        ),
        (
            "sc-1",
            "char-sarah",
            "The lamps were still burning at four in the morning.",
            None,
            0,
        ),
        (
            "sc-1",
            "char-marie",
            "Personne ne viendra les rallumer maintenant.",
            None,
            0,
        ),
        ("sc-1", "char-john", "No.", None, 0),
        (
            "sc-1",
            "char-marie",
            "Va-t'en. Je ne veux plus entendre ta voix ici.",
            None,
            0,
        ),
        (
            "sc-1",
            "char-sarah",
            "Elle a raison. You should go now, before dawn.",
            Some(&["fr", "en"]),
            0,
        ),
        (
            "sc-2",
            "char-john",
            "The first train leaves a little after six o'clock.",
            None,
            0,
        ),
        (
            "sc-2",
            "char-marie",
            "Je ne t'accompagnerai pas jusqu'à la gare.",
            None,
            0,
        ),
        (
            "sc-2",
            "char-sarah",
            "She never says what she actually means, you know.",
            None,
            0,
        ),
        (
            "sc-2",
            "char-john",
            "I stopped expecting her to years ago.",
            None,
            0,
        ),
        (
            "sc-2",
            "char-marie",
            "C'est ainsi que les choses finissent toujours.",
            None,
            0,
        ),
        (
            "sc-2",
            "char-sarah",
            "Then let them finish quietly, for once.",
            None,
            0,
        ),
    ];

    Script {
        format: "choufleur-script".into(),
        format_version: "0.1".into(),
        title: Some("Fixture — Les cerisiers".into()),
        default_lang: vec![LangCode::new("fr")],
        acts: vec![],
        scenes: vec![],
        characters: vec![
            Character {
                id: "char-marie".into(),
                name: "MARIE".into(),
                lang: Some(vec![LangCode::new("fr")]),
                channels: vec![1],
            },
            Character {
                id: "char-john".into(),
                name: "JOHN".into(),
                lang: Some(vec![LangCode::new("en")]),
                channels: vec![2],
            },
            Character {
                id: "char-sarah".into(),
                name: "SARAH".into(),
                lang: Some(vec![LangCode::new("en")]),
                channels: vec![3],
            },
        ],
        lines: rows
            .iter()
            .enumerate()
            .map(|(i, (scene, ch, text, lang, lm))| ScriptLine {
                id: format!("L-{:04}", i + 1),
                act: "act-1".into(),
                scene: (*scene).into(),
                character: (*ch).into(),
                text: (*text).into(),
                lang: lang.map(|ls| ls.iter().map(|l| LangCode::new(l)).collect()),
                landmark: *lm,
                alternates: Vec::new(),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Voices
// ---------------------------------------------------------------------------

fn default_voices() -> BTreeMap<String, Vec<&'static str>> {
    BTreeMap::from([
        (
            "fr".to_string(),
            vec!["Thomas", "Jacques", "Amélie", "Aurelie"],
        ),
        (
            "en".to_string(),
            vec!["Samantha", "Daniel", "Karen", "Alex"],
        ),
    ])
}

/// Assign one distinct voice per character, from the pool for its language.
fn assign_voices(
    script: &Script,
    overrides: Option<&str>,
    available: &[String],
) -> Result<BTreeMap<String, String>> {
    let mut pools = default_voices();
    if let Some(spec) = overrides {
        for pair in spec.split(',').filter(|p| !p.trim().is_empty()) {
            let (lang, voice) = pair
                .split_once('=')
                .with_context(|| format!("--voices expects lang=voice pairs, got {pair:?}"))?;
            pools.insert(
                lang.trim().to_lowercase(),
                vec![Box::leak(voice.trim().to_string().into_boxed_str())],
            );
        }
    }

    let mut used: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for c in &script.characters {
        let lang = c
            .lang
            .as_ref()
            .and_then(|l| l.first())
            .cloned()
            .unwrap_or_else(|| script.default_lang[0].clone());
        let key = lang.primary().to_string();
        let pool = pools.get(&key).cloned().unwrap_or_default();
        if pool.is_empty() {
            bail!("no voice configured for language {key:?}; pass --voices {key}=<VoiceName>");
        }
        // Prefer a voice actually installed, and a different one per character so
        // channels are distinguishable by ear when debugging.
        let n = used.entry(key.clone()).or_insert(0);
        let installed: Vec<&&str> = pool
            .iter()
            .filter(|v| available.iter().any(|a| a == **v))
            .collect();
        let chosen = if installed.is_empty() {
            bail!(
                "none of the {key} voices {pool:?} are installed.\n\
                 Installed voices: {}\n\
                 Add one in System Settings › Accessibility › Spoken Content › System Voice › Manage Voices,\n\
                 or pass --voices {key}=<VoiceName>.",
                available.join(", ")
            )
        } else {
            installed[*n % installed.len()].to_string()
        };
        *n += 1;
        out.insert(c.id.clone(), chosen);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    out_dir: &Path,
    script_path: Option<&Path>,
    seed: u64,
    voices: Option<&str>,
    rate_wpm: u32,
    noise_db: f32,
    load_test: Option<usize>,
) -> Result<()> {
    let script = match (load_test, script_path) {
        (Some(n), _) => load_test_script(n),
        (None, Some(p)) => {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))?
        }
        (None, None) => default_script(),
    };
    let layout = if load_test.is_some() {
        println!("layout:  concurrent — every channel speaking at once (load test)");
        Layout::Concurrent
    } else {
        Layout::Sequential
    };

    let available = SaySynth::available_voices()?;
    let voice_of = assign_voices(&script, voices, &available)?;
    for (c, v) in &voice_of {
        println!("  {c} → {v}");
    }
    let synth = SaySynth::new(out_dir.join(".tmp"))?;
    generate(
        out_dir, &script, &synth, &voice_of, seed, rate_wpm, noise_db, layout,
    )?;
    let _ = std::fs::remove_dir_all(out_dir.join(".tmp"));
    println!("\nfixture written to {}", out_dir.display());
    println!("next: choufleur-replay verify {}", out_dir.display());
    Ok(())
}

/// The generation core, with synthesis injected.
#[allow(clippy::too_many_arguments)]
pub fn generate(
    out_dir: &Path,
    script: &Script,
    synth: &dyn Synth,
    voice_of: &BTreeMap<String, String>,
    seed: u64,
    rate_wpm: u32,
    noise_db: f32,
    layout: Layout,
) -> Result<Manifest> {
    std::fs::create_dir_all(out_dir)?;

    // 1. Synthesize every line first: the timeline needs all durations before any
    //    audio can be placed.
    let mut audio: Vec<Vec<f32>> = Vec::with_capacity(script.lines.len());
    for (i, line) in script.lines.iter().enumerate() {
        let voice = voice_of
            .get(&line.character)
            .with_context(|| format!("no voice assigned for {}", line.character))?;
        let samples = synth.speak(&line.text, voice, rate_wpm)?;
        if samples.is_empty() {
            bail!("line {} ({}) synthesized to silence", i + 1, line.id);
        }
        audio.push(samples);
    }

    // 2. Place them.
    let mut rng = Rng::new(seed);
    let durations: Vec<u64> = audio.iter().map(|a| a.len() as u64).collect();
    let plan = match layout {
        Layout::Sequential => plan_timeline(&durations, &mut rng, SAMPLE_RATE),
        Layout::Concurrent => {
            let channel_of: Vec<usize> = script
                .lines
                .iter()
                .map(|l| {
                    script
                        .characters
                        .iter()
                        .position(|c| c.id == l.character)
                        .unwrap_or(0)
                })
                .collect();
            plan_timeline_concurrent(
                &durations,
                &channel_of,
                script.characters.len(),
                &mut rng,
                SAMPLE_RATE,
            )
        }
    };
    // With a concurrent layout the last line in script order is not the last in
    // time, so the recording's length is the furthest any channel reaches.
    let total_frames = plan
        .iter()
        .map(|p| p.onset + p.frames + (TAIL_S * SAMPLE_RATE as f64) as u64)
        .max()
        .unwrap_or(0);

    // 3. One writer per character channel, plus the mixdown.
    let amp = if noise_db < 0.0 {
        10f32.powf(noise_db / 20.0)
    } else {
        0.0
    };
    let mut channels: Vec<ChannelSink> = Vec::new();
    for (i, c) in script.characters.iter().enumerate() {
        let index = (i + 1) as u16;
        let name = format!("ch{index:02}-{}.wav", c.name.to_lowercase());
        let path = out_dir.join(&name);
        let writer = MonoWavWriter::create(&path, SAMPLE_RATE)?;
        channels.push(ChannelSink {
            index,
            name,
            path,
            writer,
            written: 0,
        });
    }

    let mut noise_rng = Rng::new(seed ^ 0x9E37_79B9_7F4A_7C15);
    let mut ground_truth = Vec::with_capacity(script.lines.len());

    for (i, line) in script.lines.iter().enumerate() {
        let place = plan[i];
        let ci = script
            .characters
            .iter()
            .position(|c| c.id == line.character)
            .with_context(|| {
                format!(
                    "line {} names unknown character {}",
                    line.id, line.character
                )
            })?;

        let sink = &mut channels[ci];
        pad(
            &mut sink.writer,
            place.onset.saturating_sub(sink.written),
            amp,
            &mut noise_rng,
        )?;
        sink.writer.write(&audio[i])?;
        sink.written = place.onset + place.frames;
        let index = sink.index;

        ground_truth.push(GroundTruthLine {
            line_id: line.id.clone(),
            onset: place.onset as f64 / SAMPLE_RATE as f64,
            end: (place.onset + place.frames) as f64 / SAMPLE_RATE as f64,
            channel: Some(index),
            omitted: false,
        });
    }

    // 4. Pad every channel to the same length — `verify` insists they are one take.
    let mut specs = Vec::new();
    for mut sink in channels {
        pad(
            &mut sink.writer,
            total_frames.saturating_sub(sink.written),
            amp,
            &mut noise_rng,
        )?;
        sink.writer.finalize()?;
        let index = sink.index;
        let character = script
            .characters
            .iter()
            .find(|c| c.channels.contains(&index))
            .map(|c| c.id.clone())
            .or_else(|| {
                script
                    .characters
                    .get(index as usize - 1)
                    .map(|c| c.id.clone())
            });
        specs.push(ChannelSpec {
            index,
            audio: AudioFile {
                file: PathBuf::from(&sink.name),
                sha256: sha256_file(&sink.path)?,
            },
            character,
            lang: None,
            note: None,
        });
    }

    // 5. Mix down by reading the finished channels back and summing them.
    //
    // Not by appending as the lines are placed: with a concurrent layout the lines
    // are emitted in script order but their onsets are not monotonic, so a single
    // append cursor produces a file several times too long — `verify` caught
    // exactly that. Summing the finished channels is correct for any layout, needs
    // no assumption about ordering or overlap, and stays O(1) in memory: one block
    // per channel, whatever the length of the recording.
    let mix_name = "mixdown.wav".to_string();
    let mix_path = out_dir.join(&mix_name);
    mix_channels(
        &specs
            .iter()
            .map(|c| out_dir.join(&c.audio.file))
            .collect::<Vec<_>>(),
        &mix_path,
    )?;

    // 6. Write the script, the ground truth and the manifest.
    write_json(&out_dir.join("script.json"), script, true)?;
    // Ground truth is written in time order, not script order. Under a concurrent
    // layout those differ, and `verify` insists onsets ascend.
    ground_truth.sort_by(|a, b| {
        a.onset
            .partial_cmp(&b.onset)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    write_jsonl(&out_dir.join("ground-truth.jsonl"), &ground_truth)?;

    let manifest = Manifest {
        format: crate::manifest::FORMAT.into(),
        format_version: crate::manifest::FORMAT_VERSION.into(),
        show: "fixture".into(),
        act: "act-1".into(),
        note: Some(
            "Synthetic macOS `say` audio. Proves the pipeline runs end to end; \
             proves nothing about tracking on real theatre audio."
                .into(),
        ),
        sample_rate: SAMPLE_RATE,
        script: PathBuf::from("script.json"),
        ground_truth: Some(PathBuf::from("ground-truth.jsonl")),
        channels: specs,
        mixdown: Some(AudioFile {
            file: PathBuf::from(&mix_name),
            sha256: sha256_file(&mix_path)?,
        }),
        provenance: BTreeMap::from([
            ("generator".into(), "choufleur-replay make-fixture".into()),
            ("seed".into(), seed.to_string()),
            ("rateWpm".into(), rate_wpm.to_string()),
            ("noiseDb".into(), format!("{noise_db}")),
        ]),
    };
    write_json(
        &out_dir.join(crate::manifest::MANIFEST_FILE),
        &manifest,
        true,
    )?;

    println!(
        "{} line(s), {:.1} s, {} channel(s) + mixdown",
        script.lines.len(),
        total_frames as f64 / SAMPLE_RATE as f64,
        manifest.channels.len()
    );
    Ok(manifest)
}

/// One output channel being written: its manifest identity and how far along the
/// timeline it has been filled.
struct ChannelSink {
    index: u16,
    name: String,
    path: PathBuf,
    writer: MonoWavWriter,
    written: u64,
}

/// Sum finished channel files into one mono mix, block by block.
fn mix_channels(channels: &[PathBuf], out: &Path) -> Result<()> {
    let mut readers: Vec<WavBlockReader> = channels
        .iter()
        .map(|p| WavBlockReader::open(p))
        .collect::<Result<_>>()?;
    let mut writer = MonoWavWriter::create(out, SAMPLE_RATE)?;
    let mut block = Vec::new();
    let mut acc = vec![0.0f32; 12_000];
    loop {
        let mut most = 0usize;
        acc.iter_mut().for_each(|s| *s = 0.0);
        for r in &mut readers {
            let n = r.read_block(&mut block, acc.len())?;
            for (a, b) in acc.iter_mut().zip(&block[..n]) {
                *a += b;
            }
            most = most.max(n);
        }
        if most == 0 {
            break;
        }
        // A shade below unity so a single speaker matches the per-channel level.
        // Simultaneous speakers do sum, and the writer clamps — which is what a
        // real console mix does too.
        for s in acc[..most].iter_mut() {
            *s *= 0.9;
        }
        writer.write(&acc[..most])?;
    }
    writer.finalize()
}

/// Write `frames` of room tone — noise, not digital zero, so the VAD is exercised
/// against something a microphone could actually produce.
fn pad(w: &mut MonoWavWriter, frames: u64, amp: f32, rng: &mut Rng) -> Result<()> {
    if amp <= 0.0 {
        w.write_silence(frames)?;
        return Ok(());
    }
    let mut buf = Vec::with_capacity(4096);
    let mut left = frames;
    while left > 0 {
        let n = left.min(4096);
        buf.clear();
        buf.extend((0..n).map(|_| rng.noise() * amp));
        w.write(&buf)?;
        left -= n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic stand-in for `say`: duration proportional to text length.
    struct MockSynth;
    impl Synth for MockSynth {
        fn speak(&self, text: &str, _voice: &str, _wpm: u32) -> Result<Vec<f32>> {
            let frames = text.len() * 1000;
            Ok((0..frames).map(|i| (i as f32 * 0.01).sin() * 0.4).collect())
        }
    }

    #[test]
    fn the_timeline_is_gapped_and_never_overlaps() {
        let durations = vec![48_000, 24_000, 96_000];
        let mut rng = Rng::new(7);
        let plan = plan_timeline(&durations, &mut rng, SAMPLE_RATE);
        assert_eq!(plan[0].onset, 48_000, "one second of lead-in");
        for w in plan.windows(2) {
            let gap = w[1].onset - (w[0].onset + w[0].frames);
            assert!(
                (GAP_MIN_MS * 48..=GAP_MAX_MS * 48).contains(&gap),
                "gap {gap} frames outside the configured range"
            );
        }
    }

    #[test]
    fn the_concurrent_layout_actually_overlaps() {
        // The whole point of this layout: a compute measurement taken on dialogue
        // that takes turns is measuring one active channel, whatever the manifest
        // says. If this stops overlapping, the load test measures nothing.
        let durations = vec![48_000u64; 12]; // 1 s each
        let channel_of: Vec<usize> = (0..12).map(|i| i % 4).collect();
        let plan =
            plan_timeline_concurrent(&durations, &channel_of, 4, &mut Rng::new(7), SAMPLE_RATE);

        // Every channel starts at the same instant, so all four speak at once.
        let firsts: Vec<u64> = (0..4).map(|c| plan[c].onset).collect();
        assert!(
            firsts.iter().all(|&o| o == firsts[0]),
            "channels did not start together: {firsts:?}"
        );

        // A channel's own lines still never overlap each other.
        for c in 0..4 {
            let mine: Vec<&Placement> = plan
                .iter()
                .zip(&channel_of)
                .filter(|(_, &ch)| ch == c)
                .map(|(p, _)| p)
                .collect();
            for w in mine.windows(2) {
                assert!(
                    w[1].onset >= w[0].onset + w[0].frames,
                    "channel {c} overlapped itself"
                );
            }
        }

        // The contrast: the sequential layout overlaps nothing at all.
        let seq = plan_timeline(&durations, &mut Rng::new(7), SAMPLE_RATE);
        for w in seq.windows(2) {
            assert!(w[1].onset >= w[0].onset + w[0].frames);
        }
    }

    #[test]
    fn the_load_test_script_alternates_languages_across_channels() {
        let s = load_test_script(4);
        assert_eq!(s.characters.len(), 4);
        let langs: Vec<&str> = s
            .characters
            .iter()
            .map(|c| c.lang.as_ref().unwrap()[0].as_str())
            .collect();
        assert_eq!(
            langs,
            vec!["fr", "en", "fr", "en"],
            "the load must include language switching"
        );
        assert!(s.lines.len() >= 32);
        // Channel numbers must line up with the manifest the generator writes.
        assert_eq!(s.characters[0].channels, vec![1]);
        assert_eq!(s.characters[3].channels, vec![4]);
    }

    #[test]
    fn the_same_seed_produces_the_same_timeline() {
        let durations = vec![48_000; 20];
        let a = plan_timeline(&durations, &mut Rng::new(42), SAMPLE_RATE);
        let b = plan_timeline(&durations, &mut Rng::new(42), SAMPLE_RATE);
        let c = plan_timeline(&durations, &mut Rng::new(43), SAMPLE_RATE);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn the_built_in_script_exercises_the_awkward_cases() {
        let s = default_script();
        assert!(s.lines.iter().any(|l| l.landmark == 3), "needs a landmark");
        assert!(
            s.lines
                .iter()
                .any(|l| l.lang.as_ref().is_some_and(|v| v.len() == 2)),
            "needs a bilingual line"
        );
        let repeated = s.lines.iter().filter(|l| l.text == "No.").count();
        assert_eq!(
            repeated, 2,
            "needs a repeated one-word line for the ambiguity margin"
        );
        assert!(
            s.lines.windows(2).any(|w| w[0].character == w[1].character),
            "needs two consecutive lines by one speaker"
        );
        assert!(
            s.lines.iter().any(|l| l.scene == "sc-2"),
            "needs a scene change"
        );
    }

    #[test]
    fn generation_produces_a_consistent_corpus() {
        let dir = std::env::temp_dir().join("choufleur-fixture-test");
        let _ = std::fs::remove_dir_all(&dir);
        let script = default_script();
        let voices: BTreeMap<String, String> = script
            .characters
            .iter()
            .map(|c| (c.id.clone(), "Mock".to_string()))
            .collect();

        let manifest = generate(
            &dir,
            &script,
            &MockSynth,
            &voices,
            42,
            180,
            -60.0,
            Layout::Sequential,
        )
        .unwrap();
        assert_eq!(manifest.channels.len(), 3);
        assert!(manifest.channels.iter().all(|c| !c.audio.sha256.is_empty()));

        // Ground truth must line up with what was actually written.
        let gt: Vec<GroundTruthLine> =
            crate::formats::read_jsonl(&dir.join("ground-truth.jsonl")).unwrap();
        assert_eq!(gt.len(), script.lines.len());
        assert!(
            gt.windows(2).all(|w| w[0].onset < w[1].onset),
            "onsets must ascend"
        );
        assert!(gt.iter().all(|l| l.end > l.onset));

        // Every channel is the same take, and long enough to hold the last line.
        let lengths: Vec<f64> = manifest
            .channels
            .iter()
            .map(|c| {
                WavBlockReader::open(&dir.join(&c.audio.file))
                    .unwrap()
                    .duration_seconds()
            })
            .collect();
        let first = lengths[0];
        assert!(
            lengths.iter().all(|d| (d - first).abs() < 0.01),
            "{lengths:?}"
        );
        assert!(first > gt.last().unwrap().end);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
