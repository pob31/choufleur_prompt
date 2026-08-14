//! `transcribe` — audio in, transcript segments out.
//!
//! The expensive half of the pipeline, run once and written to disk so that every
//! subsequent matcher experiment costs milliseconds instead of minutes. It also
//! reports the numbers the devplan's compute criterion is stated in: how much
//! faster than real time this machine is, and whether it ever fell behind.

use std::path::{Path, PathBuf};

use anyhow::Result;
use choufleur_core::lang::LangCode;
use choufleur_core::prompt::{proper_nouns, static_prompt_with, BiasMode};

use crate::engine::{Consumer, DecodeHint, Engine, EngineConfig};
use crate::formats::{write_jsonl, SegmentRecord};
use crate::manifest::Corpus;

/// Collects segments, and supplies a per-show constant prompt if asked.
///
/// This consumer has no memory of position — that is what makes it the *control*
/// condition for the biasing sweep. Tracker-fed biasing lives in `track --from-audio`.
struct Collector {
    records: Vec<SegmentRecord>,
    bias: BiasMode,
    /// Every language a character speaks, resolved from the script.
    lang_of: std::collections::HashMap<String, Vec<LangCode>>,
    default_lang: LangCode,
    static_prompts: std::collections::HashMap<String, String>,
    progress_every: usize,
}

impl Consumer for Collector {
    fn decode_hint(&mut self, _channel: u16, character: Option<&str>) -> DecodeHint {
        // Without a position there is no way to know *which* of a character's
        // languages this segment is in, so offer them all and let the decoder's
        // own confidence choose. `track --from-audio` does better: it knows where
        // the show is and can name the language of the line it expects.
        let langs = character
            .and_then(|c| self.lang_of.get(c))
            .cloned()
            .unwrap_or_else(|| vec![self.default_lang.clone()]);
        let lang = langs
            .first()
            .cloned()
            .unwrap_or_else(|| self.default_lang.clone());
        let prompt = match self.bias {
            BiasMode::None => None,
            // Tracker biasing is unavailable here by construction: nothing in this
            // command knows where the show is. `track --from-audio --bias tracker`
            // is the mode that can.
            BiasMode::Static | BiasMode::Tracker => self.static_prompts.get(lang.as_str()).cloned(),
        };
        DecodeHint { langs, prompt }
    }

    fn on_segment(&mut self, record: &SegmentRecord) {
        self.records.push(record.clone());
        let n = self.records.len();
        if self.progress_every > 0 && n.is_multiple_of(self.progress_every) {
            let kept = self.records.iter().filter(|r| r.is_kept()).count();
            eprintln!(
                "  {n} segment(s) decoded, {kept} kept, at {:.0} s",
                record.t_end
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    corpus_path: &Path,
    out: &Path,
    whisper_model: Option<&Path>,
    vad_model: Option<&Path>,
    realtime: bool,
    bias: BiasMode,
    mixdown: bool,
    channels: Option<Vec<u16>>,
    interim_ms: Option<u32>,
    audio_root: Option<PathBuf>,
    no_agc: bool,
) -> Result<()> {
    let corpus = Corpus::load(corpus_path, audio_root)?;
    let (script, prepared) = super::load_script(&corpus.script_path())?;

    let mut cfg = EngineConfig::new(
        super::resolve_model(whisper_model, super::DEFAULT_WHISPER_MODEL)?,
        super::resolve_model(vad_model, super::DEFAULT_VAD_MODEL)?,
    );
    cfg.realtime = realtime;
    cfg.agc.enabled = !no_agc;
    cfg.mixdown = mixdown;
    cfg.channels = channels;
    if let Some(ms) = interim_ms {
        cfg.vad.interim_interval_ms = ms;
    }

    println!("model:   {}", cfg.whisper_model.display());
    println!("vad:     {}", cfg.vad_model.display());
    println!("bias:    {bias:?}");
    println!("mode:    {}", if realtime { "realtime" } else { "batch" });

    let names: Vec<String> = script.characters.iter().map(|c| c.name.clone()).collect();
    let proper = proper_nouns(&prepared.lines, 40);
    if !proper.is_empty() {
        println!("lexicon: {}", proper.join(", "));
    }
    let mut static_prompts = std::collections::HashMap::new();
    for lang in super::languages_used(&prepared) {
        static_prompts.insert(
            lang.as_str().to_string(),
            static_prompt_with(script.title.as_deref(), &names, &proper),
        );
    }

    let mut collector = Collector {
        records: Vec::new(),
        bias,
        lang_of: super::character_languages(&prepared),
        default_lang: script
            .default_lang
            .first()
            .cloned()
            .unwrap_or(LangCode::new("en")),
        static_prompts,
        progress_every: 25,
    };

    let mut engine = Engine::load(cfg)?;
    let stats = engine.run(&corpus, &mut collector)?;

    write_jsonl(out, &collector.records)?;

    let kept = collector.records.iter().filter(|r| r.is_kept()).count();
    let interim = collector.records.iter().filter(|r| r.interim).count();
    let decode_total: u64 = collector.records.iter().filter_map(|r| r.decode_ms).sum();
    let speech_s: f64 = collector.records.iter().map(|r| r.t_end - r.t_start).sum();

    println!("\n{stats}");
    println!(
        "{} segment(s): {kept} kept, {} filtered, {interim} interim",
        collector.records.len(),
        collector.records.len() - kept
    );
    if decode_total > 0 {
        println!(
            "decode:  {:.1} s total for {:.1} s of speech ({:.1}× faster than the speech itself)",
            decode_total as f64 / 1000.0,
            speech_s,
            speech_s / (decode_total as f64 / 1000.0)
        );
    }
    if stats.fell_behind > 0 {
        // Deliberately not phrased as failure. Decoding is bursty by nature: when
        // several channels close a segment at once they are decoded back to back,
        // so the run drops behind and catches up in the next quiet moment. What
        // decides whether that matters is the end-to-end latency distribution,
        // which `eval` reports against the budget — not the backlog itself.
        println!(
            "note:    processing was bursty — behind on {} occasion(s), worst {:.2} s.\n\
             \x20        Whether that matters is the latency distribution: eval --segments.",
            stats.fell_behind, stats.max_backlog_s
        );
    }
    println!("wrote {}", out.display());
    Ok(())
}
