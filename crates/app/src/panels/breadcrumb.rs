//! The section path above a PDF view (TOC-6): a thin line that says where in the document the
//! reader is, "7 Functional specification › 7.3 State management › 7.3.2 Transitions". Each step
//! is a button that goes to the start of that section.
//!
//! The section is the one the table of contents panel marks: the outline item at the quarter
//! height line of the viewport ([`reading_location`] and `Outline::current`). The line is not
//! drawn, and takes no room, for a document without a table of contents and before its first
//! section.
//!
//! What is decided is plain code, tested without egui (rule R-10): the path ([`path_at`]) and how
//! it is shortened for a narrow line ([`fit`], [`ellipsize`]). [`show`] measures and paints.

use egui::{Color32, CursorIcon, Id, Rect, Sense, TextStyle, Ui, pos2, vec2};
use refdesk_core::{DocumentInfo, Location, Outline};

use crate::action::{Action, OpenHow};
use crate::panels::toc::reading_location;
use crate::state::{PdfTab, TabId};
use crate::views::pdf_view::SCROLLBAR;

/// The separator between steps.
pub const SEPARATOR: &str = "›";
/// Stands for the steps that do not fit, and ends a title that is cut.
pub const ELLIPSIS: &str = "…";
/// Space left and right of the path.
const PAD: f32 = 8.0;
/// Space left and right of a separator.
const SEPARATOR_PAD: f32 = 5.0;
/// Space above and below the text.
const ROW_PAD: f32 = 3.0;

// ------------------------------------------------------------------------------------------------
// What the path is
// ------------------------------------------------------------------------------------------------

/// One step of the path.
#[derive(Clone, Debug, PartialEq)]
pub struct Crumb {
    /// Index into `Outline::items`.
    pub idx: u32,
    /// The title of the section.
    pub title: String,
    /// Where the section starts; `None` for a section that does not lead anywhere in the
    /// document.
    pub go: Option<Location>,
}

/// The index of the section that contains `at`, if the document has a table of contents that
/// reaches that far.
pub fn current_section(outline: &Outline, at: Location) -> Option<u32> {
    outline.current(at)
}

/// The path from the top level to the section `idx`. The place a click goes to has x = 0: the x
/// of a heading is the margin of the page, and going there must not scroll a zoomed view
/// sideways.
pub fn path_to(outline: &Outline, idx: u32) -> Vec<Crumb> {
    let mut chain = outline.ancestors(idx);
    chain.reverse();
    chain.push(idx);
    chain
        .into_iter()
        .filter_map(|i| outline.items.get(i as usize).map(|item| (i, item)))
        .map(|(idx, item)| Crumb {
            idx,
            title: item.title.trim().to_owned(),
            go: item.target.map(|t| Location::new(t.page, 0.0, t.y)),
        })
        .collect()
}

// ------------------------------------------------------------------------------------------------
// Fitting it into the line
// ------------------------------------------------------------------------------------------------

/// A piece of the line as drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// A step: the index into the path and the text shown (the title, or a cut one).
    Step {
        /// Index into the path.
        index: usize,
        /// The text.
        text: String,
    },
    /// The steps that were left out, from the first one on.
    Hidden,
}

