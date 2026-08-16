//! Listening to the room.
//!
//! The mirror of [`crate::monitor`], and deliberately built the same way: the audio
//! callback owns nothing but a queue, and everything that thinks runs on the far side of
//! it. `serve.rs`'s threading note is unambiguous about why — *"the audio callback is
//! real-time and owns nothing but a queue"* — and `play_file` records what happened when
//! that was ignored: every Whisper decode stalled the feed and the run fell behind 1141
//! times in five minutes.
//!
//! One device, N logical channels. The device hands the callback every input interleaved;
//! the patch says which input feeds which channel, and the callback de-interleaves into
//! one ring buffer each. That is **selection**, not the mono downmix `WavBlockReader`
//! performs on a file — reading a 128-channel Dante the way a stereo WAV is read would
//! give one mush.
//!
//! ## The stream cannot move
//!
//! `cpal::Stream` is `!Send` on macOS, so it has to live on the thread that made it. The
//! ring buffers do not: they are `Arc`s the engine, the meters and the HTTP side all
//! share. So a capture thread opens the stream, hands the buffers back, and parks —
//! holding the stream alive and doing nothing else.
//!
//! ## Rate
//!
//! Opened at 48 kHz or not at all. `Resampler48to16` is fixed at 48 kHz and
//! `ChannelFrontend` takes no sample rate, so a device running at 44.1 would produce
//! timestamps 8.8 % fast with nothing anywhere to notice — the failure `channel.rs`
//! warns of, where *"every segment boundary is uniformly late, which looks exactly like
//! the tracker being slow"*. Refusing names the rate it offered instead.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio::Venue;

/// What the recogniser needs and what a capture device runs at.
pub const RATE: u32 = 48_000;

/// How much audio a channel may hold before the callback starts dropping.
///
/// Two seconds. Long enough that a Whisper decode running long does not cost frames,
/// short enough that a run which has genuinely fallen behind says so rather than
/// accumulating a delay nobody can see. `Monitor` keeps its output buffer deliberately
/// short for the same reason and explains it at length.
const BUFFER_S: f64 = 2.0;

/// One logical channel's audio, and how it is doing.
pub struct ChannelBuf {
    pub logical: u16,
    /// The device input feeding it, one-based, as the desk counts them.
    pub input: u16,
    queue: Mutex<VecDeque<f32>>,
    filled: Condvar,
    /// Frames lost *while something was reading them*.
    ///
    /// The one thing that makes a sample-counted timeline wrong, so it is counted rather
    /// than swallowed: `Segmenter` derives every timestamp from how many samples it has
    /// seen, and a lost frame moves everything after it earlier for ever.
    ///
    /// Only while something is reading. A meter-only run consumes nothing, so its buffer
    /// fills within two seconds and then overwrites for ever — which is correct and not
    /// a fault. Counting it reported 870,144 dropped frames on a twenty-second look at a
    /// silent input, which reads as catastrophe and means nothing at all.
    dropped: AtomicU64,
    /// Set the first time anybody reads. Until then there is no timeline to break.
    consumed: AtomicBool,
    level: Mutex<Level>,
}

/// A window of loudness, reset each time it is read.
#[derive(Clone, Copy, Debug, Default)]
struct Level {
    peak: f32,
    sum_sq: f64,
    frames: u64,
    clipped: u64,
}

/// What a meter shows.
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub channel: u16,
    pub input: u16,
    /// Loudest sample in the window, dBFS. `-inf` becomes -120.
    pub peak_dbfs: f32,
    pub rms_dbfs: f32,
    pub clipped: u64,
    pub dropped: u64,
    /// Samples the callback delivered in this window.
    ///
    /// Zero and silent are different faults and look the same on a meter. Silent is a
    /// device running and carrying nothing — the ordinary afternoon. Zero frames is the
    /// callback not firing at all: the device open but delivering nothing, which on
    /// macOS is what an unbundled binary without microphone permission looks like.
    pub frames: u64,
    /// `ok`, `silent`, `clipped`, or `no audio`. The PRD's `input_health` states.
    pub state: &'static str,
}

