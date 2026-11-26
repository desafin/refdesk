//! Keyboard link hints (LINK-13): every link in view gets a label of letters; typing a label
//! follows that link like a click (or like a middle click, in a new tab).
//!
//! The "Link Hints" command (dispatch) collects the links in view, labels them and sets
//! `AppState::key_grab`, so the shortcut table leaves the letters alone. The view then reads the
//! keys ([`input`]) and paints the labels ([`paint`]). Whatever ends the mode — a chosen link,
//! Esc, a letter no label has, scrolling, a click, two seconds without a key, the focus going
//! elsewhere, the tab closing — clears the view's session, and [`settle`] (run after each frame's
//! actions) takes the grab down with it.
//!
//! The decisions are plain functions on plain data, tested without egui (rule R-10): the labels
//! ([`labels`]), which links get one and in what order ([`visible_targets`]), and narrowing by
//! the typed letters ([`LinkHints::type_char`]).

use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Duration;

use egui::{Color32, FontId, Key, Sense, Stroke, StrokeKind, Ui, WidgetInfo};
use refdesk_core::{DocumentInfo, Link, PageLayout, Point, Rect, Size};

use crate::action::{Action, OpenTarget};
use crate::i18n::tr;
use crate::state::{AppState, KeyGrab, StatusNotice, Tab, TabId};
use crate::views::links::{self, ClickButton, ClickMods, LinkAction};
use crate::views::pdf_view::PdfViewUiState;

/// The letters of the labels, the ones nearest the resting hand first.
pub const ALPHABET: &str = "asdfghjklqwertyuiopzxcvbnm";
/// The mode ends after this long (seconds) without a key.
pub const IDLE_SECS: f64 = 2.0;
/// How long the "no links in view" notice stays (seconds).
pub const NONE_NOTICE_SECS: f64 = 3.0;

// ------------------------------------------------------------------------------------------------
// Labels
// ------------------------------------------------------------------------------------------------

/// The length of every label when there are `n` links: the fewest letters that give each its own.
pub fn label_len(n: usize) -> usize {
    let base = ALPHABET.chars().count();
    let mut len = 1;
    let mut count = base;
    while count < n {
        len += 1;
        count = count.saturating_mul(base);
    }
    len
}

/// The labels of `n` links, all of length [`label_len`], in [`ALPHABET`] order (the first link
/// gets "a", or "aa", and so on).
pub fn labels(n: usize) -> Vec<String> {
    let letters: Vec<char> = ALPHABET.chars().collect();
    let base = letters.len();
    let len = label_len(n);
    (0..n)
        .map(|i| {
            let mut digits = vec![0usize; len];
            let mut rest = i;
            for d in digits.iter_mut().rev() {
                *d = rest % base;
                rest /= base;
            }
            digits.into_iter().map(|d| letters[d]).collect()
        })
        .collect()
}

// ------------------------------------------------------------------------------------------------
// Which links
// ------------------------------------------------------------------------------------------------

/// A link that gets a label: its page, the index of its (first) piece in the page's list, and the
/// link with the rectangles of all its pieces.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// The page (zero-based).
    pub page: u32,
    /// The index in the links of the page.
    pub index: usize,
    /// The link.
    pub link: Link,
}

/// Whether `next` continues `prev` on the following line: the same target, starting below the
/// last line of `prev` within about a line's height.
fn continues(prev: &Link, next: &Link) -> bool {
    if prev.target != next.target {
        return false;
    }
    let (Some(last), Some(first)) = (prev.rects.last(), next.rects.first()) else {
        return false;
    };
    let line = last.height().max(first.height()).max(1.0);
    first.y0 >= last.y0 + line * 0.5 && first.y0 <= last.y1 + line * 1.5
}

/// The links of a page, with pieces that continue each other (a link broken across lines that
/// the document stores as two annotations) joined into one.
pub fn group(page: u32, links: &[Link]) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    for (index, link) in links.iter().enumerate() {
        if link.rects.is_empty() {
            continue;
        }
        if let Some(prev) = out.last_mut()
            && continues(&prev.link, link)
        {
            prev.link.rects.extend(link.rects.iter().copied());
            continue;
        }
        out.push(Target {
            page,
            index,
            link: link.clone(),
        });
    }
    out
}

/// Sorts targets into reading order: by page, then by line from the top, then from the left. Two
/// links whose first lines overlap by half a line height are on one line.
pub fn reading_order(targets: &mut [Target]) {
    let top = |t: &Target| t.link.rects.first().copied().unwrap_or_default();
    targets.sort_by(|a, b| {
        let (ra, rb) = (top(a), top(b));
        a.page
            .cmp(&b.page)
            .then(ra.y0.partial_cmp(&rb.y0).unwrap_or(Ordering::Equal))
            .then(ra.x0.partial_cmp(&rb.x0).unwrap_or(Ordering::Equal))
    });
    // Group into lines, then order each line from the left.
    let mut start = 0;
    while start < targets.len() {
        let first = top(&targets[start]);
        let page = targets[start].page;
        let half = first.height().max(1.0) * 0.5;
        let mut end = start + 1;
        while end < targets.len()
            && targets[end].page == page
            && (top(&targets[end]).y0 - first.y0).abs() < half
        {
            end += 1;
        }
        targets[start..end]
            .sort_by(|a, b| top(a).x0.partial_cmp(&top(b).x0).unwrap_or(Ordering::Equal));
        start = end;
    }
}

