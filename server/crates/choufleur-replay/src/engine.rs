//! The streaming engine: WAVs in, transcript segments out.
//!
//! This is where the PRD's load-management policy becomes code. Channels are read
//! in lockstep, each through its own VAD front end; closed segments go into a
//! queue ordered by the moment their audio ended; and a **single** Whisper engine
//! drains that queue in order. Round-robin across active channels falls out of the
//! ordering rather than being scheduled, idle channels cost nothing because the
//! VAD never opens on them, and determinism falls out of the whole thing being one
//! thread. Parallelism lives inside Metal, where it belongs.
//!
//! Memory is flat in the length of the recording. An act at 16 channels is about
//! 11 GB as f32 and is never materialized: what is live is one block per channel,
//! each front end's filter and VAD state, and at most one open segment per channel.
//!
//! The engine knows nothing about tracking. A [`Consumer`] decides what language to
//! decode in, what to bias the decoder with, and what to do with each result —
//! which is how `transcribe` and the coupled `track --from-audio` share this code
//! without `choufleur-asr` ever learning that a tracker exists.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use choufleur_asr::agc::AgcConfig;
use choufleur_asr::channel::ChannelFrontend;
use choufleur_asr::filter::{FilterConfig, HallucinationFilter};
use choufleur_asr::segmenter::{AudioSegment, VadConfig};
use choufleur_asr::vad::SileroSession;
use choufleur_asr::whisper::{WhisperEngine, WhisperOpts};
use choufleur_core::lang::LangCode;

use crate::clock::{ClockStats, VirtualClock};
use crate::formats::SegmentRecord;
use crate::manifest::Corpus;
use crate::wav_stream::WavBlockReader;

/// How much audio is read per channel per iteration. Small enough that a segment
/// is noticed promptly, large enough that per-block overhead is irrelevant.
pub const BLOCK_SECONDS: f64 = 0.25;

/// What to force the decoder to, and what to bias it with, for one segment.
///
/// `langs` is a *candidate set*, not a single choice. Notation §8.2 requires a
/// bilingual line to be matched against both its languages, keeping the better
/// score, and real material forces the issue harder than that: an actor who opens
/// a monologue in Dutch and quotes Hamlet in English is one channel with two
/// languages, and forcing the wrong one does not merely mis-hear — Whisper
/// *translates*, returning fluent Dutch for English speech with no error anywhere.
///
/// When more than one candidate is offered the segment is decoded against each and
/// the most confident decode wins. That costs a decode per extra language, so it
/// is paid only where the script actually says two languages are possible.
pub struct DecodeHint {
    pub langs: Vec<LangCode>,
    pub prompt: Option<String>,
}

impl DecodeHint {
    pub fn one(lang: LangCode, prompt: Option<String>) -> Self {
        DecodeHint {
            langs: vec![lang],
            prompt,
        }
    }
}

/// Supplies decode hints and receives results.
///
/// Implemented by `transcribe` (collect records) and by the coupled `track` path
/// (feed the tracker, and answer the *next* hint from where the tracker now
/// believes the show to be — the feedback loop that makes tracker-fed biasing
/// possible without the ASR crate depending on the tracker).
pub trait Consumer {
    fn decode_hint(&mut self, channel: u16, character: Option<&str>) -> DecodeHint;
    fn on_segment(&mut self, record: &SegmentRecord);
}

/// Sees every block of audio as it is read, before anything is done with it.
///
/// Exists so the live spike can make the run audible: the operator judges the system
/// by whether text arrives with the voice, which requires hearing the same audio the
/// recogniser is given. Implementations may block — the monitor's backpressure is
/// what paces a live run — so this must only ever be attached to a run intended to
/// happen at real speed.
pub trait BlockSink {
    fn on_block(&mut self, channel: u16, samples: &[f32]);
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub whisper_model: PathBuf,
    pub vad_model: PathBuf,
    pub vad: VadConfig,
    /// Per-channel adaptive gain. On by default; measured off on ambient captures,
    /// where lifting the room between phrases is a way to invent speech.
    pub agc: AgcConfig,
    pub whisper: WhisperOpts,
    pub filter: FilterConfig,
    /// Pace the run against the wall clock and measure end-to-end latency.
    pub realtime: bool,
    /// Use the mixed feed instead of the per-actor channels — the PRD's degraded
    /// mode, and the comparison that says what per-channel identity is worth.
    pub mixdown: bool,
    /// Restrict to these logical channels; `None` means all of them.
    pub channels: Option<Vec<u16>>,
}