/// Below this a channel is reporting nothing, not merely something quiet.
///
/// A patched input carrying no signal and a patched input carrying the wrong signal look
/// identical on paper; the difference is a meter, and this is where it draws the line.
/// −70 dBFS is beneath any preamp's noise floor and above digital silence, so an
/// unplugged input reads silent while a live one with nobody speaking does not.
const SILENT_DBFS: f32 = -70.0;

impl ChannelBuf {
    fn new(logical: u16, input: u16) -> Self {
        ChannelBuf {
            logical,
            input,
            queue: Mutex::new(VecDeque::with_capacity((RATE as f64 * BUFFER_S) as usize)),
            filled: Condvar::new(),
            dropped: AtomicU64::new(0),
            consumed: AtomicBool::new(false),
            level: Mutex::new(Level::default()),
        }
    }

    /// Take up to `frames`, waiting briefly for them rather than returning short.
    ///
    /// A live source that has nothing yet is not at end of stream — it is a room where
    /// nobody has said anything for a moment. Returning 0 has to mean "nothing arrived in
    /// this window", and the caller decides what that is worth.
    pub fn read(&self, out: &mut Vec<f32>, frames: usize, wait: Duration) -> usize {
        self.consumed.store(true, Ordering::Relaxed);
        let mut q = self.queue.lock().unwrap();
        if q.len() < frames {
            let (g, _) = self.filled.wait_timeout(q, wait).unwrap();
            q = g;
        }
        let take = frames.min(q.len());
        out.clear();
        out.extend(q.drain(..take));
        take
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Read the meter and start a new window.
    pub fn health(&self) -> Health {
        let mut l = self.level.lock().unwrap();
        let taken = *l;
        *l = Level::default();
        drop(l);

        let db = |v: f32| if v <= 1e-7 { -120.0 } else { 20.0 * v.log10() };
        let peak = db(taken.peak);
        let rms = if taken.frames == 0 {
            -120.0
        } else {
            db((taken.sum_sq / taken.frames as f64).sqrt() as f32)
        };
        Health {
            channel: self.logical,
            input: self.input,
            peak_dbfs: peak,
            rms_dbfs: rms,
            clipped: taken.clipped,
            dropped: self.dropped(),
            frames: taken.frames,
            state: if taken.frames == 0 {
                "no audio"
            } else if taken.clipped > 0 {
                "clipped"
            } else if peak < SILENT_DBFS {
                "silent"
            } else {
                "ok"
            },
        }
    }
}

/// A running input stream and the buffers it fills.
pub struct Capture {
    pub device: String,
    pub channels: Vec<Arc<ChannelBuf>>,
    stop: Arc<AtomicBool>,
}

impl Capture {
    /// Open the venue's device and start filling one buffer per patched channel.
    ///
    /// Returns once the stream is running. The stream itself stays on a thread of its
    /// own, because `cpal::Stream` cannot be moved off the thread that built it.
    pub fn open(venue: &Venue) -> Result<Capture> {
        if venue.channels.is_empty() {
            bail!("nothing is patched — set an input against a channel in the audio panel");
        }
        let bufs: Vec<Arc<ChannelBuf>> = venue
            .channels
            .iter()
            .map(|p| Arc::new(ChannelBuf::new(p.logical, p.input)))
            .collect();

        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Result<String>>();
        let want = venue.device.clone();
        let for_thread: Vec<Arc<ChannelBuf>> = bufs.clone();
        let stop_thread = Arc::clone(&stop);

        std::thread::Builder::new()
            .name("choufleur-capture".into())
            .spawn(move || match build(&want, &for_thread) {
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
                Ok((stream, name)) => {
                    let _ = tx.send(Ok(name));
                    // Nothing to do but hold the stream. Dropping it stops the device.
                    while !stop_thread.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    drop(stream);
                }
            })
            .context("starting the capture thread")?;

        let device = rx
            .recv()
            .context("the capture thread stopped before it opened anything")??;
        Ok(Capture {
            device,
            channels: bufs,
            stop,
        })
    }

