//! Silero VAD v5, via ONNX Runtime.
//!
//! VAD gating is mandatory, not an optimization: Whisper hallucinates fluent text
//! on non-speech input (PRD, *Hallucination on silence and noise*), so anything
//! that reaches the recognizer without passing here becomes invented dialogue with
//! a plausible timestamp.
//!
//! # The 576-sample trap
//!
//! Silero v5 does **not** take a 512-sample window. Its `input` tensor is
//! `[1, 576]` — 64 samples of context carried from the previous window, followed
//! by the 512 new ones. The model declares its input shape as fully dynamic
//! (`[-1, -1]`), so feeding 512 samples is *silently accepted* and returns a
//! plausible-looking probability computed from the wrong thing. There is no error
//! to catch. The context and the recurrent state must also be reset together, or a
//! channel restarts mid-utterance with stale history.
//!
//! One [`SileroSession`] serves every channel: the model is stateless and all
//! per-channel memory travels in the tensors, so a second session would only cost
//! memory and load time. Inference is serialized — ONNX Runtime's `Run` is not
//! thread-safe, and `run` takes `&mut self` to enforce it.

use std::path::Path;

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

use crate::AsrError;

/// New samples per step: 32 ms at 16 kHz.
pub const WINDOW: usize = 512;
/// Samples carried forward from the previous window.
pub const CONTEXT: usize = 64;
/// Flattened `[2, 1, 128]` recurrent state.
pub const STATE_LEN: usize = 256;
pub const SAMPLE_RATE: i64 = 16_000;

/// The loaded model. Cheap to share, expensive to duplicate.
pub struct SileroSession {
    session: Session,
}

/// One channel's memory of what it has heard. Reset with [`SileroChannelState::reset`].
#[derive(Clone)]
pub struct SileroChannelState {
    state: [f32; STATE_LEN],
    context: [f32; CONTEXT],
}

impl Default for SileroChannelState {
    fn default() -> Self {
        SileroChannelState {
            state: [0.0; STATE_LEN],
            context: [0.0; CONTEXT],
        }
    }
}

impl SileroChannelState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything. Both halves go together — a fresh recurrent state with a
    /// stale context window is a subtly wrong start, not a clean one.
    pub fn reset(&mut self) {
        self.state = [0.0; STATE_LEN];
        self.context = [0.0; CONTEXT];
    }
}

impl SileroSession {
    pub fn load(model: &Path) -> Result<Self, AsrError> {
        if !model.exists() {
            return Err(AsrError::Model(format!(
                "{} not found — run scripts/fetch-models.sh",
                model.display()
            )));
        }
        let session = build_session(model)
            .map_err(|e| AsrError::Model(format!("loading {}: {e}", model.display())))?;
        Ok(SileroSession { session })
    }

    /// Speech probability for one 512-sample window.
    ///
    /// Takes a fixed-size array rather than a slice so the 512/576 confusion cannot
    /// be made at a call site — the compiler checks what the model will not.
    pub fn process_window(
        &mut self,
        st: &mut SileroChannelState,
        window: &[f32; WINDOW],
    ) -> Result<f32, AsrError> {
        let mut frame = Vec::with_capacity(CONTEXT + WINDOW);
        frame.extend_from_slice(&st.context);
        frame.extend_from_slice(window);

        let input = Tensor::from_array(([1usize, CONTEXT + WINDOW], frame))
            .map_err(|e| AsrError::Vad(format!("building input tensor: {e}")))?;
        let state = Tensor::from_array(([2usize, 1, 128], st.state.to_vec()))
            .map_err(|e| AsrError::Vad(format!("building state tensor: {e}")))?;
        let sr = Tensor::from_array(((), vec![SAMPLE_RATE]))
            .map_err(|e| AsrError::Vad(format!("building sample-rate tensor: {e}")))?;

        // `ort::inputs!` is not fallible in this release — no `?` on the macro.
        let outputs = self
            .session
            .run(ort::inputs! { "input" => input, "state" => state, "sr" => sr })
            .map_err(|e| AsrError::Vad(format!("running VAD: {e}")))?;

        // Indexing SessionOutputs by an unknown name panics, so go through `get`.
        // This doubles as the model-version check: a Silero v4 file has `h`/`c`
        // outputs instead of `stateN` and would otherwise fail somewhere less
        // obvious, or not at all.
        let prob = outputs
            .get("output")
            .ok_or_else(|| AsrError::Vad(unexpected_model("output")))?
            .try_extract_scalar::<f32>()
            .map_err(|e| AsrError::Vad(format!("reading probability: {e}")))?;

        let (_shape, new_state) = outputs
            .get("stateN")
            .ok_or_else(|| AsrError::Vad(unexpected_model("stateN")))?
            .try_extract_tensor::<f32>()
            .map_err(|e| AsrError::Vad(format!("reading state: {e}")))?;
        if new_state.len() != STATE_LEN {
            return Err(AsrError::Vad(format!(
                "VAD returned {} state values, expected {STATE_LEN}",
                new_state.len()
            )));
        }
        st.state.copy_from_slice(new_state);
        st.context.copy_from_slice(&window[WINDOW - CONTEXT..]);
        Ok(prob)
    }
}