impl EngineConfig {
    pub fn new(whisper_model: PathBuf, vad_model: PathBuf) -> Self {
        EngineConfig {
            agc: AgcConfig::default(),
            whisper_model,
            vad_model,
            vad: VadConfig::default(),
            whisper: WhisperOpts::default(),
            filter: FilterConfig::default(),
            realtime: false,
            mixdown: false,
            channels: None,
        }
    }
}

/// One channel being read: its audio, its identity, and its front end.
/// Where a channel's audio comes from.
///
/// A file and a microphone differ in three ways the loop has to know about, and every
/// one of them is a way a live run would otherwise end early or hang: a live source has
/// no total duration, a moment of quiet is not the end of it, and nothing ever finishes.
/// Not `Send`, deliberately. `WavBlockReader` holds a boxed iterator that is not, and
/// the engine has always been one blocking thread that owns everything it touches — the
/// live source hands its audio across a queue rather than crossing the boundary itself.
pub trait BlockSource {
    /// Up to `frames` mono samples. Zero means nothing arrived, which for a file is the
    /// end and for a room is a pause.
    fn read_block(&mut self, out: &mut Vec<f32>, frames: usize) -> Result<usize>;
    fn sample_rate(&self) -> u32;
    /// How long it runs, when that is a knowable thing.
    fn duration_seconds(&self) -> Option<f64>;
    /// Whether an empty read means this source is done. False for anything live.
    fn ends(&self) -> bool {
        true
    }
}

impl BlockSource for WavBlockReader {
    fn read_block(&mut self, out: &mut Vec<f32>, frames: usize) -> Result<usize> {
        WavBlockReader::read_block(self, out, frames)
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn duration_seconds(&self) -> Option<f64> {
        Some(WavBlockReader::duration_seconds(self))
    }
}

struct Source {
    index: u16,
    character: Option<String>,
    /// Everyone this channel might carry, when a mic is shared. See `ChannelSpec`.
    characters: Vec<String>,
    lang: Option<LangCode>,
    reader: Box<dyn BlockSource>,
    frontend: ChannelFrontend,
    finished: bool,
}

/// Heap key: segments are decoded in the order their audio *ended*, across all
/// channels. Microseconds because f64 has no total order; channel and sequence
/// break ties so a run is reproducible rather than merely usually-the-same.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Key(i64, u16, u64);

pub struct Engine {
    /// Set to end a run that has no end of its own. A file stops when it runs out; a
    /// room does not, so somebody has to say when.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    cfg: EngineConfig,
    vad: SileroSession,
    whisper: WhisperEngine,
    filter: HallucinationFilter,
    /// Optional tap on the raw blocks; see `BlockSink`.
    monitor: Option<Box<dyn BlockSink>>,
}

impl Engine {
    pub fn load(cfg: EngineConfig) -> Result<Self> {
        let vad = SileroSession::load(&cfg.vad_model)
            .with_context(|| format!("loading VAD model {}", cfg.vad_model.display()))?;
        let whisper = WhisperEngine::load(&cfg.whisper_model, cfg.whisper.clone())
            .with_context(|| format!("loading model {}", cfg.whisper_model.display()))?;
        let filter = HallucinationFilter::new(cfg.filter.clone());
        Ok(Engine {
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cfg,
            vad,
            whisper,
            filter,
            monitor: None,
        })
    }

