//! Whisper decoding, via whisper.cpp.
//!
//! One model loaded once, one reusable state, segments decoded sequentially in
//! global timestamp order. That is the PRD's shared-model policy, and it is also
//! the only safe arrangement: `full` takes `&mut self`, so one state serves one
//! decode at a time by construction.
//!
//! Several defaults in whisper.cpp are wrong for this application and are set
//! explicitly rather than inherited — see [`WhisperOpts`] and the notes on each
//! parameter in [`WhisperEngine::transcribe`].

use std::path::Path;

use choufleur_core::lang::LangCode;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperTokenId,
};

use crate::filter::DecodeOutput;
use crate::AsrError;

#[derive(Clone, Debug)]
pub struct WhisperOpts {
    pub n_threads: i32,
    pub use_gpu: bool,
    /// Trims the encoder's audio context. A common speed trick for short clips,
    /// marked experimental upstream, and it costs accuracy. Off by default; a
    /// value above the model's own `n_audio_ctx` makes decoding fail outright.
    pub audio_ctx: Option<i32>,
    pub no_speech_thold: f32,
    /// Cap on prompt tokens. whisper.cpp keeps only the last ~223 and trims the
    /// front silently, so the imminent lines must be at the end of the prompt.
    pub max_prompt_tokens: usize,
}

impl Default for WhisperOpts {
    fn default() -> Self {
        WhisperOpts {
            n_threads: 4,
            use_gpu: true,
            audio_ctx: None,
            no_speech_thold: 0.6,
            max_prompt_tokens: 200,
        }
    }
}

pub struct WhisperEngine {
    ctx: WhisperContext,
    state: whisper_rs::WhisperState,
    opts: WhisperOpts,
    /// Text tokens have ids below this; specials and timestamps are at or above.
    eot: WhisperTokenId,
}

impl WhisperEngine {
    pub fn load(model: &Path, opts: WhisperOpts) -> Result<Self, AsrError> {
        if !model.exists() {
            return Err(AsrError::Model(format!(
                "{} not found — run scripts/fetch-models.sh",
                model.display()
            )));
        }
        let mut cparams = WhisperContextParameters::default();
        cparams.use_gpu(opts.use_gpu);
        cparams.flash_attn(false);

        let ctx = WhisperContext::new_with_params(
            model
                .to_str()
                .ok_or_else(|| AsrError::Model("model path is not UTF-8".into()))?,
            cparams,
        )
        .map_err(|e| AsrError::Model(format!("loading {}: {e}", model.display())))?;
        let state = ctx
            .create_state()
            .map_err(|e| AsrError::Model(format!("creating decoder state: {e}")))?;
        let eot = ctx.token_eot();

        let mut engine = WhisperEngine {
            ctx,
            state,
            opts,
            eot,
        };
        // One throwaway decode so the first real segment does not pay for Metal
        // shader compilation. Without this the first measured latency is an
        // outlier by a second or more, which would poison the p95.
        let warmup = vec![0.0f32; 16_000];
        let _ = engine.transcribe(&warmup, &LangCode::new("en"), None);
        Ok(engine)
    }

    /// Is this a language code whisper.cpp actually knows?
    ///
    /// Worth checking, because it does not check: an unknown code yields language
    /// id -1, which is passed straight through to the decoder as an out-of-range
    /// token and produces confident nonsense with no error anywhere.
    pub fn supports_language(lang: &LangCode) -> bool {
        whisper_rs::get_lang_id(lang.primary()).is_some()
    }

