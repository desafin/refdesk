//! Highlights in the PDF view (ETC-3) and the letter typed after "Set Mark" / "Go to Mark"
//! (ETC-5).
//!
//! The view does not reach into the store of marks: once per frame `panels::marks::tick` hands
//! every PDF tab a [`HighlightEnv`] — a snapshot of its document's highlights and whether the
//! next letter belongs to a mark. Comparison views get none, so nothing is tinted there. What is
//! visible, what lies under the pointer and which letter a key names are plain functions tested
//! without egui (R-10); this module only paints and reads input.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use egui::{Color32, Event, Key, Modifiers, Pos2, Response, Ui, vec2};
use refdesk_core::{Highlight, HighlightColor, PageLayout, Point, Rect as PageRect, Size};

use crate::action::Action;
use crate::i18n::tr;
use crate::panels::marks::MarksAction;
use crate::state::TabId;
use crate::views::pdf_view::PdfViewUiState;
use crate::views::selection;

/// Side of the small square that marks a highlight with a note, in logical pixels.
const NOTE_MARKER: f32 = 6.0;

/// The highlights of one document as a view needs them for a frame.
#[derive(Debug, Default)]
pub struct DocView {
    /// The document's path as the store knows it (actions name the document by it).
    pub path: PathBuf,
    /// Every highlight piece, in document order (page first).
    pub highlights: Vec<Highlight>,
    /// Ids of the first piece of every highlight (where a note marker goes).
    heads: HashSet<u64>,
}

impl DocView {
    /// A snapshot of `highlights` (in document order) of the document at `path`.
    pub fn new(path: PathBuf, highlights: Vec<Highlight>) -> DocView {
        let mut groups = HashSet::new();
        let heads = highlights
            .iter()
            .filter(|h| groups.insert(h.group))
            .map(|h| h.id)
            .collect();
        DocView {
            path,
            highlights,
            heads,
        }
    }

    /// The pieces on `pages`.
    fn on_pages(&self, pages: std::ops::Range<u32>) -> &[Highlight] {
        let start = self.highlights.partition_point(|h| h.page < pages.start);
        let end = self.highlights.partition_point(|h| h.page < pages.end);
        &self.highlights[start..end.max(start)]
    }

    /// Whether the piece is the first of its highlight.
    pub fn is_head(&self, piece: &Highlight) -> bool {
        self.heads.contains(&piece.id)
    }

    /// The first piece of highlight `group`.
    pub fn head_of(&self, group: u64) -> Option<&Highlight> {
        self.highlights.iter().find(|h| h.group == group)
    }
}

/// Which mark the next letter is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkGrab {
    /// "Set Mark": the letter names the mark to set here.
    Set,
    /// "Go to Mark": the letter is the mark to go to.
    Go,
}

/// What a PDF view gets each frame (see the module comment).
#[derive(Clone, Debug)]
pub struct HighlightEnv {
    /// The highlights of the view's document.
    pub doc: Arc<DocView>,
    /// The next letter typed in this view is for a mark.
    pub grab: Option<MarkGrab>,
}

/// The highlight state of one view. Not saved.
#[derive(Clone, Debug, Default)]
pub struct HighlightView {
    /// Set by `panels::marks::tick` for PDF tabs; `None` for the sides of a comparison.
    pub env: Option<HighlightEnv>,
    /// The highlight that was under the pointer when the menu was opened with a right click.
    menu_target: Option<u64>,
}

impl HighlightView {
    /// The highlight the open right-click menu is about, if the click was on one.
    #[cfg(test)]
    pub fn menu_target(&self) -> Option<u64> {
        self.menu_target
    }
}

// ---------------------------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------------------------

/// One area to tint, in viewport coordinates (the top-left corner of the view is the origin).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaintRect {
    /// The area, in logical pixels.
    pub rect: PageRect,
    /// Its colour.
    pub color: HighlightColor,
    /// The highlight it belongs to.
    pub group: u64,
    /// The first line of a highlight with a note: it gets the note marker.
    pub note_marker: bool,
}

/// The areas to tint for the pieces on the pages that overlap the viewport; pieces on other
/// pages are not looked at.
pub fn visible_rects(
    doc: &DocView,
    layout: &PageLayout,
    scroll: Point,
    viewport: Size,
) -> Vec<PaintRect> {
    let scale = layout.scale();
    let mut out = Vec::new();
    for piece in doc.on_pages(layout.visible_pages(scroll, viewport)) {
        let origin = layout.page_rect(piece.page);
        let marker = !piece.note.is_empty() && doc.is_head(piece);
        for (i, r) in piece.rects.iter().enumerate() {
            out.push(PaintRect {
                rect: PageRect::new(
                    origin.x0 + r.x0 * scale - scroll.x,
                    origin.y0 + r.y0 * scale - scroll.y,
                    origin.x0 + r.x1 * scale - scroll.x,
                    origin.y0 + r.y1 * scale - scroll.y,
                ),
                color: piece.color,
                group: piece.group,
                note_marker: marker && i == 0,
            });
        }
    }
    out
}

