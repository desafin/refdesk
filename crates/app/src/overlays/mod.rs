//! Overlays: UI drawn above the content of a window (`docs/ARCHITECTURE.md` §5).
//!
//! `fuzzy` holds the matching shared by the lists of an overlay; `palette` is the command palette
//! and quick open (CMD-1, CMD-2); `link_preview` is the popup that shows where a link leads
//! (LINK-8); `capture` is the screen capture (SHOT-1 .. SHOT-4); `translate` and
//! `translate_popup` are the translation mode and its popups (TR-1 .. TR-4); `region_zoom` is the
//! region zoom and the middle-button pan (VIEW-14, VIEW-6); `link_hints` are the keyboard link
//! hints (LINK-13); `pinned_preview` are the pinned link previews (LINK-14); `pretranslate` is
//! background translation in a project (TR-15 ~ 19).

pub mod capture;
pub mod fuzzy;
pub mod link_hints;
pub mod link_preview;
pub mod palette;
pub mod pinned_preview;
pub mod pretranslate;
pub mod region_zoom;
pub mod translate;
pub mod translate_popup;