    pub fn health(&self) -> Vec<Health> {
        self.channels.iter().map(|c| c.health()).collect()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Open the device and start the stream. Runs on the capture thread.
fn build(want: &str, bufs: &[Arc<ChannelBuf>]) -> Result<(cpal::Stream, String)> {
    let host = cpal::default_host();
    // Matched loosely by name, as the monitor already does, because a device's name
    // gains and loses suffixes between OS versions.
    let device = if want.trim().is_empty() {
        host.default_input_device()
            .context("no default audio input device")?
    } else {
        host.input_devices()?
            .find(|d| {
                d.name()
                    .map(|n| n.to_lowercase().contains(&want.to_lowercase()))
                    .unwrap_or(false)
            })
            .with_context(|| format!("no input device matching {want:?}"))?
    };
    let name = device.name().unwrap_or_else(|_| "?".into());

    let cfg = device
        .supported_input_configs()?
        .find(|r| {
            r.min_sample_rate().0 <= RATE
                && RATE <= r.max_sample_rate().0
                && r.sample_format() == cpal::SampleFormat::F32
        })
        .map(|r| r.with_sample_rate(cpal::SampleRate(RATE)))
        .with_context(|| {
            let offered = device
                .default_input_config()
                .map(|c| format!("{} Hz", c.sample_rate().0))
                .unwrap_or_else(|_| "an unknown rate".into());
            format!(
                "{name} will not run at {RATE} Hz — it offers {offered}. \
                 Recognition is built for 48 kHz, and running at another rate would \
                 shift every timestamp silently rather than fail."
            )
        })?;

    let device_channels = cfg.channels() as usize;
    // A patch pointing past the end of the device is worth saying now rather than
    // discovering as a channel that never carries anything.
    for b in bufs {
        if b.input as usize > device_channels {
            bail!(
                "channel {} is patched to input {}, but {name} has {device_channels}",
                b.logical,
                b.input
            );
        }
    }

    // Which device input feeds which buffer, resolved once so the callback is a copy.
    // One input may feed several channels — the shared position mic — so this is a list
    // per buffer rather than a map from input to buffer.
    let taps: Vec<(usize, Arc<ChannelBuf>)> = bufs
        .iter()
        .map(|b| ((b.input as usize).saturating_sub(1), Arc::clone(b)))
        .collect();
    let cap = (RATE as f64 * BUFFER_S) as usize;

    let stream = device.build_input_stream(
        &cfg.config(),
        move |data: &[f32], _| {
            for (offset, buf) in &taps {
                let consumed = buf.consumed.load(Ordering::Relaxed);
                let mut q = buf.queue.lock().unwrap();
                let mut level = buf.level.lock().unwrap();
                let mut dropped = 0u64;
                let mut i = *offset;
                while i < data.len() {
                    let s = data[i];
                    i += device_channels;
                    level.frames += 1;
                    let a = s.abs();
                    if a > level.peak {
                        level.peak = a;
                    }
                    level.sum_sq += (s as f64) * (s as f64);
                    // Full scale, or near enough that a converter is already squaring it.
                    if a >= 0.999 {
                        level.clipped += 1;
                    }
                    if q.len() >= cap {
                        // Keep the newest. A meter wants the last two seconds, and a
                        // reader that has fallen this far behind is not going to catch
                        // up by being handed stale audio first.
                        q.pop_front();
                        if consumed {
                            dropped += 1;
                        }
                    }
                    q.push_back(s);
                }
                drop(level);
                drop(q);
                if dropped > 0 {
                    buf.dropped.fetch_add(dropped, Ordering::Relaxed);
                }
                buf.filled.notify_all();
            }
        },
        |err| eprintln!("audio input error: {err}"),
        None,
    )?;
    stream.play()?;
    println!("capture: {name} @ {RATE} Hz, {device_channels} ch");
    Ok((stream, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf() -> ChannelBuf {
        ChannelBuf::new(1, 17)
    }

    #[test]
    fn a_device_delivering_nothing_is_not_the_same_as_a_quiet_one() {
        // No callback at all — the device open and handing over nothing. On macOS this
        // is what a binary without microphone permission looks like, and calling it
        // "silent" would send somebody to check their patch instead of their
        // permissions.
        let b = buf();
        assert_eq!(b.health().state, "no audio");
        {
            let mut l = b.level.lock().unwrap();
            l.frames = 4800;          // a tenth of a second of digital silence
        }
        assert_eq!(b.health().state, "silent");
    }

    #[test]
    fn an_empty_channel_reads_silent_not_broken() {
        let b = buf();
        b.level.lock().unwrap().frames = 4800;
        let h = b.health();
        assert_eq!(h.state, "silent");
        assert_eq!(h.channel, 1);
        assert_eq!(h.input, 17);
        // A patched input with nothing on it is the ordinary state of a rig in the
        // afternoon. It must be legible, not an error.
        assert!(h.peak_dbfs <= SILENT_DBFS);
    }

    #[test]
    fn a_signal_reads_ok_and_the_window_resets() {
        let b = buf();
        {
            let mut l = b.level.lock().unwrap();
            l.peak = 0.5;
            l.sum_sq = 0.25 * 100.0;
            l.frames = 100;
        }
        let h = b.health();
        assert_eq!(h.state, "ok");
        assert!((h.peak_dbfs - -6.02).abs() < 0.05, "{}", h.peak_dbfs);
        // Read once, then it starts again — otherwise a peak from a minute ago holds
        // the meter up for ever. And an empty window is "no audio" rather than "silent",
        // because at 48 kHz a tenth of a second with no callback is a fault, not a pause.
        assert_eq!(b.health().state, "no audio");
    }

    #[test]
    fn full_scale_reads_clipped() {
        let b = buf();
        {
            let mut l = b.level.lock().unwrap();
            l.peak = 1.0;
            l.frames = 10;
            l.clipped = 3;
        }
        let h = b.health();
        assert_eq!(h.state, "clipped");
        assert_eq!(h.clipped, 3);
    }

    #[test]
    fn a_meter_only_run_does_not_report_the_buffer_filling_as_loss() {
        // Twenty seconds of looking at an input nobody is transcribing fills the buffer
        // in two and overwrites for the other eighteen. That is the design, not a fault,
        // and reporting 870,144 dropped frames for it is how a real drop stops being
        // believed.
        let b = buf();
        assert!(!b.consumed.load(Ordering::Relaxed));
        assert_eq!(b.dropped(), 0);
    }

    #[test]
    fn reading_marks_the_channel_as_consumed() {
        let b = buf();
        let mut out = Vec::new();
        b.read(&mut out, 1, Duration::from_millis(1));
        assert!(b.consumed.load(Ordering::Relaxed), "now a lost frame matters");
    }

    #[test]
    fn reading_takes_what_is_there_and_no_more() {
        let b = buf();
        b.queue.lock().unwrap().extend([0.1, 0.2, 0.3]);
        let mut out = Vec::new();
        assert_eq!(b.read(&mut out, 2, Duration::from_millis(1)), 2);
        assert_eq!(out, [0.1, 0.2]);
        // Short rather than blocking for ever: a quiet room is not a closed stream.
        assert_eq!(b.read(&mut out, 5, Duration::from_millis(1)), 1);
        assert_eq!(out, [0.3]);
        assert_eq!(b.read(&mut out, 1, Duration::from_millis(1)), 0);
    }

    #[test]
    fn an_unpatched_venue_is_refused_with_a_reason() {
        let Err(e) = Capture::open(&Venue::default()) else {
            panic!("an empty patch must be refused, not opened");
        };
        assert!(e.to_string().contains("nothing is patched"), "{e}");
    }
}