    /// A handle that ends a live run.
    ///
    /// The engine owns Metal and cannot be reached from another thread, so the one thing
    /// somebody outside needs — "stop" — travels as a flag rather than as a call.
    pub fn stop_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.stop)
    }

    /// Attach a tap that sees every block as it is read.
    ///
    /// Only the first selected channel is sent: monitoring exists so a person can
    /// hear what is being analysed, and summing several actors' close mics would
    /// produce something nobody's ears or the recogniser ever get.
    pub fn set_monitor(&mut self, sink: Box<dyn BlockSink>) {
        self.monitor = Some(sink);
    }

    /// Run the whole corpus through the pipeline.
    pub fn run(&mut self, corpus: &Corpus, consumer: &mut dyn Consumer) -> Result<ClockStats> {
        let sources = self.open_sources(corpus)?;
        self.pump(sources, consumer)
    }

    /// Run against the room instead of a file.
    ///
    /// One source per patched channel, and the cast's channel numbers decide who each
    /// carries — so a patched multitrack show discriminates speakers and a single mixed
    /// feed does not, which is the same rule as the replay path, arrived at from the
    /// room instead of a manifest.
    pub fn run_live(
        &mut self,
        capture: &crate::capture::Capture,
        who: &[(u16, Vec<String>)],
        consumer: &mut dyn Consumer,
    ) -> Result<ClockStats> {
        let sources: Vec<Source> = capture
            .channels
            .iter()
            .map(|b| {
                let carried: Vec<String> = who
                    .iter()
                    .find(|(ch, _)| *ch == b.logical)
                    .map(|(_, names)| names.clone())
                    .unwrap_or_default();
                Ok(Source {
                    index: b.logical,
                    character: carried.first().cloned(),
                    characters: carried,
                    lang: None,
                    reader: Box::new(crate::capture::LiveSource::new(std::sync::Arc::clone(b))),
                    frontend: ChannelFrontend::with_agc(
                        b.logical,
                        self.cfg.vad.clone(),
                        self.cfg.agc.clone(),
                    )?,
                    finished: false,
                })
            })
            .collect::<Result<_>>()?;
        self.pump(sources, consumer)
    }

    /// The loop itself, whatever the audio came from.
    fn pump(
        &mut self,
        mut sources: Vec<Source>,
        consumer: &mut dyn Consumer,
    ) -> Result<ClockStats> {
        if sources.is_empty() {
            anyhow::bail!("no channels selected");
        }
        let rate = sources[0].reader.sample_rate() as f64;
        let block_frames = (BLOCK_SECONDS * rate) as usize;
        // `None` when anything is live: there is no total, so nothing may be clamped to
        // it and the run cannot be scored against it.
        let audio_s: Option<f64> = sources
            .iter()
            .map(|s| s.reader.duration_seconds())
            .try_fold(0.0f64, |a, d| d.map(|d| a.max(d)));
        // Live sources are paced by the device, so the clock only measures. A run with
        // no total that also slept against one would drift by whatever the device's
        // rate really is.
        let live = audio_s.is_none();

        let mut clock = if self.cfg.realtime {
            VirtualClock::realtime()
        } else {
            VirtualClock::batch()
        };
        let mut queue: BinaryHeap<Reverse<(Key, usize)>> = BinaryHeap::new();
        let mut pending: Vec<AudioSegment> = Vec::new();
        let mut arena: Vec<Option<AudioSegment>> = Vec::new();
        let mut seq = 0u64;
        let mut block_buf: Vec<f32> = Vec::with_capacity(block_frames);
        let mut audio_t = 0.0f64;
        let monitored = sources[0].index;

        loop {
            let mut any = false;
            for src in &mut sources {
                if src.finished {
                    continue;
                }
                let n = src.reader.read_block(&mut block_buf, block_frames)?;
                pending.clear();
                if n == 0 {
                    // A file that has run out is done. A room that has gone quiet is a
                    // room that has gone quiet.
                    if src.reader.ends() {
                        src.finished = true;
                        src.frontend.finish(&mut self.vad, &mut pending)?;
                    }
                } else {
                    any = true;
                    if let Some(m) = self.monitor.as_mut() {
                        if src.index == monitored {
                            // Blocks until the device has room. This, not the clock,
                            // is what paces a monitored run.
                            m.on_block(src.index, &block_buf[..n]);
                        }
                    }
                    src.frontend
                        .push_block(&mut self.vad, &block_buf, &mut pending)?;
                }
                for seg in pending.drain(..) {
                    let key = Key((seg.t_end * 1e6) as i64, src.index, seq);
                    seq += 1;
                    arena.push(Some(seg));
                    queue.push(Reverse((key, arena.len() - 1)));
                }
            }
            audio_t += BLOCK_SECONDS;

            // Nothing may be decoded before its audio has actually happened.
            if self.cfg.realtime && !live {
                clock.wait_until(audio_t.min(audio_s.unwrap_or(audio_t)));
            }
            self.drain(&mut queue, &mut arena, &sources, &mut clock, consumer)?;

            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            // A live run ends when it is told to, never because nothing was said.
            if !live && !any && sources.iter().all(|s| s.finished) {
                break;
            }
        }
        // Anything left after the last block (a run closed by `finish`).
        self.drain(&mut queue, &mut arena, &sources, &mut clock, consumer)?;
        Ok(clock.stats(audio_s.unwrap_or(audio_t)))
    }

    fn drain(
        &mut self,
        queue: &mut BinaryHeap<Reverse<(Key, usize)>>,
        arena: &mut [Option<AudioSegment>],
        sources: &[Source],
        clock: &mut VirtualClock,
        consumer: &mut dyn Consumer,
    ) -> Result<()> {
        while let Some(Reverse((key, slot))) = queue.pop() {
            let Some(seg) = arena[slot].take() else {
                continue;
            };
            let channel = key.1;
            let src = sources
                .iter()
                .find(|s| s.index == channel)
                .expect("segment came from a source we opened");

            let hint = consumer.decode_hint(channel, src.character.as_deref());
            // A language pinned on the channel in the manifest overrides the script:
            // the escape hatch for a mic known to carry one language whatever the
            // script says.
            let candidates: Vec<LangCode> = match &src.lang {
                Some(l) => vec![l.clone()],
                None if hint.langs.is_empty() => vec![LangCode::new("en")],
                None => hint.langs.clone(),
            };

            let mut best: Option<(LangCode, choufleur_asr::DecodeOutput)> = None;
            for cand in &candidates {
                let out = self
                    .whisper
                    .transcribe(&seg.samples, cand, hint.prompt.as_deref())?;
                // Whisper's own confidence is the arbiter. A decode forced to the
                // wrong language scores markedly worse — exactly the signal needed,
                // and free to read.
                let better = best
                    .as_ref()
                    .is_none_or(|(_, b)| out.avg_logprob > b.avg_logprob);
                if better {
                    best = Some((cand.clone(), out));
                }
            }
            let (lang, decoded) = best.expect("at least one candidate language");
            let verdict = self.filter.check(&decoded, seg.duration());

            let record = SegmentRecord {
                gain_db: Some(src.frontend.gain_db()),
                speech_dbfs: Some(src.frontend.speech_dbfs()),
                channel,
                character: src.character.clone(),
                characters: src.characters.clone(),
                t_start: seg.t_start,
                t_end: seg.t_end,
                text: decoded.text.clone(),
                langs: vec![lang],
                avg_logprob: decoded.avg_logprob,
                no_speech_prob: decoded.no_speech_prob,
                forced_split: seg.forced_split,
                interim: seg.interim,
                decode_ms: Some(decoded.decode_ms),
                latency_ms: clock.is_realtime().then(|| clock.latency_ms_for(seg.t_end)),
                filtered: verdict.reason().map(|r| r.as_str().to_string()),
            };
            consumer.on_segment(&record);
        }
        Ok(())
    }

    fn open_sources(&self, corpus: &Corpus) -> Result<Vec<Source>> {
        let mut sources = Vec::new();
        if self.cfg.mixdown {
            let mix = corpus
                .manifest
                .mixdown
                .as_ref()
                .context("--mixdown asked for, but this corpus has no mixdown")?;
            let path = corpus.resolve_audio(&mix.file);
            let reader = WavBlockReader::open(&path)?;
            sources.push(Source {
                index: 0,
                // A mix carries no speaker identity: it is a zone channel by nature.
                character: None,
                characters: Vec::new(),
                lang: None,
                reader: Box::new(reader),
                frontend: ChannelFrontend::with_agc(0, self.cfg.vad.clone(), self.cfg.agc.clone())?,
                finished: false,
            });
            return Ok(sources);
        }

        for ch in &corpus.manifest.channels {
            if let Some(want) = &self.cfg.channels {
                if !want.contains(&ch.index) {
                    continue;
                }
            }
            let path = corpus.resolve_audio(&ch.audio.file);
            let reader = WavBlockReader::open(&path)?;
            sources.push(Source {
                index: ch.index,
                character: ch.primary().map(str::to_string),
                characters: ch.speakers().to_vec(),
                lang: ch.lang.clone(),
                reader: Box::new(reader),
                frontend: ChannelFrontend::with_agc(ch.index, self.cfg.vad.clone(), self.cfg.agc.clone())?,
                finished: false,
            });
        }
        Ok(sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_ordered_by_when_their_audio_ended() {
        // The heap key is what makes one Whisper engine round-robin across active
        // channels without a scheduler; if it ever stops being a total order, runs
        // stop being reproducible.
        let mut heap: BinaryHeap<Reverse<(Key, usize)>> = BinaryHeap::new();
        heap.push(Reverse((Key(3_000_000, 2, 0), 0)));
        heap.push(Reverse((Key(1_000_000, 5, 1), 1)));
        heap.push(Reverse((Key(2_000_000, 1, 2), 2)));
        // Same instant on two channels: the lower channel goes first, always.
        heap.push(Reverse((Key(1_000_000, 3, 3), 3)));

        let order: Vec<(i64, u16)> = std::iter::from_fn(|| heap.pop())
            .map(|Reverse((k, _))| (k.0, k.1))
            .collect();
        assert_eq!(
            order,
            vec![
                (1_000_000, 3),
                (1_000_000, 5),
                (2_000_000, 1),
                (3_000_000, 2)
            ]
        );
    }

    #[test]
    fn a_block_is_a_sensible_size() {
        // Small enough to notice a closed segment promptly against a 1.5 s budget,
        // large enough that per-block bookkeeping is noise.
        const { assert!(BLOCK_SECONDS <= 0.5 && BLOCK_SECONDS >= 0.05) };
        assert_eq!((BLOCK_SECONDS * 48_000.0) as usize, 12_000);
    }
}
