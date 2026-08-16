//! Making, listing and filling shows — the library from a terminal.
//!
//! The Shows screen will do all of this with a window and a file dialog. This exists
//! first because the operations are the same either way, and having them behind a
//! command means they can be tested, scripted and used today rather than only once the
//! shell is standing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use choufleur_server::Library;

use choufleur_server::import;

/// Where shows live when nobody says otherwise.
pub fn default_root() -> PathBuf {
    std::env::var_os("CHOUFLEUR_LIBRARY")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Choufleur")))
        .unwrap_or_else(|| PathBuf::from("Choufleur"))
}

pub fn list(root: &Path) -> Result<()> {
    let shows = Library::new(root).list()?;
    if shows.is_empty() {
        println!("no shows in {}", root.display());
        println!("  choufleur-replay show new \"My Show\"");
        return Ok(());
    }
    println!("{}\n", root.display());
    for s in &shows {
        let title = s.title.as_deref().filter(|t| *t != s.name);
        println!(
            "  {:<28} {:>5} lines  {} list{}{}{}",
            s.name,
            s.lines,
            s.sheets.len(),
            if s.sheets.len() == 1 { "" } else { "s" },
            match s.versions {
                0 => String::new(),
                n => format!("  {n} version{}", if n == 1 { "" } else { "s" }),
            },
            title.map(|t| format!("  — {t}")).unwrap_or_default(),
        );
    }
    Ok(())
}

pub fn new(root: &Path, name: &str, from: Option<&Path>) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("creating the library at {}", root.display()))?;
    let lib = Library::new(root);
    let show = lib.create(name)?;
    println!("created {}", show.dir.display());

    if let Some(text) = from {
        let report = import::text_file(&show.script, text)?;
        println!("\n{report}");
    }
    println!("\n  choufleur-replay serve {} --prep", show.manifest.display());
    Ok(())
}

pub fn import_show(root: &Path, manifest: &Path, name: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let show = Library::new(root).import(manifest, name)?;
    println!(
        "imported {} — {} lines, {} cue list{}",
        show.dir.display(),
        show.lines,
        show.sheets.len(),
        if show.sheets.len() == 1 { "" } else { "s" }
    );
    println!("the original at {} was not touched", manifest.display());
    println!("\n  choufleur-replay serve {} --prep", show.manifest.display());
    Ok(())
}

/// The rules a preparer works to — printed so they can be handed to an AI.
///
/// Prep is judgement work and the heuristic importer is deliberately bad at it. Handing
/// that judgement to a model is a good division of labour, and this is its half of the
/// contract: what the file is for, what the two failure modes cost, and the shape of the
/// answer. `show check` is the other half.
pub fn rules() {
    print!("{}", include_str!("../../../../../docs/choufleur-prep-rules.md"));
}

/// Check a prepared script before trusting it.
pub fn check(script: &Path, source: Option<&Path>) -> Result<()> {
    let text = match source {
        Some(p) => Some(
            std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        ),
        None => None,
    };
    let report = choufleur_server::check::script(script, text.as_deref())?;
    println!("{report}");
    if source.is_none() {
        println!("\n(pass --source to check that no text went missing — the check that matters)");
    }
    anyhow::ensure!(report.ok(), "{} problem(s) must be fixed first", report.fatal());
    Ok(())
}

/// Bring a cue list in from anywhere, re-anchored against this show's script.
pub fn cues_in(show: &Path, from: &Path, name: Option<&str>) -> Result<()> {
    let report = choufleur_server::transfer::import_sheet(show, from, name)?;
    println!("{report}\n\nwrote {}", report.sheet.display());
    Ok(())
}

/// Copy a cue list out, to carry to another show.
pub fn cues_out(sheet: &Path, to: &Path) -> Result<()> {
    let dest = choufleur_server::transfer::export_sheet(sheet, to)?;
    println!("wrote {}", dest.display());
    Ok(())
}

/// Read a text file into an existing show's script, replacing whatever was there.
pub fn add_text(script: &Path, text: &Path) -> Result<()> {
    let report = import::text_file(script, text)?;
    println!("{report}");
    Ok(())
}