/// Builder options each return their own error type, so the chain lives in its own
/// function and the conversion to [`AsrError`] happens once at the call site.
///
/// Single-threaded on purpose: this model is tiny, the run is serialized across
/// channels anyway, and one thread makes the probabilities reproducible — which
/// matters, because they decide where every segment boundary falls.
fn build_session(model: &Path) -> ort::Result<Session> {
    Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_intra_threads(1)?
        .with_inter_threads(1)?
        .commit_from_file(model)
}

fn unexpected_model(missing: &str) -> String {
    format!(
        "the VAD model has no `{missing}` tensor. Silero v5 is required (inputs \
         input/state/sr, outputs output/stateN); a v4 file exposes h/c instead. \
         Re-run scripts/fetch-models.sh."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn model_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/silero_vad.onnx")
    }

    fn have_model() -> bool {
        model_path().exists()
    }

    #[test]
    fn state_reset_clears_both_halves_together() {
        let mut st = SileroChannelState::new();
        st.state[7] = 0.5;
        st.context[3] = 0.5;
        st.reset();
        assert!(st.state.iter().all(|v| *v == 0.0));
        assert!(st.context.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn a_missing_model_says_what_to_do_about_it() {
        let Err(err) = SileroSession::load(Path::new("/nonexistent/silero.onnx")) else {
            panic!("loading a nonexistent model should fail");
        };
        assert!(format!("{err}").contains("fetch-models.sh"), "{err}");
    }

    /// These need `scripts/fetch-models.sh` to have run.
    #[test]
    fn silence_and_speech_are_told_apart() {
        if !have_model() {
            eprintln!("skipping: no model at {}", model_path().display());
            return;
        }
        let mut vad = SileroSession::load(&model_path()).unwrap();
        let mut st = SileroChannelState::new();

        // Digital silence must not read as speech.
        let quiet = [0.0f32; WINDOW];
        let mut silence_max = 0.0f32;
        for _ in 0..20 {
            silence_max = silence_max.max(vad.process_window(&mut st, &quiet).unwrap());
        }
        assert!(silence_max < 0.3, "silence scored {silence_max}");

        // A voiced-formant-ish buzz should read higher than silence does. This is a
        // sanity check on the plumbing, not on Silero's accuracy: real validation
        // is the fixture run, where actual speech goes through.
        st.reset();
        let mut n = 0usize;
        let mut buzz_max = 0.0f32;
        for _ in 0..40 {
            let w: [f32; WINDOW] = std::array::from_fn(|i| {
                let t = (n + i) as f32 / 16_000.0;
                ((t * TAU * 120.0).sin() * 0.6 + (t * TAU * 700.0).sin() * 0.3) * 0.8
            });
            n += WINDOW;
            buzz_max = buzz_max.max(vad.process_window(&mut st, &w).unwrap());
        }
        assert!(
            buzz_max > silence_max,
            "buzz {buzz_max} did not exceed silence {silence_max}"
        );
    }

    #[test]
    fn one_session_serves_many_channels_independently() {
        if !have_model() {
            return;
        }
        let mut vad = SileroSession::load(&model_path()).unwrap();
        let quiet = [0.0f32; WINDOW];

        // Two channels through one session, interleaved, must match each channel
        // run on its own — otherwise state is leaking between them.
        let mut a = SileroChannelState::new();
        let mut b = SileroChannelState::new();
        let mut interleaved = Vec::new();
        for _ in 0..8 {
            interleaved.push(vad.process_window(&mut a, &quiet).unwrap());
            let _ = vad.process_window(&mut b, &quiet).unwrap();
        }

        let mut solo_state = SileroChannelState::new();
        let solo: Vec<f32> = (0..8)
            .map(|_| vad.process_window(&mut solo_state, &quiet).unwrap())
            .collect();
        assert_eq!(
            interleaved, solo,
            "channel state leaked through the shared session"
        );
    }

    #[test]
    fn the_carried_context_actually_changes_the_answer() {
        // If this ever stops being true, the 64-sample context is not reaching the
        // model and the 576-sample trap has been silently re-entered.
        if !have_model() {
            return;
        }
        let mut vad = SileroSession::load(&model_path()).unwrap();
        let loud: [f32; WINDOW] =
            std::array::from_fn(|i| (i as f32 * TAU * 300.0 / 16_000.0).sin() * 0.7);

        let mut fresh = SileroChannelState::new();
        let first = vad.process_window(&mut fresh, &loud).unwrap();
        let second = vad.process_window(&mut fresh, &loud).unwrap();
        assert_ne!(
            first, second,
            "identical windows gave identical answers — state/context is not being carried"
        );
    }
}