/// The links in view, in reading order: those of `pages` (page, links) with a piece that is at
/// least partly inside a viewport of size `viewport` scrolled to `scroll` in `layout`.
pub fn visible_targets(
    pages: &[(u32, Arc<[Link]>)],
    layout: &PageLayout,
    scroll: Point,
    viewport: Size,
) -> Vec<Target> {
    let view = Rect::new(0.0, 0.0, viewport.w, viewport.h);
    let mut out: Vec<Target> = pages
        .iter()
        .filter(|(page, _)| *page < layout.page_count())
        .flat_map(|(page, links)| group(*page, links))
        .filter(|t| {
            links::screen_rects(layout, scroll, t.page, &t.link.rects)
                .iter()
                .any(|r| r.intersects(&view))
        })
        .collect();
    reading_order(&mut out);
    out
}

// ------------------------------------------------------------------------------------------------
// The session of one view
// ------------------------------------------------------------------------------------------------

/// A labelled link.
#[derive(Clone, Debug, PartialEq)]
pub struct Hint {
    /// The letters to type.
    pub label: String,
    /// The link.
    pub target: Target,
    /// What the link says (the text of its tooltip), for the accessible name.
    pub name: String,
}

/// What a typed letter did.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Fewer labels match; more letters are needed.
    Narrowed,
    /// One label is left: follow it. The session is over.
    Chosen {
        /// The chosen link.
        hint: Hint,
        /// Open it in a new tab.
        new_tab: bool,
    },
    /// No label starts with the letters: the session is over.
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
struct Session {
    hints: Vec<Hint>,
    typed: String,
    new_tab: bool,
    last_key: f64,
}

/// The link hints of one PDF view. Not saved; a copy of a view starts without them.
#[derive(Debug, Default)]
pub struct LinkHints {
    session: Option<Session>,
}

impl Clone for LinkHints {
    fn clone(&self) -> LinkHints {
        LinkHints::default()
    }
}

impl LinkHints {
    /// Starts labelling `hints` at time `now` (seconds).
    pub fn begin(&mut self, hints: Vec<Hint>, new_tab: bool, now: f64) {
        self.session = Some(Session {
            hints,
            typed: String::new(),
            new_tab,
            last_key: now,
        });
    }

    /// Whether labels are shown.
    pub fn is_active(&self) -> bool {
        self.session.is_some()
    }

    /// The letters typed so far.
    pub fn typed(&self) -> &str {
        self.session.as_ref().map_or("", |s| s.typed.as_str())
    }

    /// Whether the chosen link opens in a new tab.
    #[cfg(test)]
    pub fn new_tab(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.new_tab)
    }

    /// The hints whose label starts with the typed letters, in reading order.
    pub fn matching(&self) -> impl Iterator<Item = &Hint> {
        self.session.iter().flat_map(|s| {
            s.hints
                .iter()
                .filter(move |h| h.label.starts_with(s.typed.as_str()))
        })
    }

    /// Ends the session.
    pub fn cancel(&mut self) {
        self.session = None;
    }

    /// Types the letter `c` at `now`: narrows the labels, follows the one that is left, or ends
    /// the session when none starts with the letters.
    pub fn type_char(&mut self, c: char, now: f64) -> Step {
        let Some(s) = &mut self.session else {
            return Step::Cancelled;
        };
        s.typed.push(c.to_ascii_lowercase());
        s.last_key = now;
        let mut left = s.hints.iter().filter(|h| h.label.starts_with(&s.typed));
        match (left.next(), left.next()) {
            (None, _) => {
                self.session = None;
                Step::Cancelled
            }
            (Some(hint), None) => {
                let step = Step::Chosen {
                    hint: hint.clone(),
                    new_tab: s.new_tab,
                };
                self.session = None;
                step
            }
            _ => Step::Narrowed,
        }
    }

    /// Takes back the last typed letter.
    pub fn backspace(&mut self, now: f64) {
        if let Some(s) = &mut self.session {
            s.typed.pop();
            s.last_key = now;
        }
    }

    /// Whether [`IDLE_SECS`] passed without a key by `now`.
    pub fn expired(&self, now: f64) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| now - s.last_key >= IDLE_SECS)
    }

    /// Time until the session expires; `None` without one.
    pub fn remaining(&self, now: f64) -> Option<Duration> {
        let s = self.session.as_ref()?;
        Some(Duration::from_secs_f64(
            (IDLE_SECS - (now - s.last_key)).max(0.0),
        ))
    }
}

// ------------------------------------------------------------------------------------------------
// Commands (called by `dispatch`)
// ------------------------------------------------------------------------------------------------

