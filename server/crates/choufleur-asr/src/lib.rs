//! # choufleur-asr
//!
//! Buffers in, transcript segments out. This crate knows nothing about files,
//! audio devices, or the position tracker — which is what lets the offline replay
//! harness and the live server drive exactly the same recognition path, and is
//! why the "works in the lab" seam the devplan warns about does not exist here.
//!
//! The pipeline, in order:
//!
//! ```text
//! 48 kHz blocks → resample → Silero VAD → [segmenter] → Whisper → [filter] → segments
//! ```
//!
//! ## What is implemented
//!
//! - [`segmenter`] — the VAD state machine: when a speech run opens, closes, and
//!   splits, including the **interim emission** policy that keeps detection lag
//!   inside the PRD's budget. Pure; no model required.
//! - [`filter`] — the hallucination filter that stands between Whisper's
//!   imagination and the tracker's position. Pure; no model required.
//!
//! ## What is not yet implemented
//!
//! The two model-bound stages, which are the next milestone's work:
//!
//! - `resample` — streaming 48 kHz → 16 kHz (`rubato`), with the group delay
//!   subtracted once so every VAD timestamp is not quietly shifted.
//! - `vad` — one shared `ort` session running Silero v5, with per-channel state.
//!   Note the v5 input tensor is `[1, 576]`: 64 samples of carried context plus the
//!   512-sample window, and the context must be reset together with the state.
//! - `whisper` — one `whisper-rs` context and one reusable state, decoding
//!   segments sequentially in global timestamp order (the PRD's shared-model,
//!   round-robin policy), with the language forced per segment from the script and
//!   `initial_prompt` biasing supplied by the caller.
//!
//! The boundary those stages plug into is already fixed by [`segmenter::AudioSegment`]
//! going in and [`filter::DecodeOutput`] coming out, so adding them changes no
//! signature elsewhere.

pub mod filter;
pub mod segmenter;

pub use filter::{DecodeOutput, DropReason, FilterConfig, HallucinationFilter, Verdict};
pub use segmenter::{AudioSegment, Segmenter, VadConfig};

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("audio format: {0}")]
    Audio(String),
    #[error("model: {0}")]
    Model(String),
}
