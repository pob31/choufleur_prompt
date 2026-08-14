//! Audible monitoring of the audio being analysed.
//!
//! The live spike exists to be *judged*, and the judgement is whether the text
//! arrives with the voice. That cannot be measured from a file — it has to be heard
//! while the screen is watched. So the same blocks that go into the recogniser also
//! go to an output device.
//!
//! # The buffer is deliberately short
//!
//! Whatever sits in this buffer is sound that has been analysed but not yet heard, so
//! the display runs *ahead* of the ear by exactly the buffer depth. A generous buffer
//! would therefore flatter the system: text would appear early and the lag under test
//! would be hidden. A quarter of a second is enough to absorb scheduling jitter and
//! small enough to be irrelevant beside the ~600 ms the pipeline actually takes.
//!
//! # It is also the clock
//!
//! `push` blocks until the device has consumed enough to make room, so the run is
//! paced by the sound card rather than by `Instant::now()`. Sound and analysis then
//! advance on one clock and cannot drift apart. `VirtualClock` still runs alongside
//! for latency accounting; it simply never has to sleep, because this got there first.

use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Shared between the pushing thread and the audio callback.
struct Shared {
    /// Interleaved output frames waiting to be played.
    queue: Mutex<std::collections::VecDeque<f32>>,
    /// Signalled by the callback whenever it has taken samples out.
    drained: Condvar,
    /// Frames the device has actually consumed. A true playback clock.
    played: Mutex<u64>,
    /// Frames the device asked for and did not get. Any of these is a fault: the
    /// silence inserted does not merely click, it pushes every later word further
    /// behind real time, so a steady trickle of them makes the display drift ahead
    /// of the sound without either clock being wrong.
    starved: Mutex<u64>,
}

pub struct Monitor {
    shared: Arc<Shared>,
    /// Output channels; a mono source is duplicated across them.
    channels: usize,
    /// Samples (not frames) allowed to queue before `push` waits.
    capacity: usize,
    /// Source frames per device frame. 1.0 when the rates already agree.
    ratio: f64,
    /// Fractional read position carried between blocks, so resampling does not
    /// click at every block boundary.
    phase: Mutex<f64>,
    /// Last sample of the previous block, to interpolate across the seam.
    tail: Mutex<f32>,
    device_rate: u32,
    _stream: cpal::Stream,
}