/// The labelled links in view of a PDF tab, or `None` when the tab is not laid out yet.
fn hints_in_view(state: &AppState, tab: TabId) -> Option<Vec<Hint>> {
    let pdf = state.tabs.pdf(tab)?;
    let doc = state.docs.get(pdf.doc)?;
    let info = doc.info()?;
    let layout = pdf.ui.layout()?;
    let pages: Vec<(u32, Arc<[Link]>)> = pdf
        .ui
        .visible_pages()
        .filter_map(|p| doc.page_links(p).map(|links| (p, links)))
        .collect();
    let targets = visible_targets(&pages, layout, pdf.ui.scroll(), pdf.ui.viewport());
    let names = labels(targets.len());
    Some(
        targets
            .into_iter()
            .zip(names)
            .map(|(target, label)| {
                let name = links::describe(
                    &target.link.target,
                    &info.page_sizes,
                    &info.page_labels,
                    &info.outline,
                )
                .text();
                Hint {
                    label,
                    target,
                    name,
                }
            })
            .collect(),
    )
}

/// "Link Hints": labels the links in view of a PDF tab and takes the keyboard for the letters.
/// Without links in view only a notice says so.
pub fn start(state: &mut AppState, tab: TabId, new_tab: bool, ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    let Some(hints) = hints_in_view(state, tab) else {
        return;
    };
    if hints.is_empty() {
        state.status_notice = Some(StatusNotice {
            text: tr("hints.none").to_owned(),
            until: now + NONE_NOTICE_SECS,
        });
        ctx.request_repaint();
        return;
    }
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        pdf.ui.links.hints.begin(hints, new_tab, now);
        state.key_grab = Some(KeyGrab::LinkHints { tab, new_tab });
    }
    ctx.request_repaint();
}

/// Called after each frame's actions: keeps `key_grab` and the sessions of the views in step. The
/// grab goes when its session ended, its tab is gone or no longer focused, or the time is up; a
/// session goes when the grab is not its own (another mode took the keyboard).
pub fn settle(state: &mut AppState, ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    if let Some(KeyGrab::LinkHints { tab, .. }) = state.key_grab {
        let alive = state.focus == Some(tab)
            && !ctx.text_edit_focused()
            && state
                .tabs
                .pdf(tab)
                .is_some_and(|p| p.ui.links.hints.is_active() && !p.ui.links.hints.expired(now));
        if !alive {
            state.key_grab = None;
        }
    }
    let owner = match state.key_grab {
        Some(KeyGrab::LinkHints { tab, .. }) => Some(tab),
        _ => None,
    };
    let orphans: Vec<TabId> = state
        .tabs
        .iter()
        .filter(|(id, tab)| {
            matches!(tab, Tab::Pdf(p) if p.ui.links.hints.is_active()) && Some(*id) != owner
        })
        .map(|(id, _)| id)
        .collect();
    for id in orphans {
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(id) {
            pdf.ui.links.hints.cancel();
        }
    }
}

// ------------------------------------------------------------------------------------------------
// egui
// ------------------------------------------------------------------------------------------------

/// The letter keys and the letters they type.
const LETTERS: [(Key, char); 26] = [
    (Key::A, 'a'),
    (Key::B, 'b'),
    (Key::C, 'c'),
    (Key::D, 'd'),
    (Key::E, 'e'),
    (Key::F, 'f'),
    (Key::G, 'g'),
    (Key::H, 'h'),
    (Key::I, 'i'),
    (Key::J, 'j'),
    (Key::K, 'k'),
    (Key::L, 'l'),
    (Key::M, 'm'),
    (Key::N, 'n'),
    (Key::O, 'o'),
    (Key::P, 'p'),
    (Key::Q, 'q'),
    (Key::R, 'r'),
    (Key::S, 's'),
    (Key::T, 't'),
    (Key::U, 'u'),
    (Key::V, 'v'),
    (Key::W, 'w'),
    (Key::X, 'x'),
    (Key::Y, 'y'),
    (Key::Z, 'z'),
];

fn letter(key: Key) -> Option<char> {
    LETTERS.iter().find(|(k, _)| *k == key).map(|(_, c)| *c)
}

/// Whether the session takes this key event for itself (it is removed from the input).
fn is_hint_key(event: &egui::Event) -> bool {
    match event {
        egui::Event::Key { key, .. } => {
            letter(*key).is_some() || matches!(key, Key::Escape | Key::Backspace)
        }
        egui::Event::Text(_) => true,
        _ => false,
    }
}

