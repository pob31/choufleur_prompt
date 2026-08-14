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
//! 48 kHz blocks -> resample -> Silero VAD -> segmenter -> Whisper -> filter -> segments
//!    channel.rs    resample.rs   vad.rs     segmenter.rs  whisper.rs  filter.rs
//! ```
//!
//! [`channel::ChannelFrontend`] composes the first four for one channel;
//! [`whisper::WhisperEngine`] and [`filter::HallucinationFilter`] are shared
//! across channels and driven by the caller in global timestamp order.
//!
//! Three things here are load-bearing and easy to get subtly wrong, so each is
//! documented at its definition: the 576-sample Silero input ([`vad`]), the
//! constant resampler group delay ([`channel`]), and the interim emission policy
//! that keeps detection lag inside budget ([`segmenter`]).

pub mod agc;
pub mod channel;
pub mod filter;
pub mod resample;
pub mod segmenter;
pub mod vad;
pub mod whisper;

pub use agc::{AgcConfig, AutoGain};
pub use channel::ChannelFrontend;
pub use filter::{DecodeOutput, DropReason, FilterConfig, HallucinationFilter, Verdict};
pub use resample::Resampler48to16;
pub use segmenter::{AudioSegment, Segmenter, VadConfig};
pub use vad::{SileroChannelState, SileroSession};
pub use whisper::{WhisperEngine, WhisperOpts};

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("audio: {0}")]
    Audio(String),
    #[error("resampling: {0}")]
    Resample(String),
    #[error("voice activity detection: {0}")]
    Vad(String),
    #[error("model: {0}")]
    Model(String),
}
