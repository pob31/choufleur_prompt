//! Watch the inputs, from a terminal.
//!
//! The same capture the panel uses, printed. It exists because a permission prompt is a
//! window, and a window needs somebody looking at it — a server started in the
//! background cannot show you one, and on macOS a process that has never been granted
//! the microphone gets a stream that runs and delivers nothing at all rather than an
//! error. That failure is indistinguishable from a dead patch unless you can see the
//! frame count, so this prints it.
//!
//! It is also the check worth running before a show: point it at the patch and watch
//! whether the meters move when somebody speaks.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::audio;
use crate::capture::Capture;

pub fn run(library: &std::path::Path, seconds: f64, scan: bool) -> Result<()> {
    let patched = audio::load(library);
    // Scanning ignores the patch and listens to everything the device has. Which input
    // a signal arrives on is otherwise found one channel at a time, and a Digiface has
    // a hundred and twenty-eight of them.
    let venue = if scan {
        audio::every_input(&patched.device)?
    } else {
        patched
    };
    println!("library: {}", library.display());
    println!(
        "patch:   {} channel(s) on {}\n",
        venue.channels.len(),
        if venue.device.is_empty() {
            "the default input"
        } else {
            &venue.device
        }
    );

    let cap = Capture::open(&venue)?;
    let started = Instant::now();
    let mut ever = 0u64;
    // The loudest each input has been, across the whole look. A signal that comes and
    // goes would otherwise only be caught by whichever half-second happened to be
    // printed.
    let mut loudest: std::collections::BTreeMap<u16, f32> = Default::default();

    while started.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(500));
        let health = cap.health();
        for h in &health {
            ever += h.frames;
            let e = loudest.entry(h.input).or_insert(f32::NEG_INFINITY);
            *e = e.max(h.peak_dbfs);
        }
        if scan {
            let live: Vec<String> = health
                .iter()
                .filter(|h| h.state == "ok" || h.state == "clipped")
                .map(|h| format!("in {} {:.1} dBFS", h.input, h.peak_dbfs))
                .collect();
            println!(
                "  {}",
                if live.is_empty() {
                    format!("nothing on any of {} inputs", health.len())
                } else {
                    live.join("   ")
                }
            );
        } else {
            let line: Vec<String> = health
                .iter()
                .map(|h| {
                    format!(
                        "ch{:<3} in {:<4} {:<9} {:>7.1} dBFS{}",
                        h.channel,
                        h.input,
                        h.state,
                        h.peak_dbfs,
                        if h.dropped > 0 {
                            format!("  {} dropped", h.dropped)
                        } else {
                            String::new()
                        }
                    )
                })
                .collect();
            println!("  {}", line.join("   "));
        }
    }

    if scan {
        let mut hot: Vec<(&u16, &f32)> =
            loudest.iter().filter(|(_, db)| **db > -70.0).collect();
        hot.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
        println!();
        if hot.is_empty() {
            println!("None of the {} inputs carried anything.", loudest.len());
        } else {
            println!("Inputs that carried signal, loudest first:");
            for (input, db) in hot {
                println!("  input {input:<4} {db:>7.1} dBFS");
            }
        }
    }

    // The whole reason this command exists. A patch that carries nothing and a process
    // that was never allowed to listen look the same on a meter; the frame count is what
    // separates them, so it is stated rather than left to be inferred.
    println!();
    if ever == 0 {
        println!(
            "No audio arrived at all — not silence, nothing.\n\n\
             The device opened and reported no error, so this is almost certainly macOS\n\
             microphone permission. A plain binary never asks for it, so nothing appears\n\
             under System Settings > Privacy & Security > Microphone to allow. Running\n\
             from a signed .app bundle carrying NSMicrophoneUsageDescription is what\n\
             makes the prompt appear."
        );
    } else {
        println!("{ever} frames arrived — capture works on this machine.");
    }
    Ok(())
}