impl Monitor {
    /// Open the default output, or a device whose name contains `want`.
    pub fn open(sample_rate: u32, want: Option<&str>, buffer_ms: u32) -> Result<Self> {
        let host = cpal::default_host();
        let device = match want {
            None => host
                .default_output_device()
                .context("no default audio output device")?,
            Some(name) => host
                .output_devices()?
                .find(|d| {
                    d.name()
                        .map(|n| n.to_lowercase().contains(&name.to_lowercase()))
                        .unwrap_or(false)
                })
                .with_context(|| format!("no output device matching {name:?}"))?,
        };

        let default = device.default_output_config()?;
        // Prefer running the device at the corpus rate: then monitoring is a copy and
        // there is nothing to argue about. Macs commonly sit at 44.1 kHz, though, and
        // requiring the operator to go and change a system setting before they can
        // hear anything would be a silly obstacle in front of the one feature this
        // spike exists for — so fall back to the device's own rate and resample.
        let wanted = device
            .supported_output_configs()?
            .find(|r| {
                r.min_sample_rate().0 <= sample_rate
                    && sample_rate <= r.max_sample_rate().0
                    && r.sample_format() == cpal::SampleFormat::F32
            })
            .map(|r| r.with_sample_rate(cpal::SampleRate(sample_rate)));
        let cfg = wanted.unwrap_or(default);
        let channels = cfg.channels() as usize;
        let device_rate = cfg.sample_rate().0;
        // Linear interpolation, deliberately: this path feeds ears, never the
        // recogniser, which still receives the untouched 48 kHz stream.
        let ratio = sample_rate as f64 / device_rate as f64;

        let shared = Arc::new(Shared {
            queue: Mutex::new(std::collections::VecDeque::new()),
            drained: Condvar::new(),
            played: Mutex::new(0),
            starved: Mutex::new(0),
        });
        let cb = Arc::clone(&shared);
        let stream = device.build_output_stream(
            &cfg.config(),
            move |out: &mut [f32], _| {
                let mut q = cb.queue.lock().unwrap();
                let mut taken = 0usize;
                let mut missed = 0usize;
                for slot in out.iter_mut() {
                    match q.pop_front() {
                        Some(s) => {
                            *slot = s;
                            taken += 1;
                        }
                        None => {
                            *slot = 0.0;
                            missed += 1;
                        }
                    }
                }
                drop(q);
                if taken > 0 {
                    *cb.played.lock().unwrap() += (taken / channels.max(1)) as u64;
                }
                if missed > 0 {
                    *cb.starved.lock().unwrap() += (missed / channels.max(1)) as u64;
                }
                cb.drained.notify_all();
            },
            |err| eprintln!("audio output error: {err}"),
            None,
        )?;
        stream.play()?;

        // At least a few blocks deep, whatever was asked for. The producer hands over
        // a quarter-second at a time, so a capacity of one block forces it to wait for
        // the queue to run completely dry before every push — and the sliver between
        // "empty" and "refilled" is emitted as silence. That was audible as a warble
        // on every single block, and it made the sound fall steadily further behind.
        let block = (crate::engine::BLOCK_SECONDS * device_rate as f64) as usize;
        let want = (device_rate as usize * buffer_ms as usize / 1000).max(block * 4);
        let capacity = want * channels;
        println!(
            "monitor: {} @ {} Hz, {} ch, {} ms buffer{}",
            device.name().unwrap_or_else(|_| "?".into()),
            device_rate,
            channels,
            buffer_ms,
            if device_rate == sample_rate {
                String::new()
            } else {
                format!(" (resampling from {sample_rate} Hz for playback only)")
            }
        );
        Ok(Monitor {
            shared,
            channels,
            capacity: capacity.max(channels * 64),
            ratio,
            phase: Mutex::new(0.0),
            tail: Mutex::new(0.0),
            device_rate,
            _stream: stream,
        })
    }

    /// Queue one mono block, waiting until the device has room.
    ///
    /// This wait is what paces the whole run.
    pub fn push(&self, mono: &[f32]) {
        if mono.is_empty() {
            return;
        }
        // Resample first, outside the lock, so the critical section stays short and
        // the audio callback is never kept waiting on arithmetic.
        let out: Vec<f32> = if (self.ratio - 1.0).abs() < 1e-9 {
            mono.to_vec()
        } else {
            let mut phase = self.phase.lock().unwrap();
            let mut tail = self.tail.lock().unwrap();
            let mut v = Vec::with_capacity((mono.len() as f64 / self.ratio) as usize + 2);
            let mut p = *phase;
            while p < mono.len() as f64 {
                let i = p.floor() as isize;
                let frac = (p - p.floor()) as f32;
                let a = if i < 0 { *tail } else { mono[i as usize] };
                let b = mono.get((i + 1).max(0) as usize).copied().unwrap_or(a);
                v.push(a + (b - a) * frac);
                p += self.ratio;
            }
            *phase = p - mono.len() as f64;
            *tail = *mono.last().unwrap();
            v
        };

        let mut q = self.shared.queue.lock().unwrap();
        while q.len() + out.len() * self.channels > self.capacity {
            q = self.shared.drained.wait(q).unwrap();
        }
        for s in &out {
            for _ in 0..self.channels {
                q.push_back(*s);
            }
        }
    }

    /// Seconds of audio the device has actually played.
    pub fn played_seconds(&self) -> f64 {
        *self.shared.played.lock().unwrap() as f64 / self.device_rate as f64
    }

    /// Frames the device asked for and did not get.
    pub fn starved_frames(&self) -> u64 {
        *self.shared.starved.lock().unwrap()
    }

    /// Wait for the queue to empty, so a run does not end mid-word.
    pub fn drain(&self) {
        let mut q = self.shared.queue.lock().unwrap();
        while !q.is_empty() {
            q = self.shared.drained.wait(q).unwrap();
        }
    }
}
