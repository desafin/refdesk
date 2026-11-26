//! Overlays: UI drawn above the content of a window (`docs/ARCHITECTURE.md` §5).
//!
//! `fuzzy` holds the matching shared by the lists of an overlay; `palette` is the command palette
//! and quick open (CMD-1, CMD-2); `link_preview` is the popup that shows where a link leads
//! (LINK-8); `capture` is the screen capture (SHOT-1 .. SHOT-4); `translate` and
//! `translate_popup` are the translation mode and its popups (TR-1 .. TR-4).

pub mod capture;
pub mod fuzzy;
pub mod link_preview;
pub mod palette;
pub mod translate;
pub mod translate_popup;
