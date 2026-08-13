//! Streaming 48 kHz → 16 kHz downsampling.
//!
//! Both Silero and Whisper want 16 kHz mono; theatre audio arrives at 48 kHz.
//! The ratio is exactly 3:1, so this is a fixed-ratio synchronous resample with a
//! constant group delay — which is the property that matters, because every VAD
//! timestamp is derived from the output and would otherwise be uniformly wrong.
//!
//! rubato requires *exactly* `input_frames_next()` frames per call, so this wraps
//! it with the buffering the caller would otherwise have to write. Blocks in are
//! whatever length the reader produced; frames out are whatever is ready.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use crate::AsrError;

pub const INPUT_RATE: usize = 48_000;
pub const OUTPUT_RATE: usize = 16_000;

/// 30 ms of 48 kHz input per internal block.
///
/// This is a bandwidth-versus-delay trade, and both ends of it are real. rubato's
/// anti-alias cutoff and its delay both scale with the block: 480 frames gives a
/// 7325 Hz cutoff and 80 frames of delay; 1440 gives 7773 Hz and 240; 4800 gives
/// 7931 Hz and 800. Speech energy that matters to Whisper runs to about 8 kHz, so
/// 1440 buys most of the available bandwidth for 15 ms of delay, and 15 ms is far
/// below anything the warning scheduler will notice.
pub const DEFAULT_CHUNK_IN: usize = 1440;

/// Fixed 3:1 downsampler with internal buffering.
pub struct Resampler48to16 {
    inner: Fft<f32>,
    chunk_in: usize,
    chunk_out: usize,
    /// Input frames left over from the previous call, always fewer than a chunk.
    carry: Vec<f32>,
    scratch_out: Vec<f32>,
}

impl Resampler48to16 {
    pub fn new() -> Result<Self, AsrError> {
        Self::with_chunk(DEFAULT_CHUNK_IN)
    }

    pub fn with_chunk(chunk_in_hint: usize) -> Result<Self, AsrError> {
        let inner = Fft::<f32>::new(INPUT_RATE, OUTPUT_RATE, chunk_in_hint, 1, FixedSync::Both)
            .map_err(|e| AsrError::Resample(format!("constructing resampler: {e}")))?;
        let chunk_in = inner.input_frames_next();
        let chunk_out = inner.output_frames_next();
        debug_assert_eq!(chunk_in, 3 * chunk_out, "48k->16k must be exactly 3:1");
        Ok(Resampler48to16 {
            inner,
            chunk_in,
            chunk_out,
            carry: Vec::with_capacity(chunk_in),
            scratch_out: vec![0.0; chunk_out],
        })
    }

    /// Constant group delay in **output** frames.
    ///
    /// Output frame `i` carries what happened at input frame `3 * (i - delay)`.
    /// Subtract this once when mapping output samples back to the recording's
    /// timeline, or every segment boundary is late by a fixed amount.
    pub fn output_delay_frames(&self) -> usize {
        self.inner.output_delay()
    }

    pub fn output_delay_seconds(&self) -> f64 {
        self.output_delay_frames() as f64 / OUTPUT_RATE as f64
    }

    /// Feed any number of 48 kHz frames; append every 16 kHz frame now ready.
    pub fn process(&mut self, input_48k: &[f32], out_16k: &mut Vec<f32>) -> Result<(), AsrError> {
        let mut pos = 0usize;

        // Finish the partial chunk left from last time, if the new block completes it.
        if !self.carry.is_empty() {
            let need = self.chunk_in - self.carry.len();
            if input_48k.len() < need {
                self.carry.extend_from_slice(input_48k);
                return Ok(());
            }
            let mut full = std::mem::take(&mut self.carry);
            full.extend_from_slice(&input_48k[..need]);
            pos = need;
            self.run_chunk(&full, 0, out_16k)?;
            full.clear();
            self.carry = full; // keep the allocation
        }

        // Whole chunks straight out of the caller's slice, no copy.
        while pos + self.chunk_in <= input_48k.len() {
            self.run_chunk(input_48k, pos, out_16k)?;
            pos += self.chunk_in;
        }

        self.carry.extend_from_slice(&input_48k[pos..]);
        Ok(())
    }

    /// End of stream: zero-pad the remainder and emit it.
    ///
    /// The tail is not trimmed back to the exact theoretical length — the last
    /// few milliseconds of a recording are silence in every corpus we care about,
    /// and a segmenter that has already closed will not notice them.
    pub fn flush(&mut self, out_16k: &mut Vec<f32>) -> Result<(), AsrError> {
        if self.carry.is_empty() {
            return Ok(());
        }
        let mut buf = std::mem::take(&mut self.carry);
        buf.resize(self.chunk_in, 0.0);
        self.run_chunk(&buf, 0, out_16k)?;
        buf.clear();
        self.carry = buf;
        Ok(())
    }

