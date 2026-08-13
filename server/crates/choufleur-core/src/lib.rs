//! # choufleur-core
//!
//! The tracking engine: text normalization, fuzzy line matching, and the position
//! tracker. Pure logic — no I/O, no async, no wall clock. Time enters exclusively
//! as timestamps on [`TranscriptSegment`](types::TranscriptSegment), which is what
//! makes a replay run and a live run the same computation, and what makes both
//! reproducible.
//!
//! Normative references are to `docs/choufleur-notation_1.md`.

pub mod lang;
pub mod matcher;
pub mod normalize;
pub mod prompt;
pub mod script;
pub mod tracker;
pub mod types;

pub use lang::{LangCode, LangNormalizer, MatchText, NormalizerRegistry};
pub use script::{Character, PreparedScript, Script, ScriptLine, Span};
pub use tracker::{Confidence, Tracker, TrackerConfig, TrackerEvent};
pub use types::{AsrQuality, TranscriptSegment};