/// Reads this frame's keys for the link hints of the view, when it shows them: letters narrow
/// and choose, Backspace takes one back; Esc, another key, the wheel, a click, the view losing
/// the focus, a text field taking the keyboard, or [`IDLE_SECS`] without a key end the session.
/// A chosen link is followed the way a click (or a middle click, in new-tab mode, or with Shift
/// on the last letter) follows it.
pub fn input(
    ui: &Ui,
    state: &mut PdfViewUiState,
    info: &DocumentInfo,
    tab: TabId,
    focused: bool,
    queue: &mut Vec<Action>,
) {
    let hints = &mut state.links.hints;
    if !hints.is_active() {
        return;
    }
    let now = ui.input(|i| i.time);
    if !focused || ui.ctx().text_edit_focused() || hints.expired(now) {
        hints.cancel();
        return;
    }
    let events = ui.input(|i| i.events.clone());
    let mut chosen = None;
    for event in &events {
        match event {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => match (*key, letter(*key)) {
                (Key::Escape, _) => hints.cancel(),
                (Key::Backspace, _) => hints.backspace(now),
                (_, Some(c)) if !(modifiers.ctrl || modifiers.alt || modifiers.command) => {
                    if let Step::Chosen { hint, new_tab } = hints.type_char(c, now) {
                        chosen = Some((hint, new_tab || modifiers.shift));
                    }
                }
                _ => hints.cancel(),
            },
            egui::Event::MouseWheel { .. } | egui::Event::PointerButton { pressed: true, .. } => {
                hints.cancel();
            }
            _ => {}
        }
        if !hints.is_active() {
            break;
        }
    }
    ui.input_mut(|i| i.events.retain(|e| !is_hint_key(e)));
    if let Some((hint, new_tab)) = chosen {
        follow(ui, &hint, new_tab, &info.page_sizes, tab, queue);
    }
    if let Some(left) = state.links.hints.remaining(now) {
        ui.ctx().request_repaint_after(left);
    }
}

/// Follows a chosen link the way a click does: a jump in this view, or a new tab for a middle
/// click; an address opens in the browser, another document as a tab.
fn follow(
    ui: &Ui,
    hint: &Hint,
    new_tab: bool,
    page_sizes: &[Size],
    tab: TabId,
    queue: &mut Vec<Action>,
) {
    let button = if new_tab {
        ClickButton::Middle
    } else {
        ClickButton::Primary
    };
    match links::follow(
        &hint.target.link.target,
        button,
        ClickMods::default(),
        page_sizes,
    ) {
        LinkAction::Navigate { to, how } => queue.push(Action::Navigate { tab, to, how }),
        LinkAction::OpenUrl(url) => ui.ctx().open_url(egui::OpenUrl::new_tab(url)),
        LinkAction::OpenFile { path, page } => {
            queue.push(Action::OpenFiles(vec![OpenTarget { path, page }]));
        }
        LinkAction::Blocked(url) => log::info!("not opening link {url}"),
    }
}

/// The accessible name of a label: "Link hint as: Page 12 — 7.3 State management".
pub fn accessible_name(hint: &Hint) -> String {
    tr("hints.a11y")
        .replace("{label}", &hint.label)
        .replace("{target}", &hint.name)
}