    fn run_chunk(
        &mut self,
        src: &[f32],
        offset: usize,
        out: &mut Vec<f32>,
    ) -> Result<(), AsrError> {
        let input = InterleavedSlice::new(&src[offset..offset + self.chunk_in], 1, self.chunk_in)
            .map_err(|e| AsrError::Resample(format!("wrapping input: {e}")))?;
        let mut output = InterleavedSlice::new_mut(&mut self.scratch_out, 1, self.chunk_out)
            .map_err(|e| AsrError::Resample(format!("wrapping output: {e}")))?;
        let (_consumed, produced) = self
            .inner
            .process_into_buffer(&input, &mut output, None)
            .map_err(|e| AsrError::Resample(format!("resampling: {e}")))?;
        out.extend_from_slice(&self.scratch_out[..produced]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn sine(freq: f32, frames: usize, rate: f32) -> Vec<f32> {
        (0..frames)
            .map(|i| (i as f32 * TAU * freq / rate).sin() * 0.5)
            .collect()
    }

    #[test]
    fn output_is_a_third_of_the_input_length() {
        let mut r = Resampler48to16::new().unwrap();
        let input = sine(440.0, 48_000, 48_000.0);
        let mut out = Vec::new();
        r.process(&input, &mut out).unwrap();
        r.flush(&mut out).unwrap();
        // One second in, one second out, within a chunk's worth of slack.
        assert!(
            (out.len() as i64 - 16_000).abs() < r.chunk_out as i64 * 2,
            "1 s of 48 kHz produced {} frames at 16 kHz",
            out.len()
        );
    }

    #[test]
    fn arbitrary_block_sizes_are_buffered_not_rejected() {
        // The reader hands us whatever the WAV gives; rubato demands exact chunks.
        let input = sine(440.0, 48_000, 48_000.0);
        let mut whole = Vec::new();
        Resampler48to16::new()
            .unwrap()
            .process(&input, &mut whole)
            .unwrap();

        for block in [1usize, 7, 100, 999, 4096] {
            let mut r = Resampler48to16::new().unwrap();
            let mut out = Vec::new();
            for chunk in input.chunks(block) {
                r.process(chunk, &mut out).unwrap();
            }
            assert_eq!(
                out.len(),
                whole.len(),
                "block size {block} changed the output length"
            );
            for (i, (a, b)) in whole.iter().zip(&out).enumerate() {
                assert!(
                    (a - b).abs() < 1e-6,
                    "block {block} differs at frame {i}: {a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn a_speech_band_tone_survives_at_full_amplitude() {
        // 1 kHz is squarely in the passband; it must come through unattenuated or
        // every downstream level threshold is measuring the resampler.
        let mut r = Resampler48to16::new().unwrap();
        let mut out = Vec::new();
        r.process(&sine(1000.0, 48_000, 48_000.0), &mut out)
            .unwrap();
        let delay = r.output_delay_frames();
        let steady = &out[delay + 1000..out.len() - 1000];
        let peak = steady.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (peak - 0.5).abs() < 0.02,
            "1 kHz peak came out at {peak}, expected 0.5"
        );
    }

    #[test]
    fn content_above_the_new_nyquist_is_rejected_rather_than_aliased() {
        // 11 kHz cannot exist at 16 kHz; if it aliased it would appear as 5 kHz
        // and the VAD would see speech energy that was never there.
        let mut r = Resampler48to16::new().unwrap();
        let mut out = Vec::new();
        r.process(&sine(11_000.0, 48_000, 48_000.0), &mut out)
            .unwrap();
        let steady = &out[r.output_delay_frames() + 1000..out.len() - 1000];
        let peak = steady.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 0.01, "11 kHz leaked through at {peak}");
    }

    #[test]
    fn the_group_delay_is_constant_and_known() {
        let r = Resampler48to16::new().unwrap();
        let delay = r.output_delay_frames();
        assert!(delay > 0, "a windowed resampler always has delay");
        // 1440 in / 480 out per block => 240 frames => 15 ms.
        assert_eq!(delay, r.chunk_in / 6);
        assert!(
            (r.output_delay_seconds() - 0.015).abs() < 1e-9,
            "{}",
            r.output_delay_seconds()
        );
    }

    #[test]
    fn silence_in_silence_out() {
        let mut r = Resampler48to16::new().unwrap();
        let mut out = Vec::new();
        r.process(&vec![0.0; 48_000], &mut out).unwrap();
        assert!(
            out.iter().all(|s| s.abs() < 1e-9),
            "silence acquired energy"
        );
    }
}