/// The text of `text` cut to at most `max` wide, ending with [`ELLIPSIS`]; unchanged when it
/// fits. `measure` gives the width of a text.
pub fn ellipsize(text: &str, max: f32, measure: &dyn Fn(&str) -> f32) -> String {
    if measure(text) <= max {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let with_ellipsis = |n: usize| {
        let mut s: String = chars[..n].iter().collect();
        s.push_str(ELLIPSIS);
        s
    };
    // The most characters that still fit: widths grow with the length.
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&with_ellipsis(mid)) <= max {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    with_ellipsis(lo)
}

/// The steps of a path as they fit in a line `available` wide, `separator` being the width of
/// a separator with its space. Steps are left out from the front, replaced by one
/// [`Part::Hidden`]; the last step is always there. When the last step alone is too wide it is
/// cut with [`ELLIPSIS`] (keeping the [`Part::Hidden`] before it only if there is room for it).
pub fn fit(
    titles: &[&str],
    available: f32,
    separator: f32,
    measure: &dyn Fn(&str) -> f32,
) -> Vec<Part> {
    let n = titles.len();
    let Some(last) = titles.last() else {
        return Vec::new();
    };
    let width: Vec<f32> = titles.iter().map(|t| measure(t)).collect();
    let step = |index: usize| Part::Step {
        index,
        text: titles[index].to_owned(),
    };
    let hidden = measure(ELLIPSIS) + separator;
    // The most steps, counted from the end, that fit.
    for first in 0..n {
        let steps = &width[first..];
        let total: f32 = steps.iter().sum::<f32>() + separator * (steps.len() - 1) as f32;
        let lead = if first > 0 { hidden } else { 0.0 };
        if lead + total <= available {
            let mut parts = Vec::new();
            if first > 0 {
                parts.push(Part::Hidden);
            }
            parts.extend((first..n).map(step));
            return parts;
        }
    }
    // Only the last step is left, and it is too wide.
    let mut parts = Vec::new();
    let mut room = available;
    if n > 1 && available - hidden > measure(ELLIPSIS) * 2.0 {
        parts.push(Part::Hidden);
        room -= hidden;
    }
    parts.push(Part::Step {
        index: n - 1,
        text: ellipsize(last, room, measure),
    });
    parts
}

// ------------------------------------------------------------------------------------------------
// Drawing
// ------------------------------------------------------------------------------------------------

/// The height of the viewport of the view below, as if the line were not there. The decision to
/// draw the line must not depend on whether it was drawn (its height moves the quarter line, and
/// the line would flicker at a section boundary).
pub fn view_height_without_line(ui: &Ui) -> f32 {
    (ui.available_height() - SCROLLBAR).max(0.0)
}

/// The section the view is in, judged as the table of contents panel does, at `viewport_h`.
pub fn section_of(tab: &PdfTab, info: &DocumentInfo, viewport_h: f32) -> Option<u32> {
    if info.outline.items.is_empty() {
        return None;
    }
    let at = reading_location(tab.view.anchor, tab.ui.layout(), viewport_h);
    current_section(&info.outline, at)
}

/// Draws the line for the section the view is in, and queues the jump when a step is clicked.
/// Draws nothing, and takes no room, when there is no section. `viewport_h` comes from
/// [`view_height_without_line`]. Returns the section that was drawn.
pub fn show(
    ui: &mut Ui,
    tab: &PdfTab,
    info: &DocumentInfo,
    tab_id: TabId,
    viewport_h: f32,
    queue: &mut Vec<Action>,
) -> Option<u32> {
    let current = section_of(tab, info, viewport_h)?;
    let path = path_to(&info.outline, current);
    if path.is_empty() {
        return None;
    }

    let font = TextStyle::Body.resolve(ui.style());
    let row_h = ui.text_style_height(&TextStyle::Body) + ROW_PAD * 2.0;
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::hover());
    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(row);
    painter.rect_filled(row, 0.0, visuals.faint_bg_color);
    painter.line_segment(
        [row.left_bottom(), row.right_bottom()],
        visuals.widgets.noninteractive.bg_stroke,
    );

    let measure = |text: &str| {
        painter
            .layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER)
            .size()
            .x
    };
    let separator = measure(SEPARATOR) + SEPARATOR_PAD * 2.0;
    let titles: Vec<&str> = path.iter().map(|c| c.title.as_str()).collect();
    let parts = fit(&titles, row.width() - PAD * 2.0, separator, &measure);

    let mut x = row.min.x + PAD;
    let last = path.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            let at = pos2(x + SEPARATOR_PAD, row.center().y);
            painter.text(
                at,
                egui::Align2::LEFT_CENTER,
                SEPARATOR,
                font.clone(),
                visuals.weak_text_color(),
            );
            x += separator;
        }
        let (text, index) = match part {
            Part::Step { index, text } => (text.as_str(), Some(*index)),
            Part::Hidden => (ELLIPSIS, None),
        };
        let w = measure(text);
        let rect = Rect::from_min_size(pos2(x, row.min.y), vec2(w, row_h));
        x += w;

        let id = Id::unique(("section_path", tab_id, i));
        let response = ui.interact(rect.expand2(vec2(2.0, 0.0)), id, Sense::click());
        let go = index.and_then(|i| path[i].go);
        let color = match (index, response.hovered() && go.is_some()) {
            (_, true) => visuals.hyperlink_color,
            (Some(i), _) if i == last => visuals.strong_text_color(),
            _ => visuals.text_color(),
        };
        painter.text(
            pos2(rect.min.x, row.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            font.clone(),
            color,
        );
        if response.hovered() && go.is_some() {
            painter.line_segment(
                [
                    pos2(rect.min.x, row.center().y + row_h / 2.0 - ROW_PAD),
                    pos2(rect.max.x, row.center().y + row_h / 2.0 - ROW_PAD),
                ],
                egui::Stroke::new(1.0, color),
            );
        }
        // The whole title on hover; for the hidden steps, their titles.
        let full = match index {
            Some(i) => path[i].title.clone(),
            None => {
                let hidden_until = match parts.get(1) {
                    Some(Part::Step { index, .. }) => *index,
                    _ => last,
                };
                titles[..hidden_until].join(&format!(" {SEPARATOR} "))
            }
        };
        let response = response.on_hover_text(full);
        if go.is_some() {
            response.clone().on_hover_cursor(CursorIcon::PointingHand);
        }
        if response.clicked()
            && let Some(to) = go
        {
            queue.push(Action::Navigate {
                tab: tab_id,
                to,
                how: OpenHow::Here,
            });
        }
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use refdesk_core::OutlineNode;

    use crate::action::{Action, OpenHow, dispatch};
    use crate::overlays::link_preview::tests::{Harness, opened_document};

    fn node(title: &str, page: u32, y: f32, children: Vec<OutlineNode>) -> OutlineNode {
        OutlineNode {
            title: title.to_string(),
            target: Some(Location::new(page, 72.0, y)),
            uri: None,
            children,
        }
    }

    /// 7 › 7.3 › 7.3.2, then 8, and an item before them all on page 0.
    fn outline() -> Outline {
        Outline::from_tree(vec![
            node("Preface", 0, 10.0, vec![]),
            node(
                "7 Functional specification",
                1,
                40.0,
                vec![node(
                    "7.3 State management",
                    1,
                    200.0,
                    vec![node("7.3.2 Transitions", 2, 100.0, vec![])],
                )],
            ),
            node("8 Annex", 3, 50.0, vec![]),
        ])
    }

    fn titles(path: &[Crumb]) -> Vec<&str> {
        path.iter().map(|c| c.title.as_str()).collect()
    }

    #[test]
    fn at_depth_three_the_path_has_three_steps_from_the_top() {
        let o = outline();
        let idx = current_section(&o, Location::new(2, 0.0, 150.0)).expect("section");
        let path = path_to(&o, idx);
        assert_eq!(
            titles(&path),
            [
                "7 Functional specification",
                "7.3 State management",
                "7.3.2 Transitions"
            ]
        );
        // Each step goes to the start of its section, at x = 0.
        assert_eq!(path[0].go, Some(Location::new(1, 0.0, 40.0)));
        assert_eq!(path[1].go, Some(Location::new(1, 0.0, 200.0)));
        assert_eq!(path[2].go, Some(Location::new(2, 0.0, 100.0)));
        // Between the second and the third heading: two steps.
        let idx = current_section(&o, Location::new(1, 0.0, 300.0)).expect("section");
        assert_eq!(titles(&path_to(&o, idx)).len(), 2);
        // A top-level section: one step.
        let idx = current_section(&o, Location::new(3, 0.0, 60.0)).expect("section");
        assert_eq!(titles(&path_to(&o, idx)), ["8 Annex"]);
    }

    #[test]
    fn there_is_no_path_before_the_first_section_or_without_an_outline() {
        let o = outline();
        assert_eq!(current_section(&o, Location::new(0, 0.0, 5.0)), None);
        assert_eq!(
            current_section(&Outline::default(), Location::new(2, 0.0, 5.0)),
            None
        );
    }

    #[test]
    fn titles_are_trimmed() {
        let o = Outline::from_tree(vec![node("  Padded \n", 0, 0.0, vec![])]);
        assert_eq!(path_to(&o, 0)[0].title, "Padded");
    }

    /// Every character is 10 wide.
    fn m(text: &str) -> f32 {
        text.chars().count() as f32 * 10.0
    }

    fn texts(parts: &[Part]) -> Vec<String> {
        parts
            .iter()
            .map(|p| match p {
                Part::Step { text, .. } => text.clone(),
                Part::Hidden => ELLIPSIS.to_owned(),
            })
            .collect()
    }

    #[test]
    fn a_path_that_fits_is_shown_whole() {
        let titles = ["aaaa", "bbbb", "cccc"];
        // 12 characters and two separators of 20.
        let parts = fit(&titles, 160.0, 20.0, &m);
        assert_eq!(texts(&parts), ["aaaa", "bbbb", "cccc"]);
        assert_eq!(
            parts[2],
            Part::Step {
                index: 2,
                text: "cccc".into()
            }
        );
    }

    #[test]
    fn a_narrow_line_drops_steps_from_the_front_and_the_last_one_stays() {
        let titles = ["aaaa", "bbbb", "cccc"];
        // Whole: 120 + 40 = 160. One step less: "…" (10 + 20) + 80 + 20 = 130.
        let parts = fit(&titles, 150.0, 20.0, &m);
        assert_eq!(texts(&parts), ["…", "bbbb", "cccc"]);
        assert!(matches!(parts[0], Part::Hidden));
        assert!(matches!(&parts[1], Part::Step { index: 1, .. }));
        // Narrower: only the last, after the "…" (30 + 40 = 70).
        let parts = fit(&titles, 100.0, 20.0, &m);
        assert_eq!(texts(&parts), ["…", "cccc"]);
        let parts = fit(&titles, 70.0, 20.0, &m);
        assert_eq!(texts(&parts), ["…", "cccc"]);
    }

    #[test]
    fn a_last_step_that_is_too_wide_is_cut_with_an_ellipsis() {
        let titles = ["aaaa", "bbbb", "a very long last title"];
        let parts = fit(&titles, 120.0, 20.0, &m);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], Part::Hidden);
        let Part::Step { index, text } = &parts[1] else {
            panic!("a step");
        };
        assert_eq!(*index, 2);
        assert!(text.ends_with(ELLIPSIS) && text.chars().count() > 1);
        // The line, with the hidden steps in front, fits.
        let used: f32 = m(ELLIPSIS) + 20.0 + m(text);
        assert!(used <= 120.0, "{used}");
        // And one more character would not.
        let more = format!("{}x{ELLIPSIS}", &text[..text.len() - ELLIPSIS.len()]);
        assert!(m(ELLIPSIS) + 20.0 + m(&more) > 120.0);

        // A single step is cut the same way, with nothing before it.
        let parts = fit(&["a very long title"], 80.0, 20.0, &m);
        assert_eq!(texts(&parts), ["a very …"]);
    }

    #[test]
    fn with_hardly_any_room_the_last_step_gets_all_of_it() {
        let titles = ["aaaa", "a very long last title"];
        let parts = fit(&titles, 30.0, 20.0, &m);
        assert_eq!(parts.len(), 1);
        assert_eq!(texts(&parts), ["a …"]);
        // Nothing fits at all: just the ellipsis.
        let parts = fit(&titles, 5.0, 20.0, &m);
        assert_eq!(texts(&parts), ["…"]);
        assert!(fit(&[], 100.0, 20.0, &m).is_empty());
    }

    #[test]
    fn ellipsize_keeps_what_fits() {
        assert_eq!(ellipsize("abc", 30.0, &m), "abc");
        assert_eq!(ellipsize("abcdef", 40.0, &m), "abc…");
        // Counts characters, not bytes.
        assert_eq!(ellipsize("가나다라마", 30.0, &m), "가나…");
    }

    // ---- on a document, frame by frame ----

    const SECTION_1: &str = "7 Functional specification";
    const SECTION_2: &str = "7.3 State management";
    const SECTION_3: &str = "7.3.2 Transitions";

    fn go_to(h: &mut Harness, tab: crate::state::TabId, to: Location) {
        dispatch(
            &mut h.state,
            Action::Navigate {
                tab,
                to,
                how: OpenHow::Here,
            },
            &h.ctx,
        );
        // The line follows the view one frame later.
        for _ in 0..3 {
            h.frame(Vec::new());
        }
    }

    #[test]
    fn the_line_is_not_drawn_before_the_first_section() {
        let (mut h, _tab, _pdf) = opened_document("crumb-before");
        h.frame(Vec::new());
        for title in [SECTION_1, SECTION_2, SECTION_3] {
            assert!(!h.drew(title), "{:?}", h.drawn);
        }
    }

    #[test]
    fn the_line_shows_the_path_of_the_section_the_view_is_in() {
        let (mut h, tab, _pdf) = opened_document("crumb-path");
        go_to(&mut h, tab, Location::new(2, 0.0, 200.0));
        for title in [SECTION_1, SECTION_2, SECTION_3] {
            assert!(h.drew(title), "{title}: {:?}", h.drawn);
        }
        // Two steps between the second and the third heading.
        go_to(&mut h, tab, Location::new(1, 0.0, 250.0));
        assert!(h.drew(SECTION_1) && h.drew(SECTION_2) && !h.drew(SECTION_3));
        // Leaving the line out: not drawn, and the view has the room.
        let height_with = h.tab(tab).ui.viewport().h;
        dispatch(&mut h.state, Action::ToggleBreadcrumb, &h.ctx);
        assert!(!h.state.show_breadcrumb);
        for _ in 0..3 {
            h.frame(Vec::new());
        }
        assert!(!h.drew(SECTION_1), "{:?}", h.drawn);
        assert!(h.tab(tab).ui.viewport().h > height_with);
        dispatch(&mut h.state, Action::ToggleBreadcrumb, &h.ctx);
        for _ in 0..3 {
            h.frame(Vec::new());
        }
        assert!(h.drew(SECTION_1));
    }

    #[test]
    fn clicking_a_step_goes_to_the_start_of_that_section() {
        let (mut h, tab, _pdf) = opened_document("crumb-click");
        go_to(&mut h, tab, Location::new(2, 0.0, 200.0));
        let (_, pos) = h
            .drawn
            .iter()
            .find(|(t, _)| t == SECTION_2)
            .cloned()
            .expect("the second step");
        let at = pos + egui::vec2(3.0, 3.0);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        h.frame(vec![egui::Event::PointerMoved(at)]);
        h.frame(vec![button(true)]);
        h.frame(vec![button(false)]);
        h.frame(Vec::new());
        let anchor = h.tab(tab).view.anchor;
        assert_eq!(anchor.page, 1);
        // The section starts at y = 200 of that page (the view puts it at the top).
        assert!((anchor.y - 200.0).abs() < 30.0, "{anchor:?}");
        // The jump is recorded: back returns.
        assert!(h.tab(tab).view.history.can_back());
        dispatch(&mut h.state, Action::Back(tab), &h.ctx);
        for _ in 0..3 {
            h.frame(Vec::new());
        }
        assert_eq!(h.tab(tab).view.anchor.page, 2);
    }

    #[test]
    fn the_line_does_not_flicker_where_a_section_starts() {
        // The view is 588 high without the line and its quarter line is at 147. Put that line
        // 1 pt below the start of the first section: with the line drawn the view is a few pixels
        // lower and its quarter line would be above the section.
        let (mut h, tab, _pdf) = opened_document("crumb-flicker");
        h.frame(Vec::new());
        let scale = h.tab(tab).ui.layout().expect("layout").scale();
        let top = 40.0 + 1.0 - 147.0 / scale;
        if let Some(crate::state::Tab::Pdf(pdf)) = h.state.tabs.get_mut(tab) {
            pdf.view.anchor = Location::new(1, 0.0, top);
        }
        let mut seen = Vec::new();
        for _ in 0..12 {
            h.frame(Vec::new());
            seen.push(h.drew(SECTION_1));
        }
        assert!(seen.iter().all(|s| *s), "{seen:?}");
        // And 1 pt above it: never drawn.
        if let Some(crate::state::Tab::Pdf(pdf)) = h.state.tabs.get_mut(tab) {
            pdf.view.anchor = Location::new(1, 0.0, top - 2.0);
        }
        let mut seen = Vec::new();
        for _ in 0..12 {
            h.frame(Vec::new());
            seen.push(h.drew(SECTION_1));
        }
        assert!(seen.iter().all(|s| !*s), "{seen:?}");
    }

    #[test]
    fn the_toggle_action_flips_the_setting() {
        let ctx = egui::Context::default();
        let mut state = crate::state::AppState::new();
        assert!(state.show_breadcrumb);
        dispatch(&mut state, Action::ToggleBreadcrumb, &ctx);
        assert!(!state.show_breadcrumb);
        dispatch(&mut state, Action::ToggleBreadcrumb, &ctx);
        assert!(state.show_breadcrumb);
    }

    #[test]
    fn a_document_without_an_outline_has_no_line() {
        let pdf = refdesk_core::testutil::PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "no outline");
            })
            .write_temp("crumb-none");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the layout", |h| h.tab(tab).ui.layout().is_some());
        h.frame(Vec::new());
        assert!(h.drawn.is_empty(), "{:?}", h.drawn);
    }
}
