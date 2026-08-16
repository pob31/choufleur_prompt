//! The show server: what holds a show on disk and serves it to the room.
//!
//! Split out of `choufleur-replay` because two things now drive it — the replay harness
//! that measures against the corpus, and the desktop shell that runs a performance — and
//! neither should have to carry the other's dependencies to do so.
//!
//! What belongs here is everything between the tracker and the screens: the wire
//! protocol, the live state a client resyncs against, and the reading and writing of
//! show files. What does not belong here is audio, recognition, or the engine loop; the
//! engine stays a blocking thread in the harness that owns it, bridged to this side by a
//! channel and nothing else.

pub mod check;
pub mod import;
pub mod library;
pub mod store;
pub mod transfer;

pub use library::{Library, Entry};
pub use store::{Store, Version};
