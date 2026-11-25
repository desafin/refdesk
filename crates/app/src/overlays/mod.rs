//! Overlays: UI drawn above the content of a window (`docs/ARCHITECTURE.md` §5).
//!
//! `fuzzy` holds the matching shared by the lists of an overlay; `palette` is the command palette
//! and quick open (CMD-1, CMD-2).

pub mod fuzzy;
pub mod palette;
