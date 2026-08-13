pub mod eval;
pub mod make_fixture;
pub mod track;
pub mod verify;

use std::path::Path;

use anyhow::{Context, Result};
use choufleur_core::lang::NormalizerRegistry;
use choufleur_core::script::{PreparedScript, Script};

/// Load and index a corpus's script — the step every subcommand starts from.
pub fn load_script(path: &Path) -> Result<(Script, PreparedScript)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading script {}", path.display()))?;
    let script: Script = serde_json::from_str(&text)
        .with_context(|| format!("parsing script {}", path.display()))?;
    let mut reg = NormalizerRegistry::with_defaults();
    let prepared = PreparedScript::build(&script, &mut reg);
    Ok((script, prepared))
}
