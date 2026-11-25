//! Look and feel: egui's default theme (it follows the system brightness) and fonts.

/// Applies the application theme once at start-up.
pub fn install(ctx: &egui::Context) {
    configure_fonts(ctx);
}

/// Font setup. Intentionally empty: eframe's default behavior is used for now. Korean glyph
/// coverage is being verified separately; adjust here when the result is in.
pub fn configure_fonts(_ctx: &egui::Context) {}
