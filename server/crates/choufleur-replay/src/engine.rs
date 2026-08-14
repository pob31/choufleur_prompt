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

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub whisper_model: PathBuf,
    pub vad_model: PathBuf,
    pub vad: VadConfig,
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
struct Source {
    index: u16,
    character: Option<String>,
    lang: Option<LangCode>,
    reader: WavBlockReader,
    frontend: ChannelFrontend,
    finished: bool,
}

/// Heap key: segments are decoded in the order their audio *ended*, across all
/// channels. Microseconds because f64 has no total order; channel and sequence
/// break ties so a run is reproducible rather than merely usually-the-same.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Key(i64, u16, u64);

pub struct Engine {
    cfg: EngineConfig,
    vad: SileroSession,
    whisper: WhisperEngine,
    filter: HallucinationFilter,
}

impl Engine {
    pub fn load(cfg: EngineConfig) -> Result<Self> {
        let vad = SileroSession::load(&cfg.vad_model)
            .with_context(|| format!("loading VAD model {}", cfg.vad_model.display()))?;
        let whisper = WhisperEngine::load(&cfg.whisper_model, cfg.whisper.clone())
            .with_context(|| format!("loading model {}", cfg.whisper_model.display()))?;
        let filter = HallucinationFilter::new(cfg.filter.clone());
        Ok(Engine {
            cfg,
            vad,
            whisper,
            filter,
        })
    }

    /// Run the whole corpus through the pipeline.
    pub fn run(&mut self, corpus: &Corpus, consumer: &mut dyn Consumer) -> Result<ClockStats> {
        let mut sources = self.open_sources(corpus)?;
        if sources.is_empty() {
            anyhow::bail!("no channels selected");
        }
        let rate = sources[0].reader.sample_rate as f64;
        let block_frames = (BLOCK_SECONDS * rate) as usize;
        let audio_s = sources
            .iter()
            .map(|s| s.reader.duration_seconds())
            .fold(0.0f64, f64::max);

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

        loop {
            let mut any = false;
            for src in &mut sources {
                if src.finished {
                    continue;
                }
                let n = src.reader.read_block(&mut block_buf, block_frames)?;
                pending.clear();
                if n == 0 {
                    src.finished = true;
                    src.frontend.finish(&mut self.vad, &mut pending)?;
                } else {
                    any = true;
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
            if self.cfg.realtime {
                clock.wait_until(audio_t.min(audio_s));
            }
            self.drain(&mut queue, &mut arena, &sources, &mut clock, consumer)?;

            if !any && sources.iter().all(|s| s.finished) {
                break;
            }
        }
        // Anything left after the last block (a run closed by `finish`).
        self.drain(&mut queue, &mut arena, &sources, &mut clock, consumer)?;
        Ok(clock.stats(audio_s))
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
                channel,
                character: src.character.clone(),
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
                lang: None,
                reader,
                frontend: ChannelFrontend::new(0, self.cfg.vad.clone())?,
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
                character: ch.character.clone(),
                lang: ch.lang.clone(),
                reader,
                frontend: ChannelFrontend::new(ch.index, self.cfg.vad.clone())?,
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