/// The highlight piece under a viewport position (the last one drawn wins, as it is on top).
pub fn hit<'a>(
    doc: &'a DocView,
    layout: &PageLayout,
    scroll: Point,
    viewport_pos: Point,
) -> Option<&'a Highlight> {
    let at = selection::locate(layout, scroll, viewport_pos).filter(|h| h.on_page)?;
    let p = Point::new(at.loc.x, at.loc.y);
    doc.on_pages(at.loc.page..at.loc.page + 1)
        .iter()
        .rev()
        .find(|h| h.rects.iter().any(|r| r.contains(p)))
}

/// The mark letter a key press names: a to z without Shift, A to Z (a global mark) with it.
/// `None` for any other key or with Ctrl, Alt or Command held.
pub fn mark_letter(key: Key, modifiers: Modifiers) -> Option<char> {
    if modifiers.ctrl || modifiers.alt || modifiers.command || modifiers.mac_cmd {
        return None;
    }
    let name = key.name();
    let mut chars = name.chars();
    let c = match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => c,
        _ => return None,
    };
    Some(if modifiers.shift {
        c.to_ascii_uppercase()
    } else {
        c.to_ascii_lowercase()
    })
}

/// The fill of a highlight on the page: the colour, see-through.
pub fn fill(color: HighlightColor) -> Color32 {
    let (r, g, b) = rgb(color);
    Color32::from_rgba_unmultiplied(r, g, b, 96)
}

/// The colour without transparency, for the list and the note marker.
pub fn solid(color: HighlightColor) -> Color32 {
    let (r, g, b) = rgb(color);
    Color32::from_rgb(r, g, b)
}

fn rgb(color: HighlightColor) -> (u8, u8, u8) {
    match color {
        HighlightColor::Yellow => (250, 210, 0),
        HighlightColor::Green => (80, 200, 80),
        HighlightColor::Blue => (70, 150, 250),
        HighlightColor::Pink => (245, 100, 180),
        HighlightColor::Orange => (250, 140, 30),
    }
}

/// The colour's name for the reader ("yellow", "노랑").
pub fn color_name(color: HighlightColor) -> &'static str {
    tr(match color {
        HighlightColor::Yellow => "marks.color.yellow",
        HighlightColor::Green => "marks.color.green",
        HighlightColor::Blue => "marks.color.blue",
        HighlightColor::Pink => "marks.color.pink",
        HighlightColor::Orange => "marks.color.orange",
    })
}

// ---------------------------------------------------------------------------------------------
// Input and painting
// ---------------------------------------------------------------------------------------------

/// While the next letter belongs to a mark (`AppState::key_grab`), takes the first key press of
/// this frame from the input and turns it into the end of the mark: a letter finishes it, any
/// other key (Esc too) cancels it. Typed text of the frame is taken as well, so nothing else
/// sees the letter. Called once per frame by the view of the tab that waits.
pub fn handle_keys(ui: &mut Ui, view: &HighlightView, tab: TabId, queue: &mut Vec<Action>) {
    if view.env.as_ref().and_then(|e| e.grab).is_none() {
        return;
    }
    let pressed = ui.input_mut(|i| {
        let at = i
            .events
            .iter()
            .position(|e| matches!(e, Event::Key { pressed: true, .. }))?;
        let Event::Key { key, modifiers, .. } = i.events.remove(at) else {
            return None;
        };
        i.events.retain(|e| !matches!(e, Event::Text(_)));
        Some((key, modifiers))
    });
    if let Some((key, modifiers)) = pressed {
        queue.push(Action::Marks(MarksAction::FinishMark {
            tab,
            letter: mark_letter(key, modifiers),
        }));
    }
}

fn to_rect(view_rect: egui::Rect, r: &PageRect) -> egui::Rect {
    egui::Rect::from_min_max(
        view_rect.min + vec2(r.x0, r.y0),
        view_rect.min + vec2(r.x1, r.y1),
    )
}