/// Paints the labels that still match at the top-left corner of their links: dark boxes with
/// light letters, the typed part in another colour. Each label is an accessible node.
pub fn paint(ui: &Ui, view_rect: egui::Rect, state: &PdfViewUiState, tab: TabId) {
    let hints = &state.links.hints;
    let Some(layout) = state.layout() else {
        return;
    };
    if !hints.is_active() {
        return;
    }
    let typed = hints.typed();
    let painter = ui.painter_at(view_rect);
    let font = FontId::monospace(12.0);
    let fill = Color32::from_rgba_unmultiplied(24, 24, 28, 235);
    let text = Color32::from_rgb(255, 230, 120);
    let done = Color32::from_rgb(130, 200, 255);
    let pad = egui::vec2(3.0, 1.0);
    for (i, hint) in hints.matching().enumerate() {
        let Some(first) = links::screen_rects(
            layout,
            state.scroll(),
            hint.target.page,
            &hint.target.link.rects,
        )
        .into_iter()
        .next() else {
            continue;
        };
        let mut job = egui::text::LayoutJob::default();
        let (head, tail) = hint.label.split_at(typed.len().min(hint.label.len()));
        for (part, color) in [(head, done), (tail, text)] {
            job.append(
                &part.to_uppercase(),
                0.0,
                egui::TextFormat::simple(font.clone(), color),
            );
        }
        let galley = painter.layout_job(job);
        let size = galley.size() + pad * 2.0;
        let at = view_rect.min + egui::vec2(first.x0, first.y0) - egui::vec2(2.0, 2.0);
        let min = egui::pos2(
            at.x.clamp(
                view_rect.min.x,
                (view_rect.max.x - size.x).max(view_rect.min.x),
            ),
            at.y.clamp(
                view_rect.min.y,
                (view_rect.max.y - size.y).max(view_rect.min.y),
            ),
        );
        let rect = egui::Rect::from_min_size(min, size);
        painter.rect_filled(rect, 3.0, fill);
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.0, Color32::from_black_alpha(200)),
            StrokeKind::Outside,
        );
        painter.galley(rect.min + pad, galley, text);
        let response = ui.interact(
            rect,
            ui.scope_id().with(("link_hint", tab, i)),
            Sense::hover(),
        );
        let name = accessible_name(hint);
        response.widget_info(|| WidgetInfo::labeled(egui::accesskit::Role::Label, true, &name));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use refdesk_core::{DestKind, Destination, LayoutParams, LinkTarget, ZoomMode};

    use crate::views::pdf_view::{MARGIN, PAGE_GAP};

    #[test]
    fn labels_have_the_fewest_letters_and_follow_the_alphabet() {
        assert_eq!(labels(1), vec!["a"]);
        let l26 = labels(26);
        assert!(l26.iter().all(|l| l.len() == 1));
        assert_eq!(l26.concat(), ALPHABET);
        let l27 = labels(27);
        assert!(l27.iter().all(|l| l.len() == 2));
        assert_eq!(&l27[..3], &["aa", "as", "ad"]);
        assert_eq!(l27[26], "sa");
        let l100 = labels(100);
        assert!(l100.iter().all(|l| l.len() == 2));
        assert_eq!(l100[0], "aa");
        // 26 * 3 + 21 = 99: the fourth letter, then the 22nd.
        assert_eq!(l100[99], "fc");
        for l in [&l26, &l27, &l100] {
            let mut sorted = (*l).clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), l.len(), "labels are distinct");
            // No label is the start of another (they all have the same length).
            assert!(
                l.iter()
                    .all(|a| l.iter().all(|b| a == b || !b.starts_with(a.as_str())))
            );
        }
        assert_eq!(label_len(676), 2);
        assert_eq!(label_len(677), 3);
        assert!(labels(0).is_empty());
    }

    fn internal(page: u32) -> LinkTarget {
        LinkTarget::Internal(Destination {
            page,
            kind: DestKind::Xyz {
                left: Some(0.0),
                top: Some(0.0),
            },
        })
    }

    fn link(rect: Rect, page: u32) -> Link {
        Link {
            rects: vec![rect],
            target: internal(page),
        }
    }

    #[test]
    fn two_pieces_of_one_link_on_consecutive_lines_get_one_label() {
        let links = [
            link(Rect::new(400.0, 100.0, 550.0, 112.0), 5),
            link(Rect::new(72.0, 114.0, 150.0, 126.0), 5),
            // The same target again, but far below: another link.
            link(Rect::new(72.0, 400.0, 150.0, 412.0), 5),
            // Right after, another target.
            link(Rect::new(72.0, 414.0, 150.0, 426.0), 6),
        ];
        let g = group(0, &links);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].link.rects.len(), 2);
        assert_eq!((g[0].index, g[1].index, g[2].index), (0, 2, 3));
    }

    #[test]
    fn targets_are_in_reading_order() {
        let mut t: Vec<Target> = [
            (1, Rect::new(72.0, 50.0, 100.0, 62.0)),
            (0, Rect::new(300.0, 100.5, 350.0, 112.0)),
            (0, Rect::new(72.0, 101.0, 150.0, 113.0)),
            (0, Rect::new(72.0, 30.0, 150.0, 42.0)),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (page, r))| Target {
            page,
            index: i,
            link: link(r, 9),
        })
        .collect();
        reading_order(&mut t);
        let order: Vec<usize> = t.iter().map(|t| t.index).collect();
        // Page 1: the top line, then the line at y ≈ 100 from the left; then page 2.
        assert_eq!(order, vec![3, 2, 1, 0]);
    }

    #[test]
    fn only_links_in_view_get_labels() {
        let sizes = vec![Size::new(595.0, 842.0); 3];
        let layout = PageLayout::new(
            &sizes,
            LayoutParams {
                viewport: Size::new(800.0, 600.0),
                zoom: ZoomMode::Custom(1.0),
                gap: PAGE_GAP,
                margin: MARGIN,
                fit_page: 0,
            },
        );
        let page0: Arc<[Link]> = vec![
            link(Rect::new(72.0, 100.0, 150.0, 112.0), 2),
            // Below the bottom of the view at 100%: 800 pt is about 1067 px.
            link(Rect::new(72.0, 800.0, 150.0, 812.0), 2),
        ]
        .into();
        let t = visible_targets(
            &[(0, page0)],
            &layout,
            Point::new(0.0, 0.0),
            Size::new(800.0, 600.0),
        );
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].index, 0);
    }

    pub(crate) fn hint(label: &str) -> Hint {
        Hint {
            label: label.to_owned(),
            target: Target {
                page: 0,
                index: 0,
                link: link(Rect::new(0.0, 0.0, 10.0, 10.0), 1),
            },
            name: format!("link {label}"),
        }
    }

    #[test]
    fn typing_narrows_and_the_last_one_left_is_chosen() {
        let mut h = LinkHints::default();
        h.begin(labels(30).iter().map(|l| hint(l)).collect(), false, 10.0);
        assert!(h.is_active());
        assert_eq!(h.matching().count(), 30);
        assert_eq!(h.type_char('a', 10.5), Step::Narrowed);
        assert_eq!(h.matching().count(), 26);
        assert_eq!(h.typed(), "a");
        // Backspace takes it back.
        h.backspace(10.6);
        assert_eq!(h.typed(), "");
        assert_eq!(h.matching().count(), 30);
        // "s" leaves "sa" .. "sf": four.
        assert_eq!(h.type_char('S', 10.7), Step::Narrowed);
        assert_eq!(h.matching().count(), 4);
        match h.type_char('d', 10.8) {
            Step::Chosen { hint, new_tab } => {
                assert_eq!(hint.label, "sd");
                assert!(!new_tab);
            }
            other => panic!("{other:?}"),
        }
        assert!(!h.is_active());

        // A letter no label has ends the session.
        h.begin(labels(3).iter().map(|l| hint(l)).collect(), true, 20.0);
        assert!(h.new_tab());
        assert_eq!(h.type_char('z', 20.1), Step::Cancelled);
        assert!(!h.is_active());

        // A single letter label is chosen at once, in new-tab mode it says so.
        h.begin(labels(3).iter().map(|l| hint(l)).collect(), true, 20.0);
        assert!(matches!(
            h.type_char('d', 20.1),
            Step::Chosen { new_tab: true, .. }
        ));
    }

    #[test]
    fn the_new_commands_have_their_keys_and_are_in_the_menus() {
        use crate::commands::{COMMANDS, MenuGroup, command, in_menu};
        use crate::i18n::{Lang, lookup};
        use egui::{KeyboardShortcut, Modifiers};
        for (id, menu, shortcut) in [
            (
                "view.zoom_region",
                MenuGroup::View,
                Some((Modifiers::NONE, Key::Z)),
            ),
            (
                "view.zoom_back",
                MenuGroup::View,
                Some((Modifiers::SHIFT, Key::Z)),
            ),
            (
                "nav.link_hints",
                MenuGroup::Go,
                Some((Modifiers::NONE, Key::F)),
            ),
            (
                "nav.link_hints_new_tab",
                MenuGroup::Go,
                Some((Modifiers::SHIFT, Key::F)),
            ),
            ("view.close_pinned", MenuGroup::View, None),
        ] {
            let c = command(id).expect(id);
            assert_eq!(c.menu, menu, "{id}");
            assert!(in_menu(menu).any(|m| m.id == id), "{id}");
            assert_eq!(
                c.shortcut,
                shortcut.map(|(m, k)| KeyboardShortcut::new(m, k)),
                "{id}"
            );
            for lang in [Lang::Ko, Lang::En] {
                assert!(lookup(lang, c.name_key).is_some(), "{id} {lang:?}");
            }
            // No other command has the same key.
            if let Some(s) = c.shortcut {
                let users = COMMANDS
                    .iter()
                    .filter(|o| o.all_shortcuts().any(|x| x == s))
                    .count();
                assert_eq!(users, 1, "{id}");
            }
        }
        for key in [
            "zoom_region.hint",
            "hints.none",
            "hints.a11y",
            "pin.button",
            "pin.window",
            "pin.go",
            "pin.close",
            "pin.resize",
        ] {
            assert!(
                lookup(Lang::Ko, key).is_some() && lookup(Lang::En, key).is_some(),
                "{key}"
            );
        }
    }

    #[test]
    fn the_session_expires_two_seconds_after_the_last_key() {
        let mut h = LinkHints::default();
        h.begin(labels(30).iter().map(|l| hint(l)).collect(), false, 10.0);
        assert!(!h.expired(11.9));
        h.type_char('a', 11.9);
        assert!(!h.expired(13.8));
        assert!(h.expired(13.9));
        let left = h.remaining(12.4).expect("active");
        assert!((left.as_secs_f64() - 1.5).abs() < 1e-9);
        // A copy of a view does not take the session along.
        assert!(!h.clone().is_active());
    }
}

