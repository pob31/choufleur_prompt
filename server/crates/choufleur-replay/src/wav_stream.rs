//! Streaming WAV reader and writer.
//!
//! Reading is block-at-a-time on purpose. A full act at 16 channels is roughly
//! 11 GB as f32; the replay engine holds one short block per channel and nothing
//! else, so memory is flat in the length of the recording.

use std::path::Path;

use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};

/// A WAV file presented as a mono `f32` stream, whatever it is on disk.
///
/// Console and DAW exports arrive as 16-bit, 24-bit or float, occasionally as
/// WAVE_FORMAT_EXTENSIBLE, and occasionally stereo when a mono channel was meant.
/// All of that is normalized here so nothing downstream has to care.
pub struct WavBlockReader {
    inner: Box<dyn Iterator<Item = Result<f32, hound::Error>>>,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    frames_read: u64,
}

impl WavBlockReader {
    pub fn open(path: &Path) -> Result<Self> {
        let reader =
            WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
        let spec = reader.spec();
        let frames = reader.len() as u64 / spec.channels.max(1) as u64;
        // Normalize to [-1, 1) by the format's full scale, matching what whisper.cpp
        // and Silero expect.
        let scale = |bits: u16| (1i64 << (bits - 1)) as f32;
        let inner: Box<dyn Iterator<Item = Result<f32, hound::Error>>> =
            match (spec.sample_format, spec.bits_per_sample) {
                (SampleFormat::Float, 32) => Box::new(reader.into_samples::<f32>()),
                (SampleFormat::Int, bits @ (8 | 16 | 24 | 32)) => {
                    let s = scale(bits);
                    Box::new(
                        reader
                            .into_samples::<i32>()
                            .map(move |v| v.map(|v| v as f32 / s)),
                    )
                }
                (fmt, bits) => bail!(
                    "{}: unsupported WAV format ({fmt:?}, {bits} bit). Convert with: \
                     afconvert -f WAVE -d LEI16@48000 -c 1 <in> <out>",
                    path.display()
                ),
            };
        Ok(WavBlockReader {
            inner,
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            frames,
            frames_read: 0,
        })
    }

    pub fn duration_seconds(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }

    /// Read up to `frames` mono frames into `out` (cleared first). Returns the
    /// number of frames read; 0 means end of file. Multi-channel input is averaged
    /// down, which is what a stereo-by-accident mono channel wants.
    pub fn read_block(&mut self, out: &mut Vec<f32>, frames: usize) -> Result<usize> {
        out.clear();
        let nch = self.channels.max(1) as usize;
        for _ in 0..frames {
            let mut acc = 0.0f32;
            let mut got = 0usize;
            for _ in 0..nch {
                match self.inner.next() {
                    Some(v) => {
                        acc += v?;
                        got += 1;
                    }
                    None => break,
                }
            }
            if got == 0 {
                break;
            }
            out.push(acc / got as f32);
        }
        self.frames_read += out.len() as u64;
        Ok(out.len())
    }

    /// Seconds of audio consumed so far — the caller's clock on the audio timeline.
    pub fn position_seconds(&self) -> f64 {
        self.frames_read as f64 / self.sample_rate as f64
    }
}

/// Mono 16-bit WAV writer at a fixed rate, used by `make-fixture`.
pub struct MonoWavWriter {
    writer: WavWriter<std::io::BufWriter<std::fs::File>>,
    written: u64,
}

impl MonoWavWriter {
    pub fn create(path: &Path, sample_rate: u32) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let spec = WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let writer = WavWriter::create(path, spec)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(MonoWavWriter { writer, written: 0 })
    }

    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &s in samples {
            let clamped = s.clamp(-1.0, 1.0);
            self.writer
                .write_sample((clamped * i16::MAX as f32) as i16)?;
        }
        self.written += samples.len() as u64;
        Ok(())
    }

    pub fn write_silence(&mut self, frames: u64) -> Result<()> {
        for _ in 0..frames {
            self.writer.write_sample(0i16)?;
        }
        self.written += frames;
        Ok(())
    }

    pub fn frames_written(&self) -> u64 {
        self.written
    }

    pub fn finalize(self) -> Result<()> {
        self.writer.finalize()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("choufleur-wav-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn writes_and_reads_back_a_mono_file() {
        let path = tmp("tone.wav");
        let mut w = MonoWavWriter::create(&path, 48_000).unwrap();
        let tone: Vec<f32> = (0..4800)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 0.5)
            .collect();
        w.write_silence(2400).unwrap();
        w.write(&tone).unwrap();
        assert_eq!(w.frames_written(), 7200);
        w.finalize().unwrap();

        let mut r = WavBlockReader::open(&path).unwrap();
        assert_eq!(r.sample_rate, 48_000);
        assert_eq!(r.channels, 1);
        assert_eq!(r.frames, 7200);
        assert!((r.duration_seconds() - 0.15).abs() < 1e-9);

        let mut buf = Vec::new();
        let mut total = 0usize;
        let mut peak = 0.0f32;
        loop {
            let n = r.read_block(&mut buf, 1024).unwrap();
            if n == 0 {
                break;
            }
            total += n;
            peak = peak.max(buf.iter().fold(0.0f32, |a, b| a.max(b.abs())));
        }
        assert_eq!(total, 7200);
        // 16-bit round trip of a 0.5 peak sine.
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
        assert!((r.position_seconds() - 0.15).abs() < 1e-9);
    }

    #[test]
    fn clipping_is_clamped_not_wrapped() {
        let path = tmp("hot.wav");
        let mut w = MonoWavWriter::create(&path, 48_000).unwrap();
        w.write(&[2.0, -2.0, 0.0]).unwrap();
        w.finalize().unwrap();

        let mut r = WavBlockReader::open(&path).unwrap();
        let mut buf = Vec::new();
        r.read_block(&mut buf, 8).unwrap();
        assert!(buf[0] > 0.9 && buf[1] < -0.9, "{buf:?}");
    }
}