/// Paints the highlights of the view's document over the pages (below the selection and the
/// search marks), remembers the highlight under a right click for the menu, and shows the note
/// of the highlight under the pointer as a tooltip (not over a link, whose tooltip wins).
pub fn paint(
    ui: &Ui,
    response: &Response,
    view_rect: egui::Rect,
    state: &mut PdfViewUiState,
    over_link: bool,
) {
    let Some(env) = state.highlights.env.clone() else {
        return;
    };
    let Some(layout) = state.layout() else {
        return;
    };
    let scroll = state.scroll();
    let rects = visible_rects(&env.doc, layout, scroll, state.viewport());
    let painter = ui.painter().with_clip_rect(view_rect);
    for r in &rects {
        let rect = to_rect(view_rect, &r.rect);
        painter.rect_filled(rect, 1.0, fill(r.color));
        if r.note_marker {
            let marker = egui::Rect::from_min_size(
                Pos2::new(
                    rect.right() - NOTE_MARKER / 2.0,
                    rect.top() - NOTE_MARKER / 2.0,
                ),
                vec2(NOTE_MARKER, NOTE_MARKER),
            );
            painter.rect_filled(marker, 1.0, solid(r.color));
            painter.rect_stroke(
                marker,
                1.0,
                egui::Stroke::new(1.0, Color32::from_gray(60)),
                egui::StrokeKind::Inside,
            );
        }
    }
    let pointer = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| view_rect.contains(*p));
    let under = pointer.and_then(|p| {
        let v = p - view_rect.min;
        hit(&env.doc, layout, scroll, Point::new(v.x, v.y))
    });
    if response.secondary_clicked() {
        state.highlights.menu_target = under.map(|h| h.group);
    }
    if let Some(piece) = under
        && !over_link
        && response.hovered()
        && !ui.ctx().any_popup_open()
    {
        let note = env
            .doc
            .head_of(piece.group)
            .map(|h| h.note.clone())
            .unwrap_or_default();
        if !note.is_empty() {
            response.clone().on_hover_text_at_pointer(note);
        }
    }
}

/// A menu button with a square of `color` before its name.
fn color_button(ui: &mut Ui, color: HighlightColor) -> bool {
    let response = ui.add(egui::Button::new(format!("     {}", color_name(color))));
    let h = response.rect.height().min(12.0);
    let swatch = egui::Rect::from_min_size(
        Pos2::new(
            response.rect.left() + 6.0,
            response.rect.center().y - h / 2.0,
        ),
        vec2(h, h),
    );
    ui.painter().rect_filled(swatch, 2.0, solid(color));
    response.clicked()
}