/// Link hints in the drawn view, frame by frame, with the shortcut table running.
#[cfg(test)]
mod widget_tests {
    use super::*;
    use egui::Modifiers;
    use egui::accesskit::Role;
    use refdesk_core::testutil::{PdfBuilder, TempPdf};

    use crate::action::dispatch;
    use crate::overlays::link_preview::tests::{Harness, opened_document};

    /// The link preview's document (page 1 links to page 3 and to an address), the shortcut
    /// table and the accessibility tree on.
    fn rig(stem: &str) -> (Harness, TabId, TempPdf) {
        let (mut h, tab, pdf) = opened_document(stem);
        h.shortcuts = true;
        h.accesskit = true;
        h.frame_at(10.0, Vec::new());
        (h, tab, pdf)
    }

    fn hints(h: &Harness, tab: TabId) -> &LinkHints {
        &h.tab(tab).ui.links.hints
    }

    fn page(h: &Harness, tab: TabId) -> u32 {
        h.tab(tab).view.anchor.page
    }

    #[test]
    fn f_labels_the_links_in_view_and_takes_the_keyboard() {
        let (mut h, tab, _pdf) = rig("hints-show");
        assert_eq!(h.state.key_grab, None);
        h.key_at(10.1, Key::F, Modifiers::NONE);
        assert_eq!(
            h.state.key_grab,
            Some(KeyGrab::LinkHints {
                tab,
                new_tab: false
            })
        );
        let labels: Vec<String> = hints(&h, tab).matching().map(|x| x.label.clone()).collect();
        assert_eq!(
            labels,
            vec!["a", "s"],
            "reading order: the page link, then the address"
        );
        // The labels are drawn and each is an accessible node named after its link.
        h.frame_at(10.2, Vec::new());
        let title = crate::i18n::tr("link.page_title")
            .replace("{page}", "3")
            .replace("{title}", "7.3.2 Transitions");
        let name = tr("hints.a11y")
            .replace("{label}", "a")
            .replace("{target}", &title);
        assert!(h.has_node(Role::Label, &name), "{:?}", h.nodes);
        let url = tr("hints.a11y")
            .replace("{label}", "s")
            .replace("{target}", "https://example.com/");
        assert!(h.has_node(Role::Label, &url), "{:?}", h.nodes);
        assert!(h.drew("A") && h.drew("S"), "{:?}", h.drawn);
    }

