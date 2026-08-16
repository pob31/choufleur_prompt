//! What the computer can hear, and which of it belongs to whom.
//!
//! Two halves that must not be confused, and the devplan's pre-coding decision 4 draws
//! the line: **the show says which logical channel carries a character; the venue says
//! which physical input feeds that logical channel.** Shows travel between houses;
//! venues do not. Without the indirection, taking a production to the next theatre means
//! editing the script, and editing a script to change a patch is how a script gets
//! broken by somebody who only meant to move a microphone.
//!
//! The physical side is deliberately unconstrained. A desk sends the cast on inputs 17
//! to 24 as often as on 1 to 8 — a Dante card's first sixteen are usually the band, or
//! the console's direct outs start wherever the engineer had room. Numbering that begins
//! at one and counts up is a convenience for the person who wrote the software, not a
//! fact about any theatre.
//!
//! **Nothing here captures audio yet.** Enumerating the inputs is real — cpal will list
//! them today — and so is saving the patch. Feeding those inputs into `ChannelFrontend`
//! is M2.1 and does not exist. That is worth having anyway: the patch is what somebody
//! sets up in the afternoon, and it can be got right long before there is anything to
//! route through it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};

/// An input the machine is offering.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    pub name: String,
    /// How many channels it presents at its default configuration.
    pub channels: u16,
    pub sample_rate: u32,
    pub default: bool,
}

/// One logical channel, and where its audio comes from in this room.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Patch {
    /// Matches `Character::channels` and the manifest's `ChannelSpec::index`.
    pub logical: u16,
    /// The input number on the device, exactly as the desk counts them. One-based
    /// because that is what is printed on the hardware, and free to start anywhere.
    pub input: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// The room, as opposed to the show.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Venue {
    /// The input device, by name. Matched loosely at open time, as the monitor already
    /// does, because a device's name gains and loses suffixes between OS versions.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device: String,
    #[serde(default)]
    pub channels: Vec<Patch>,
}

/// Every input device, the default one first.
///
/// Failures are swallowed per device rather than aborting the list: an interface that is
/// half asleep, or one whose driver refuses a config query, should not stop the picker
/// showing the other seven.
pub fn inputs() -> Vec<Input> {
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut out: Vec<Input> = devices
        .filter_map(|d| {
            let name = d.name().ok()?;
            let cfg = d.default_input_config().ok()?;
            Some(Input {
                default: name == default,
                channels: cfg.channels(),
                sample_rate: cfg.sample_rate().0,
                name,
            })
        })
        .collect();
    out.sort_by_key(|i| (!i.default, i.name.to_lowercase()));
    out
}

/// A patch over every input a device has, for looking rather than for running.
///
/// Finding which input a signal actually arrives on is otherwise a guessing game played
/// one channel at a time — and on a 128-channel card that is not a game anybody wins.
pub fn every_input(device: &str) -> Result<Venue> {
    let found = inputs();
    let dev = found
        .iter()
        .find(|i| {
            !device.trim().is_empty() && i.name.to_lowercase().contains(&device.to_lowercase())
        })
        .or_else(|| found.iter().find(|i| i.default))
        .or_else(|| found.first())
        .context("no input devices at all")?;
    Ok(Venue {
        device: dev.name.clone(),
        channels: (1..=dev.channels)
            .map(|n| Patch {
                logical: n,
                input: n,
                note: String::new(),
            })
            .collect(),
    })
}

/// Where the patch lives: beside the library, not inside a show.
///
/// A show carried to another theatre must not bring this with it, which is the whole
/// reason the two are separate files.
pub fn venue_path(library: &Path) -> PathBuf {
    library.join("venue.json")
}

pub fn load(library: &Path) -> Venue {
    std::fs::read(venue_path(library))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(library: &Path, venue: &Venue) -> Result<()> {
    std::fs::create_dir_all(library)?;
    let path = venue_path(library);
    // Written whole and renamed, like everything else that matters: a patch half-read
    // at the top of a show is worse than no patch.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(venue)? + "\n")
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Clean a patch that arrived from a screen.
///
/// Two logical channels fed by one input is legitimate — a shared position mic reaches
/// two performers' channels — but one logical channel fed by two inputs is not a thing
/// audio can do, so the later entry wins and the earlier is dropped.
pub fn tidy(mut channels: Vec<Patch>) -> Vec<Patch> {
    channels.retain(|c| c.logical > 0 && c.input > 0);
    let mut seen = std::collections::HashSet::new();
    channels.reverse();
    channels.retain(|c| seen.insert(c.logical));
    channels.reverse();
    channels.sort_by_key(|c| c.logical);
    channels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(logical: u16, input: u16) -> Patch {
        Patch {
            logical,
            input,
            note: String::new(),
        }
    }

    #[test]
    fn inputs_can_start_anywhere() {
        // A desk sending the cast on 17–20 is as ordinary as one sending them on 1–4.
        let out = tidy(vec![p(1, 17), p(2, 18), p(3, 19), p(4, 20)]);
        assert_eq!(
            out.iter().map(|c| (c.logical, c.input)).collect::<Vec<_>>(),
            [(1, 17), (2, 18), (3, 19), (4, 20)]
        );
    }

    #[test]
    fn one_input_may_feed_several_channels() {
        // The shared SM58: Lies and Veronica are separate logical channels, one mic.
        let out = tidy(vec![p(1, 12), p(2, 12)]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_channel_fed_twice_keeps_the_later_answer() {
        let out = tidy(vec![p(1, 5), p(1, 9)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].input, 9);
    }

    #[test]
    fn nothing_is_channel_zero() {
        assert!(tidy(vec![p(0, 3), p(3, 0)]).is_empty());
    }

    #[test]
    fn a_venue_round_trips_and_a_missing_one_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load(tmp.path()).channels.is_empty());
        let v = Venue {
            device: "Dante Virtual Soundcard".into(),
            channels: tidy(vec![p(1, 17), p(2, 18)]),
        };
        save(tmp.path(), &v).unwrap();
        let back = load(tmp.path());
        assert_eq!(back.device, "Dante Virtual Soundcard");
        assert_eq!(back.channels.len(), 2);
        assert_eq!(back.channels[0].input, 17);
        // And it is beside the library, never inside a show.
        assert!(venue_path(tmp.path()).ends_with("venue.json"));
    }
}