/// The entries of the view's right-click menu that are about highlights: "Highlight ▸" with the
/// five colours when text is selected, and for the highlight under the click "Change Color ▸",
/// "Note…", "Copy Text" and "Remove". A separator follows when anything was added. Nothing in a
/// comparison view.
pub fn context_menu(ui: &mut Ui, state: &PdfViewUiState, tab: TabId, queue: &mut Vec<Action>) {
    let Some(env) = &state.highlights.env else {
        return;
    };
    let mut added = false;
    if state.has_selection() {
        ui.menu_button(tr("marks.menu.highlight"), |ui| {
            for color in HighlightColor::ALL {
                if color_button(ui, color) {
                    queue.push(Action::Marks(MarksAction::Highlight {
                        tab,
                        color: Some(color),
                    }));
                    ui.close();
                }
            }
        });
        added = true;
    }
    let target = state
        .highlights
        .menu_target
        .filter(|g| env.doc.head_of(*g).is_some());
    if let Some(group) = target {
        let doc = env.doc.path.clone();
        ui.menu_button(tr("marks.menu.change_color"), |ui| {
            for color in HighlightColor::ALL {
                if color_button(ui, color) {
                    queue.push(Action::Marks(MarksAction::SetColor {
                        doc: doc.clone(),
                        group,
                        color,
                    }));
                    ui.close();
                }
            }
        });
        let entries = [
            (
                "marks.menu.note",
                MarksAction::EditNote {
                    doc: doc.clone(),
                    group,
                },
            ),
            (
                "marks.menu.copy_text",
                MarksAction::CopyHighlight {
                    doc: doc.clone(),
                    group,
                },
            ),
            (
                "marks.menu.remove",
                MarksAction::RemoveHighlight {
                    doc: doc.clone(),
                    group,
                },
            ),
        ];
        for (key, action) in entries {
            if ui.button(tr(key)).clicked() {
                queue.push(Action::Marks(action));
                ui.close();
            }
        }
        added = true;
    }
    if added {
        ui.separator();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refdesk_core::{LayoutParams, ZoomMode};

    fn piece(id: u64, group: u64, page: u32, y: f32, note: &str) -> Highlight {
        Highlight {
            id,
            group,
            color: HighlightColor::Green,
            page,
            chars: 0..5,
            rects: vec![
                PageRect::new(72.0, y, 172.0, y + 12.0),
                PageRect::new(72.0, y + 16.0, 122.0, y + 28.0),
            ],
            text: "text".into(),
            note: note.into(),
            created: 0,
        }
    }

    fn layout(pages: usize) -> PageLayout {
        let sizes = vec![Size::new(612.0, 792.0); pages];
        PageLayout::new(
            &sizes,
            LayoutParams {
                viewport: Size::new(800.0, 600.0),
                zoom: ZoomMode::Custom(1.0),
                gap: 8.0,
                margin: 12.0,
                fit_page: 0,
            },
        )
    }

    #[test]
    fn only_the_pieces_on_visible_pages_give_rectangles() {
        let doc = DocView::new(
            PathBuf::from("/d.pdf"),
            vec![
                piece(1, 1, 0, 100.0, ""),
                piece(2, 2, 5, 100.0, ""),
                piece(3, 3, 40, 100.0, ""),
            ],
        );
        let l = layout(50);
        let top = l.content_pos(refdesk_core::Location::new(5, 0.0, 0.0));
        let rects = visible_rects(&doc, &l, top, Size::new(800.0, 600.0));
        assert!(!rects.is_empty());
        assert!(rects.iter().all(|r| r.group == 2), "{rects:?}");
        assert_eq!(rects.len(), 2, "one per line");
        // In viewport coordinates: page 5's top is at the top of the view.
        let scale = l.scale();
        assert!((rects[0].rect.y0 - 100.0 * scale).abs() < 0.5);
    }

    #[test]
    fn the_note_marker_goes_on_the_first_line_of_the_first_piece_only() {
        let doc = DocView::new(
            PathBuf::from("/d.pdf"),
            vec![piece(1, 9, 0, 100.0, "n"), piece(2, 9, 1, 50.0, "n")],
        );
        let l = layout(2);
        let rects = visible_rects(&doc, &l, Point::new(0.0, 0.0), Size::new(800.0, 2000.0));
        let marked: Vec<&PaintRect> = rects.iter().filter(|r| r.note_marker).collect();
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].rect, rects[0].rect);
        let without = DocView::new(PathBuf::from("/d.pdf"), vec![piece(1, 9, 0, 100.0, "")]);
        assert!(
            visible_rects(&without, &l, Point::new(0.0, 0.0), Size::new(800.0, 900.0))
                .iter()
                .all(|r| !r.note_marker)
        );
    }

    #[test]
    fn the_highlight_under_the_pointer_is_found_on_its_page() {
        let doc = DocView::new(
            PathBuf::from("/d.pdf"),
            vec![piece(1, 1, 0, 100.0, "note"), piece(2, 2, 1, 100.0, "")],
        );
        let l = layout(2);
        let scroll = Point::new(0.0, 0.0);
        let scale = l.scale();
        let origin = l.page_rect(0);
        let at = |x: f32, y: f32| Point::new(origin.x0 + x * scale, origin.y0 + y * scale);
        assert_eq!(
            hit(&doc, &l, scroll, at(100.0, 105.0)).map(|h| h.id),
            Some(1)
        );
        assert_eq!(
            hit(&doc, &l, scroll, at(100.0, 121.0)).map(|h| h.id),
            Some(1)
        );
        assert!(
            hit(&doc, &l, scroll, at(150.0, 121.0)).is_none(),
            "past the short line"
        );
        assert!(hit(&doc, &l, scroll, at(100.0, 300.0)).is_none());
        let origin1 = l.page_rect(1);
        let p = Point::new(origin1.x0 + 100.0 * scale, origin1.y0 + 105.0 * scale);
        assert_eq!(hit(&doc, &l, scroll, p).map(|h| h.id), Some(2));
    }

    #[test]
    fn a_letter_names_a_mark_and_shift_makes_it_global() {
        assert_eq!(mark_letter(Key::A, Modifiers::NONE), Some('a'));
        assert_eq!(mark_letter(Key::Z, Modifiers::NONE), Some('z'));
        assert_eq!(mark_letter(Key::Q, Modifiers::SHIFT), Some('Q'));
        assert_eq!(mark_letter(Key::A, Modifiers::CTRL), None);
        assert_eq!(mark_letter(Key::A, Modifiers::ALT), None);
        assert_eq!(mark_letter(Key::Escape, Modifiers::NONE), None);
        assert_eq!(mark_letter(Key::Num1, Modifiers::NONE), None);
        assert_eq!(mark_letter(Key::Space, Modifiers::NONE), None);
        assert_eq!(mark_letter(Key::F2, Modifiers::NONE), None);
    }

    #[test]
    fn every_colour_has_a_name_in_both_languages() {
        for color in HighlightColor::ALL {
            assert_ne!(color_name(color), "???");
            assert_eq!(fill(color).a(), 96);
        }
    }
}