    /// Decode one segment. `initial_prompt` biases the decoder's vocabulary toward
    /// text we expect (PRD, *ASR Engine and Latency Budget*).
    pub fn transcribe(
        &mut self,
        audio_16k: &[f32],
        lang: &LangCode,
        initial_prompt: Option<&str>,
    ) -> Result<DecodeOutput, AsrError> {
        if audio_16k.is_empty() {
            return Err(AsrError::Audio("empty segment".into()));
        }
        let code = lang.primary().to_string();
        if whisper_rs::get_lang_id(&code).is_none() {
            return Err(AsrError::Model(format!(
                "whisper does not know language {code:?}; tag the script with a \
                 language it supports or the decode will be silent nonsense"
            )));
        }

        // Prompts go in as pre-tokenized ids rather than through set_initial_prompt.
        // That setter allocates a CString and leaks it — FullParams has no Drop —
        // so a show-length run with a fresh prompt per segment would leak steadily.
        // set_tokens stores a borrowed pointer and allocates nothing.
        let prompt_tokens: Vec<i32> = match initial_prompt.filter(|p| !p.trim().is_empty()) {
            Some(p) => {
                let mut t = self
                    .ctx
                    .tokenize(p, self.opts.max_prompt_tokens * 4)
                    .unwrap_or_default();
                if t.len() > self.opts.max_prompt_tokens {
                    // Keep the tail: whisper trims the front, and our prompts put
                    // the most imminent lines last for exactly this reason.
                    t.drain(..t.len() - self.opts.max_prompt_tokens);
                }
                t
            }
            None => Vec::new(),
        };

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.opts.n_threads);
        params.set_language(Some(&code));
        params.set_translate(false);
        // Independent segments. Also clears prompt history, so a previous line
        // cannot bleed into this decode.
        params.set_no_context(true);
        params.set_single_segment(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_no_speech_thold(self.opts.no_speech_thold);
        // Temperature fallback silently re-decodes a poor segment at successively
        // higher temperatures, multiplying worst-case latency. We would rather
        // have a predictable decode and let the hallucination filter judge it.
        params.set_temperature(0.0);
        params.set_temperature_inc(0.0);
        // whisper.cpp prints progress to stdout by default; four flags to silence it.
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        if let Some(n) = self.opts.audio_ctx {
            params.set_audio_ctx(n);
        }
        if !prompt_tokens.is_empty() {
            params.set_tokens(&prompt_tokens);
        }

        let started = std::time::Instant::now();
        self.state
            .full(params, audio_16k)
            .map_err(|e| AsrError::Model(format!("decoding: {e}")))?;
        let decode_ms = started.elapsed().as_millis() as u64;

        // A successful decode can still produce nothing: whisper.cpp returns 0 with
        // zero segments for very short input rather than an error.
        let mut text = String::new();
        let mut logprob_sum = 0.0f64;
        let mut logprob_count = 0usize;
        let mut no_speech_prob = 0.0f32;

        for i in 0..self.state.full_n_segments() {
            let Some(seg) = self.state.get_segment(i) else {
                continue;
            };
            // Lossy: whisper splits multi-byte characters across token boundaries,
            // and a French act will hit that within the first minute.
            if let Ok(s) = seg.to_str_lossy() {
                text.push_str(&s);
            }
            if i == 0 {
                // One value per decode window, copied onto every segment; with
                // single_segment there is exactly one to read.
                no_speech_prob = seg.no_speech_probability();
            }
            for t in 0..seg.n_tokens() {
                let Some(tok) = seg.get_token(t) else {
                    continue;
                };
                let data = tok.token_data();
                if data.id >= self.eot {
                    continue; // specials and timestamps would skew the mean
                }
                logprob_sum += data.plog as f64; // already a natural log
                logprob_count += 1;
            }
        }

        Ok(DecodeOutput {
            text: text.trim().to_string(),
            avg_logprob: if logprob_count == 0 {
                // No text: report the worst rather than a flattering zero, so the
                // filter's logprob rule treats it as the non-answer it is.
                -10.0
            } else {
                (logprob_sum / logprob_count as f64) as f32
            },
            no_speech_prob,
            lang: lang.clone(),
            decode_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/ggml-small.bin")
    }

    #[test]
    fn a_missing_model_says_what_to_do_about_it() {
        let Err(err) =
            WhisperEngine::load(Path::new("/nonexistent/model.bin"), WhisperOpts::default())
        else {
            panic!("loading a nonexistent model should fail");
        };
        assert!(format!("{err}").contains("fetch-models.sh"), "{err}");
    }

    #[test]
    fn language_support_is_checked_rather_than_assumed() {
        assert!(WhisperEngine::supports_language(&LangCode::new("fr")));
        assert!(WhisperEngine::supports_language(&LangCode::new("en")));
        assert!(WhisperEngine::supports_language(&LangCode::new("sv")));
        // Region subtags resolve through the primary tag.
        assert!(WhisperEngine::supports_language(&LangCode::new("pt-BR")));
        // And a tag whisper has never heard of must be rejected loudly, because
        // whisper.cpp itself would accept it and decode garbage.
        assert!(!WhisperEngine::supports_language(&LangCode::new("klingon")));
    }

    #[test]
    fn an_unknown_language_is_refused_before_it_can_produce_nonsense() {
        if !model_path().exists() {
            return;
        }
        let Ok(mut e) = WhisperEngine::load(&model_path(), WhisperOpts::default()) else {
            panic!("model is present but failed to load");
        };
        let Err(err) = e.transcribe(&vec![0.0; 16_000], &LangCode::new("klingon"), None) else {
            panic!("an unknown language must be refused, not decoded");
        };
        assert!(format!("{err}").contains("does not know language"), "{err}");
    }

    #[test]
    fn silence_decodes_to_something_the_filter_can_reject() {
        if !model_path().exists() {
            eprintln!("skipping: no model at {}", model_path().display());
            return;
        }
        let Ok(mut e) = WhisperEngine::load(&model_path(), WhisperOpts::default()) else {
            panic!("model is present but failed to load");
        };
        let out = e
            .transcribe(&vec![0.0; 16_000 * 2], &LangCode::new("en"), None)
            .unwrap();
        // Whisper will happily invent something here. The contract is only that it
        // reports its own doubt, so the hallucination filter has something to act on.
        let filter = crate::filter::HallucinationFilter::default();
        let verdict = filter.check(&out, 2.0);
        assert!(
            !verdict.is_keep(),
            "two seconds of digital silence produced {:?} (no_speech {:.2}, logprob {:.2}) \
             and the filter kept it",
            out.text,
            out.no_speech_prob,
            out.avg_logprob
        );
    }
}