    #[test]
    fn typing_a_label_follows_the_link_and_back_returns() {
        let (mut h, tab, _pdf) = rig("hints-follow");
        h.key_at(10.1, Key::F, Modifiers::NONE);
        h.key_at(10.5, Key::A, Modifiers::NONE);
        h.frame_at(10.6, Vec::new());
        assert_eq!(page(&h, tab), 2, "the link's destination");
        assert_eq!(h.state.key_grab, None);
        assert!(!hints(&h, tab).is_active());
        assert_eq!(h.state.tabs.len(), 1);
        // Back with the keyboard, now that the shortcuts are free again.
        h.key_at(11.0, Key::ArrowLeft, Modifiers::ALT);
        h.frame_at(11.1, Vec::new());
        assert_eq!(page(&h, tab), 0);
    }

    #[test]
    fn the_label_of_an_address_opens_it_in_the_browser() {
        let (mut h, tab, _pdf) = rig("hints-url");
        h.key_at(10.1, Key::F, Modifiers::NONE);
        h.key_at(10.2, Key::S, Modifiers::NONE);
        assert_eq!(h.opened, vec!["https://example.com/".to_owned()]);
        assert_eq!(page(&h, tab), 0);
        assert_eq!(h.state.key_grab, None);
    }

    #[test]
    fn shift_f_opens_the_chosen_link_in_a_new_tab() {
        let (mut h, tab, _pdf) = rig("hints-new-tab");
        h.key_at(10.1, Key::F, Modifiers::SHIFT);
        assert_eq!(
            h.state.key_grab,
            Some(KeyGrab::LinkHints { tab, new_tab: true })
        );
        h.key_at(10.2, Key::A, Modifiers::NONE);
        h.frame_at(10.3, Vec::new());
        assert_eq!(h.state.tabs.len(), 2);
        let new = h.state.focus.expect("focus");
        assert_ne!(new, tab);
        assert_eq!(page(&h, new), 2);
        assert_eq!(page(&h, tab), 0, "the asking view stays");
        assert_eq!(h.state.key_grab, None);
    }

