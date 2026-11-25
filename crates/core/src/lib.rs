//! refdesk-core: document engine and view logic for refdesk.
//!
//! This crate holds everything that is not drawing or input handling. It must not depend on
//! egui, eframe or winit (rule R-4 in `docs/ARCHITECTURE.md`). Modules are added by later
//! work packages.