    #[test]
    fn every_way_out_takes_the_grab_down() {
        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -40.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Modifiers::NONE,
        };
        let press = |pos| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        };
        let esc = |pressed| egui::Event::Key {
            key: Key::Escape,
            physical_key: Some(Key::Escape),
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        let cases: Vec<(&str, Vec<egui::Event>)> = vec![
            ("esc", vec![esc(true), esc(false)]),
            (
                "wheel",
                vec![egui::Event::PointerMoved(egui::pos2(400.0, 300.0)), wheel],
            ),
            ("click", vec![press(egui::pos2(400.0, 500.0))]),
        ];
        for (name, events) in cases {
            let (mut h, tab, _pdf) = rig(&format!("hints-end-{name}"));
            h.key_at(10.1, Key::F, Modifiers::NONE);
            assert!(h.state.key_grab.is_some(), "{name}");
            h.frame_at(10.2, events);
            assert_eq!(h.state.key_grab, None, "{name}");
            assert!(!hints(&h, tab).is_active(), "{name}");
            assert_eq!(page(&h, tab), 0, "{name}: nothing was followed");
        }

        // Two seconds without a key; the clock is moved by hand.
        let (mut h, tab, _pdf) = rig("hints-end-idle");
        h.key_at(10.1, Key::F, Modifiers::NONE);
        h.frame_at(11.9, Vec::new());
        assert!(h.state.key_grab.is_some(), "still up at 1.8 s");
        h.frame_at(12.2, Vec::new());
        assert_eq!(h.state.key_grab, None);
        assert!(!hints(&h, tab).is_active());

        // The tab closes.
        let (mut h, tab, _pdf) = rig("hints-end-close");
        h.key_at(10.1, Key::F, Modifiers::NONE);
        h.state.actions.push(Action::CloseTab(tab));
        h.frame_at(10.2, Vec::new());
        assert!(h.state.tabs.pdf(tab).is_none());
        assert_eq!(h.state.key_grab, None);

        // The focus goes to another tab.
        let (mut h, tab, _pdf) = rig("hints-end-focus");
        h.key_at(10.1, Key::F, Modifiers::NONE);
        let other = crate::overlays::link_preview::tests::document("hints-end-focus-2");
        let second = h.open(other.path());
        h.frame_at(10.2, Vec::new());
        assert_eq!(h.state.focus, Some(second));
        assert_eq!(h.state.key_grab, None);
        assert!(!hints(&h, tab).is_active());
    }

    #[test]
    fn letters_typed_while_labels_are_shown_run_no_command() {
        let (mut h, tab, _pdf) = rig("hints-no-command");
        h.key_at(10.1, Key::F, Modifiers::NONE);
        // Z is "Zoom to Area" otherwise; here it is a letter no label has.
        h.key_at(10.2, Key::Z, Modifiers::NONE);
        h.frame_at(10.3, Vec::new());
        assert!(!h.tab(tab).ui.region_zoom.is_active());
        assert_eq!(h.state.key_grab, None);
        assert!(!hints(&h, tab).is_active());
        // F again while shown: no new session, the letter ends this one.
        h.key_at(10.4, Key::F, Modifiers::NONE);
        assert!(hints(&h, tab).is_active());
        h.key_at(10.5, Key::F, Modifiers::NONE);
        assert!(!hints(&h, tab).is_active());
        assert_eq!(h.state.key_grab, None);
        // And once over, Z works again.
        h.key_at(10.6, Key::Z, Modifiers::NONE);
        assert!(h.tab(tab).ui.region_zoom.is_active());
    }

    /// A page with 30 links in two columns of 15 rows; link `i` (row-major, which is reading
    /// order) leads to page 2 at y = 10 + 10 i.
    fn many_links(stem: &str) -> TempPdf {
        PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                for i in 0..30u32 {
                    let (row, col) = (i / 2, i % 2);
                    let x = 72.0 + col as f32 * 250.0;
                    let y = 60.0 + row as f32 * 20.0;
                    p.link_to(
                        (x, y, x + 180.0, y + 12.0),
                        2,
                        Some((72.0, 10.0 + 10.0 * i as f32)),
                    );
                }
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "target page");
            })
            .write_temp(stem)
    }

    #[test]
    fn with_thirty_links_two_letters_choose_and_backspace_takes_one_back() {
        let pdf = many_links("hints-many");
        let mut h = Harness::new();
        h.shortcuts = true;
        let tab = h.open(pdf.path());
        h.frames_until("the links", |h| {
            let doc = h.state.docs.get(h.tab(tab).doc).expect("doc");
            h.tab(tab).ui.layout().is_some() && doc.page_links(0).is_some()
        });
        h.frame_at(10.0, Vec::new());
        h.key_at(10.1, Key::F, Modifiers::NONE);
        assert_eq!(hints(&h, tab).matching().count(), 30);
        assert!(hints(&h, tab).matching().all(|x| x.label.len() == 2));
        h.key_at(10.2, Key::S, Modifiers::NONE);
        assert_eq!(hints(&h, tab).matching().count(), 4, "sa, ss, sd, sf");
        h.key_at(10.3, Key::Backspace, Modifiers::NONE);
        assert_eq!(hints(&h, tab).typed(), "");
        assert_eq!(hints(&h, tab).matching().count(), 30);
        h.key_at(10.4, Key::S, Modifiers::NONE);
        h.key_at(10.5, Key::D, Modifiers::NONE);
        h.frame_at(10.6, Vec::new());
        // "sd" is the 29th label: link 28.
        let at = h.tab(tab).view.anchor;
        assert_eq!(at.page, 1);
        assert!((at.y - 290.0).abs() < 0.5, "{at:?}");
        assert_eq!(h.state.key_grab, None);
    }

    #[test]
    fn without_links_in_view_only_a_notice_says_so() {
        let pdf = PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "no links here");
            })
            .write_temp("hints-none");
        let mut h = Harness::new();
        h.shortcuts = true;
        let tab = h.open(pdf.path());
        h.frames_until("the links", |h| {
            let doc = h.state.docs.get(h.tab(tab).doc).expect("doc");
            h.tab(tab).ui.layout().is_some() && doc.page_links(0).is_some()
        });
        h.key_at(10.0, Key::F, Modifiers::NONE);
        assert_eq!(h.state.key_grab, None);
        assert!(!hints(&h, tab).is_active());
        assert_eq!(
            h.state.status_notice.as_ref().map(|n| n.text.as_str()),
            Some(tr("hints.none"))
        );
        // The command, too.
        dispatch(
            &mut h.state,
            Action::ShowLinkHints {
                tab,
                new_tab: false,
            },
            &h.ctx,
        );
        assert_eq!(h.state.key_grab, None);
    }
}
