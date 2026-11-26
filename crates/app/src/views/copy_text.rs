//! Copying places and sections as text (LINK-15, SEL-5).
//!
//! * The place a reader is at, as `path#page=N` — the form `refdesk file.pdf#page=N` opens
//!   ([`location_text`]) — and as a citation line, "file.pdf, p. 87 — 7 Functional specification
//!   › 7.3 State management" ([`citation_text`]).
//! * The text of a section or of a whole chapter: the range comes from the table of contents
//!   (`refdesk_core::scope_choices`, the same ranges the search bar offers), the text is made
//!   with the rules of a selection copy ([`COPY_OPTIONS`]). The pages may not have their text
//!   extracted yet, so a background thread makes it and leaves it in an [`Inbox`]; the next
//!   frame puts it on the clipboard and tells the user how much was copied ([`finish_pending`]).
//!
//! What is decided is plain code, tested without a window (rule R-10); the functions that take
//! the application state only look things up and call those.

use std::path::Path;
use std::sync::{Arc, Mutex};

use refdesk_core::{
    DocumentHandle, Location, Outline, PageText, ScopeChoice, SearchScope, Selection, TextPos,
    scope_choices, selection_text,
};

use crate::action::{Action, dispatch};
use crate::i18n::tr;
use crate::panels::breadcrumb::SEPARATOR;
use crate::panels::toc::reading_location;
use crate::state::{AppState, PdfTab, TabId};
use crate::views::links::page_label;
use crate::views::selection::COPY_OPTIONS;

/// A range of more than this many pages is copied all the same, and the notice says how many
/// pages it was.
pub const LONG_RANGE_PAGES: u32 = 200;

// ------------------------------------------------------------------------------------------------
// The place as text
// ------------------------------------------------------------------------------------------------

/// The place the section path line shows: the quarter-height line of the viewport of `pdf`
/// ([`reading_location`]), the one the table of contents panel marks as the current section.
pub fn reading_place(pdf: &PdfTab) -> Location {
    reading_location(pdf.view.anchor, pdf.ui.layout(), pdf.ui.viewport().h)
}

/// The place a copy of "where I am" refers to: where the selection starts when there is one
/// (`text_of` gives the text of a page if it is there already, to find the height of the first
/// selected character), else [`reading_place`].
pub fn place_of(pdf: &PdfTab, text_of: impl Fn(u32) -> Option<Arc<PageText>>) -> Location {
    let Some(sel) = pdf.ui.selection.active() else {
        return reading_place(pdf);
    };
    let (start, _) = sel.ordered();
    // The bottom edge of the character, like the places of search matches and sections.
    let (x, y) = text_of(start.page)
        .and_then(|text| {
            text.chars
                .get(start.ch as usize)
                .or_else(|| text.chars.last())
                .map(|c| c.quad.bounds())
        })
        .map_or((0.0, 0.0), |b| (b.x0, b.y1));
    Location::new(start.page, x, y)
}

/// `path#page=N` for the zero-based `page`: the argument that opens the document at that page
/// (`cli::parse` reads it back). The path is written as it is, without quoting, so it may hold
/// blanks, `#` and any text. A path that is not valid UTF-8 cannot be written; its invalid
/// bytes become U+FFFD and the text does not open that file.
pub fn location_text(path: &Path, page: u32) -> String {
    format!("{}#page={}", path.to_string_lossy(), u64::from(page) + 1)
}

/// The words of a title on one line.
fn one_line(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `file name, p. label — section › section`. The section path is left out when it is empty.
pub fn citation_text(file_name: &str, page_label: &str, section_path: &[&str]) -> String {
    let mut out = format!("{file_name}, p. {page_label}");
    let path: Vec<String> = section_path.iter().map(|t| one_line(t)).collect();
    if !path.is_empty() {
        out.push_str(" — ");
        out.push_str(&path.join(&format!(" {SEPARATOR} ")));
    }
    out
}

// ------------------------------------------------------------------------------------------------
// The text of a range
// ------------------------------------------------------------------------------------------------

/// Which part of the section path a range copy takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The deepest section the place is in.
    Section,
    /// The top-level chapter the place is in.
    Chapter,
}

/// The range of the section or the chapter at `at`: the last, or the first, step of
/// `scope_choices` after "whole document". `None` before the first section and without an outline.
pub fn choice_at(
    outline: &Outline,
    at: Location,
    page_count: u32,
    part: Part,
) -> Option<ScopeChoice> {
    let mut steps = scope_choices(outline, at, page_count).into_iter().skip(1);
    match part {
        Part::Chapter => steps.next(),
        Part::Section => steps.last(),
    }
}

/// Where in the text of a page the height `y` falls: the first character of the first line (in
/// reading order) whose bottom edge is at `y` or below, or the end of the text when there is no
/// such line.
fn char_at_height(text: &PageText, y: f32) -> u32 {
    text.lines
        .iter()
        .find(|line| line.bounds.y1 >= y)
        .map_or(text.chars.len() as u32, |line| line.chars.start)
}

/// The selection that covers `scope` in a document of `page_count` pages, with the ends cut where
/// the first and the last page of the scope reach the heights of its places. `texts` gives the
/// text of a page. `None` when the scope holds no page.
///
/// The cut is made along the reading order, so on a page of several columns the text of the
/// columns that come before a heading in reading order but lie below it is cut as well.
pub fn scope_selection(
    scope: &SearchScope,
    page_count: u32,
    texts: impl Fn(u32) -> Option<Arc<PageText>>,
) -> Option<Selection> {
    let pages = scope.pages(page_count);
    if pages.is_empty() {
        return None;
    }
    let (first, last) = (pages.start, pages.end - 1);
    let start_ch = texts(first).map_or(0, |t| char_at_height(&t, scope.start.y));
    // An end past the last page of the scope (the end of the document, or the top of the page
    // after it) takes the last page whole.
    let end_ch = if scope.end.page > last {
        u32::MAX
    } else {
        texts(last).map_or(u32::MAX, |t| char_at_height(&t, scope.end.y))
    };
    let end_ch = if first == last {
        end_ch.max(start_ch)
    } else {
        end_ch
    };
    Some(Selection {
        anchor: TextPos {
            page: first,
            ch: start_ch,
        },
        focus: TextPos {
            page: last,
            ch: end_ch,
        },
    })
}

/// The text of `scope`, made like a selection copy.
pub fn scope_text(
    scope: &SearchScope,
    page_count: u32,
    texts: impl Fn(u32) -> Option<Arc<PageText>>,
) -> String {
    match scope_selection(scope, page_count, &texts) {
        Some(sel) => selection_text(&sel, &texts, COPY_OPTIONS),
        None => String::new(),
    }
}

/// A number with a comma between the thousands: 12345 → "12,345".
pub fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The notice after a range copy: "title — 12,345 characters copied", with the number of pages
/// when the range was longer than [`LONG_RANGE_PAGES`].
pub fn copied_notice(title: &str, chars: usize, pages: u32) -> String {
    let key = if pages > LONG_RANGE_PAGES {
        "status.range_copied_pages"
    } else {
        "status.range_copied"
    };
    tr(key)
        .replace("{title}", &one_line(title))
        .replace("{chars}", &group_digits(chars))
        .replace("{pages}", &pages.to_string())
}

// ------------------------------------------------------------------------------------------------
// The background copy
// ------------------------------------------------------------------------------------------------

/// A range copy that the background thread has finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The title of the section.
    pub title: String,
    /// The text; empty when the range holds no text.
    pub text: String,
    /// How many pages the range covers.
    pub pages: u32,
}

/// Where the copy threads leave what they made, for [`finish_pending`] to take. Shared between
/// the threads and the application.
#[derive(Clone, Debug, Default)]
pub struct Inbox(Arc<Mutex<Vec<Finished>>>);

impl Inbox {
    fn post(&self, done: Finished) {
        if let Ok(mut list) = self.0.lock() {
            list.push(done);
        }
    }

    /// Takes everything that is there, oldest first.
    pub fn take(&self) -> Vec<Finished> {
        self.0
            .lock()
            .map(|mut list| std::mem::take(&mut *list))
            .unwrap_or_default()
    }
}

/// Makes the text of the range in `choice` on a background thread (the pages whose text is not
/// extracted yet are waited for there, rule R-1) and posts it to `inbox`, then asks for a new
/// frame. A document that was reloaded meanwhile makes nothing (R-7). Returns false when
/// nothing was started: the document is not open, the choice has no range, or the thread could
/// not be created.
pub fn start_range_copy(
    doc: &DocumentHandle,
    choice: &ScopeChoice,
    inbox: Inbox,
    ctx: egui::Context,
) -> bool {
    let (Some(scope), Some(info)) = (choice.scope, doc.info()) else {
        return false;
    };
    let page_count = u32::try_from(info.page_sizes.len()).unwrap_or(u32::MAX);
    let generation = info.generation;
    let title = choice.title.clone();
    let doc = doc.clone();
    let spawned = std::thread::Builder::new()
        .name("copy-range".into())
        .spawn(move || {
            let pages = scope.pages(page_count);
            let mut have: Vec<(u32, Option<Arc<PageText>>)> = Vec::new();
            for page in pages.clone() {
                match doc.page_text_wait(page) {
                    Ok(text) => have.push((page, Some(text))),
                    Err(e) => log::warn!("copy: no text for page {page}: {e}"),
                }
            }
            if doc.info().map(|i| i.generation) != Some(generation) {
                log::debug!("copy: the document was reloaded, text dropped");
                return;
            }
            let text = scope_text(&scope, page_count, |p| {
                have.iter()
                    .find(|(n, _)| *n == p)
                    .and_then(|(_, t)| t.clone())
            });
            inbox.post(Finished {
                title,
                text,
                pages: pages.end - pages.start,
            });
            ctx.request_repaint();
        });
    match spawned {
        Ok(_) => true,
        Err(e) => {
            log::error!("could not start the copy thread: {e}");
            false
        }
    }
}

/// Puts what the copy threads finished on the clipboard and tells the user, once per frame
/// (called by `dispatch_queued`).
pub fn finish_pending(state: &mut AppState, ctx: &egui::Context) {
    for done in state.range_copies.take() {
        let chars = done.text.chars().count();
        let notice = if chars == 0 {
            tr("status.range_empty").replace("{title}", &one_line(&done.title))
        } else {
            ctx.copy_text(done.text);
            copied_notice(&done.title, chars, done.pages)
        };
        dispatch(state, Action::ShowNotice(notice), ctx);
    }
}

// ------------------------------------------------------------------------------------------------
// The commands
// ------------------------------------------------------------------------------------------------

/// The focused view has a table of contents that leads somewhere: what "copy section text" and
/// "find in chapter" need.
pub fn has_sections(state: &AppState, tab: TabId) -> bool {
    state
        .tabs
        .pdf(tab)
        .and_then(|pdf| state.docs.get(pdf.doc))
        .and_then(|doc| doc.info())
        .is_some_and(|info| info.outline.items.iter().any(|i| i.target.is_some()))
}

/// Copies `text` and says so in the status bar.
fn copy_and_tell(state: &mut AppState, ctx: &egui::Context, text: String) {
    ctx.copy_text(text.clone());
    let notice = tr("status.copied_text").replace("{}", &text);
    dispatch(state, Action::ShowNotice(notice), ctx);
}

/// "Copy Location": `path#page=N` of the place of the view `tab` ([`place_of`]).
pub fn copy_location(state: &mut AppState, tab: TabId, ctx: &egui::Context) {
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let Some(doc) = state.docs.get(pdf.doc) else {
        return;
    };
    let place = place_of(pdf, |p| doc.page_text(p));
    let text = location_text(doc.path(), place.page);
    copy_and_tell(state, ctx, text);
}

/// "Copy Citation": `file, p. label — section path` of the place of the view `tab`.
pub fn copy_citation(state: &mut AppState, tab: TabId, ctx: &egui::Context) {
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let Some(doc) = state.docs.get(pdf.doc) else {
        return;
    };
    let Some(info) = doc.info() else {
        return;
    };
    let place = place_of(pdf, |p| doc.page_text(p));
    let label = page_label(place.page, &info.page_labels);
    let path = info
        .outline
        .current(place)
        .map(|idx| info.outline.path_titles(idx))
        .unwrap_or_default();
    let file = doc
        .path()
        .file_name()
        .map_or_else(|| doc.path().to_string_lossy(), |n| n.to_string_lossy());
    let text = citation_text(&file, &label, &path);
    copy_and_tell(state, ctx, text);
}

/// Starts the copy of a range and says so when there is nothing to copy.
fn copy_range(
    state: &mut AppState,
    ctx: &egui::Context,
    doc: &DocumentHandle,
    choice: Option<ScopeChoice>,
) {
    let started = choice.is_some_and(|choice| {
        start_range_copy(doc, &choice, state.range_copies.clone(), ctx.clone())
    });
    if !started {
        dispatch(
            state,
            Action::ShowNotice(tr("status.no_section").into()),
            ctx,
        );
    }
}

/// "Copy Section Text" and "Copy Chapter Text": the text of the section or the chapter the view
/// `tab` is in.
pub fn copy_section_text(state: &mut AppState, tab: TabId, part: Part, ctx: &egui::Context) {
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let Some(doc) = state.docs.get(pdf.doc).cloned() else {
        return;
    };
    let Some(info) = doc.info() else {
        return;
    };
    let page_count = u32::try_from(info.page_sizes.len()).unwrap_or(u32::MAX);
    let choice = choice_at(&info.outline, reading_place(pdf), page_count, part);
    copy_range(state, ctx, &doc, choice);
}

/// "Copy Text of This Section" in the table of contents: the text of outline item `item`.
pub fn copy_outline_item_text(state: &mut AppState, tab: TabId, item: u32, ctx: &egui::Context) {
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let Some(doc) = state.docs.get(pdf.doc).cloned() else {
        return;
    };
    let Some(info) = doc.info() else {
        return;
    };
    let page_count = u32::try_from(info.page_sizes.len()).unwrap_or(u32::MAX);
    let choice = ScopeChoice::section(&info.outline, item, page_count);
    copy_range(state, ctx, &doc, choice);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::Instant;

    use refdesk_core::testutil::{PdfBuilder, TempPdf};
    use refdesk_core::{IpcRequest, OpenTarget};

    use crate::action::{OpenTarget as ActionTarget, dispatch, dispatch_queued};
    use crate::cli::{self, Cli};
    use crate::commands::{self, MenuGroup};
    use crate::i18n::lookup;
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};

    fn at(page: u32, y: f32) -> Location {
        Location::new(page, 0.0, y)
    }

    // --- the place as text -------------------------------------------------------------------

    /// What the command line makes of one argument.
    fn parsed(arg: &str) -> OpenTarget {
        match cli::parse([OsString::from(arg)]) {
            Cli::Run {
                request: IpcRequest::Open { mut targets, .. },
                ..
            } if targets.len() == 1 => targets.remove(0),
            other => panic!("not one file: {other:?}"),
        }
    }

    #[test]
    fn a_location_text_opens_the_same_file_at_the_same_page_on_the_command_line() {
        let paths = [
            "/home/oscar/specs/AUTOSAR_SWS_ExecutionManagement.pdf",
            "/home/oscar/My Documents/two  spaces.pdf",
            "/home/oscar/문서/실행 관리 사양서.pdf",
            "/tmp/a#b.pdf",
            "/tmp/ends with hash#.pdf",
            "/tmp/looks#page=3 like a page.pdf",
            "/tmp/tab\tand 'quotes' and \"double\".pdf",
        ];
        for path in paths {
            for page in [0u32, 86, 1233] {
                let text = location_text(Path::new(path), page);
                let target = parsed(&text);
                assert_eq!(target.path, PathBuf::from(path), "{text}");
                assert_eq!(target.page, Some(page), "{text}");
            }
        }
        assert_eq!(
            location_text(Path::new("/docs/a.pdf"), 86),
            "/docs/a.pdf#page=87"
        );
    }

    #[test]
    fn a_path_that_is_not_text_cannot_make_a_location_that_opens_it() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/bad\xff.pdf"));
        let text = location_text(path, 4);
        assert!(text.ends_with("#page=5"));
        assert_ne!(parsed(&text).path, path, "the invalid byte is lost");
    }

    #[test]
    fn a_citation_has_the_file_the_page_label_and_the_section_path() {
        assert_eq!(
            citation_text(
                "AUTOSAR_SWS_ExecutionManagement.pdf",
                "87",
                &["7 Functional specification", "7.3 State management"]
            ),
            "AUTOSAR_SWS_ExecutionManagement.pdf, p. 87 — 7 Functional specification › 7.3 State management"
        );
        // Without a table of contents the path is left out, with its dash.
        assert_eq!(citation_text("a.pdf", "iv", &[]), "a.pdf, p. iv");
        // A title that was wrapped over lines is one line.
        assert_eq!(
            citation_text("a.pdf", "3", &["7  Functional\nspecification "]),
            "a.pdf, p. 3 — 7 Functional specification"
        );
        // The label is the one on the page, as the status bar shows it.
        let labels = vec!["i".to_owned(), "ii".to_owned(), String::new()];
        assert_eq!(page_label(1, &labels), "ii");
        assert_eq!(page_label(2, &labels), "3");
        assert_eq!(page_label(9, &labels), "10");
    }

    #[test]
    fn numbers_are_grouped_by_thousands_and_the_notice_names_pages_only_for_long_ranges() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(12345), "12,345");
        assert_eq!(group_digits(1234567), "1,234,567");

        let short = copied_notice("7.3 State\nmanagement", 12345, LONG_RANGE_PAGES);
        let expected = tr("status.range_copied")
            .replace("{title}", "7.3 State management")
            .replace("{chars}", "12,345");
        assert_eq!(short, expected);
        assert!(!short.contains("200"), "{short}");
        let long = copied_notice("Big", 1000, LONG_RANGE_PAGES + 1);
        assert!(long.contains("1,000") && long.contains("201"), "{long}");
    }

    // --- the range of a section --------------------------------------------------------------

    fn node(
        title: &str,
        page: u32,
        y: f32,
        children: Vec<refdesk_core::OutlineNode>,
    ) -> refdesk_core::OutlineNode {
        refdesk_core::OutlineNode {
            title: title.to_string(),
            target: Some(at(page, y)),
            uri: None,
            children,
        }
    }

    #[test]
    fn the_section_is_the_deepest_step_and_the_chapter_the_first() {
        let outline = Outline::from_tree(vec![
            node("1 One", 0, 10.0, vec![]),
            node(
                "2 Two",
                1,
                10.0,
                vec![node(
                    "2.1 Sub",
                    1,
                    300.0,
                    vec![node("2.1.1 Deep", 2, 50.0, vec![])],
                )],
            ),
        ]);
        let title = |at: Location, part| choice_at(&outline, at, 5, part).map(|c| c.title);
        assert_eq!(
            title(self::at(2, 100.0), Part::Section).as_deref(),
            Some("2.1.1 Deep")
        );
        assert_eq!(
            title(self::at(2, 100.0), Part::Chapter).as_deref(),
            Some("2 Two")
        );
        assert_eq!(
            title(self::at(1, 100.0), Part::Section).as_deref(),
            Some("2 Two")
        );
        assert_eq!(
            title(self::at(1, 100.0), Part::Chapter).as_deref(),
            Some("2 Two")
        );
        // Before the first entry, and without an outline, there is no section.
        assert_eq!(title(self::at(0, 5.0), Part::Section), None);
        assert_eq!(
            choice_at(&Outline::new(), at(1, 1.0), 5, Part::Chapter),
            None
        );
    }

    // --- with documents ----------------------------------------------------------------------

    /// Six pages. Chapter 2 runs from page 1 to the top of page 4 and has the sections 2.1
    /// (page 1, 300 pt, to page 2, 400 pt) and 2.2.
    fn spec_doc(stem: &str) -> TempPdf {
        let pdf = PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 60.0, 14.0, "1 Introduction");
                p.text(72.0, 100.0, 12.0, "intro body one");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 60.0, 14.0, "2 Functional specification");
                p.text(72.0, 100.0, 12.0, "alpha two body");
                p.text(72.0, 300.0, 14.0, "2.1 States");
                p.text(72.0, 360.0, 12.0, "state machine speci-");
                p.text(72.0, 376.0, 12.0, "fication text");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "state body page two");
                p.text(72.0, 400.0, 14.0, "2.2 Errors");
                p.text(72.0, 460.0, 12.0, "error body");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "error body page three");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 60.0, 14.0, "3 Annex");
                p.text(72.0, 100.0, 12.0, "annex body");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "annex last page");
            })
            .outline_leaf("1 Introduction", 1, 60.0)
            .outline("2 Functional specification", 2, 60.0, |o| {
                o.child("2.1 States", 2, 300.0);
                o.child("2.2 Errors", 3, 400.0);
            })
            .outline_leaf("3 Annex", 5, 60.0);
        pdf.write_temp(stem)
    }

    fn plain_doc(stem: &str) -> TempPdf {
        PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "no outline here");
            })
            .write_temp(stem)
    }

    struct Rig {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        /// What frames asked the platform to put on the clipboard, oldest first.
        copied: Vec<String>,
        time: f64,
    }

    impl Rig {
        fn new() -> Rig {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            Rig {
                ctx,
                repaints,
                state: AppState::new(),
                copied: Vec::new(),
                time: 0.0,
            }
        }

        /// One frame without widgets: the commands the context was given are collected, then
        /// the queued actions (and what the copy threads finished) are dispatched.
        fn frame(&mut self) {
            self.time += 1.0 / 60.0;
            let input = egui::RawInput {
                time: Some(self.time),
                ..Default::default()
            };
            let output = self.ctx.run_ui(input, |_ui| {});
            for command in &output.platform_output.commands {
                if let egui::OutputCommand::CopyText(text) = command {
                    self.copied.push(text.clone());
                }
            }
            output.drop_without_applying_deltas();
            dispatch_queued(&mut self.state, &self.ctx);
        }

        fn frames_until(&mut self, what: &str, mut check: impl FnMut(&Rig) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.frame();
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        fn open(&mut self, pdf: &TempPdf) -> TabId {
            let target = ActionTarget {
                path: pdf.path().to_path_buf(),
                page: None,
            };
            dispatch(&mut self.state, Action::OpenFiles(vec![target]), &self.ctx);
            let tab = self.state.focus.expect("focused");
            let doc = self.doc(tab);
            // Frames, not only waiting: a context swallows the repaint requests that come while
            // one is pending and no frame ran.
            self.frames_until("the document info", |_| doc.info().is_some());
            tab
        }

        fn doc(&self, tab: TabId) -> DocumentHandle {
            let doc = self.state.tabs.pdf(tab).expect("pdf tab").doc;
            self.state.docs.get(doc).expect("doc").clone()
        }

        fn pdf_mut(&mut self, tab: TabId) -> &mut PdfTab {
            match self.state.tabs.get_mut(tab) {
                Some(crate::state::Tab::Pdf(pdf)) => pdf,
                _ => panic!("not a pdf tab"),
            }
        }

        /// Reading at page `page` (zero-based), `y` pt down.
        fn read_at(&mut self, tab: TabId, page: u32, y: f32) {
            self.pdf_mut(tab).view.anchor = at(page, y);
        }

        /// Dispatches the action and runs frames until a copy has reached the clipboard.
        fn copy_via(&mut self, action: Action) -> String {
            let before = self.copied.len();
            dispatch(&mut self.state, action, &self.ctx);
            self.frames_until("the copy", |rig| rig.copied.len() > before);
            self.copied.last().cloned().expect("something was copied")
        }

        fn notice(&self) -> String {
            self.state
                .status_notice
                .as_ref()
                .map(|n| n.text.clone())
                .expect("a notice")
        }
    }

    #[test]
    fn copy_location_gives_the_file_and_the_page_being_read_and_tells_so() {
        let pdf = spec_doc("wp58-loc");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.read_at(tab, 2, 150.0);
        // Nothing waits on the document here: the text is made at once.
        dispatch(&mut rig.state, Action::CopyLocation(tab), &rig.ctx);
        rig.frame();
        let expected = format!("{}#page=3", pdf.path().display());
        assert_eq!(rig.copied, std::slice::from_ref(&expected));
        let notice = rig.notice();
        assert!(notice.contains(&expected), "{notice}");
        // The text opens the same file at the same page.
        let target = parsed(&expected);
        assert_eq!((target.path.as_path(), target.page), (pdf.path(), Some(2)));
    }

    #[test]
    fn copy_location_takes_the_page_where_the_selection_starts() {
        let pdf = spec_doc("wp58-loc-sel");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        // Reading page 4, with a selection that starts on page 1 and ends on page 2.
        rig.read_at(tab, 4, 100.0);
        let sel = Selection {
            anchor: TextPos { page: 2, ch: 3 },
            focus: TextPos { page: 1, ch: 5 },
        };
        rig.pdf_mut(tab).ui.selection.set_for_test(sel);
        dispatch(&mut rig.state, Action::CopyLocation(tab), &rig.ctx);
        rig.frame();
        assert_eq!(rig.copied, [format!("{}#page=2", pdf.path().display())]);
    }

    #[test]
    fn copy_citation_names_the_page_and_the_section_path_and_drops_the_path_without_an_outline() {
        let pdf = spec_doc("wp58-cite");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.read_at(tab, 2, 150.0);
        dispatch(&mut rig.state, Action::CopyCitation(tab), &rig.ctx);
        rig.frame();
        let name = pdf
            .path()
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            rig.copied,
            [format!(
                "{name}, p. 3 — 2 Functional specification › 2.1 States"
            )]
        );
        // Before the first heading: the page only.
        rig.read_at(tab, 0, 10.0);
        dispatch(&mut rig.state, Action::CopyCitation(tab), &rig.ctx);
        rig.frame();
        assert_eq!(rig.copied[1], format!("{name}, p. 1"));

        let plain = plain_doc("wp58-cite-plain");
        let tab = rig.open(&plain);
        dispatch(&mut rig.state, Action::CopyCitation(tab), &rig.ctx);
        rig.frame();
        let name = plain
            .path()
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert_eq!(rig.copied[2], format!("{name}, p. 1"));
    }

    #[test]
    fn the_text_of_a_section_over_several_pages_reaches_the_clipboard_and_the_next_one_does_not() {
        let pdf = spec_doc("wp58-section");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.read_at(tab, 2, 150.0);
        let text = rig.copy_via(Action::CopySectionText(tab));
        // Joined lines, the hyphen of the broken word gone, a line end between the paragraphs
        // and between the pages.
        assert_eq!(
            text,
            "2.1 States\nstate machine specification text\nstate body page two"
        );
        assert!(!text.contains("2.2") && !text.contains("error body"));
        assert!(
            !text.contains("alpha two body"),
            "the text above the heading"
        );
        let chars = text.chars().count();
        let notice = rig.notice();
        assert_eq!(
            notice,
            tr("status.range_copied")
                .replace("{title}", "2.1 States")
                .replace("{chars}", &group_digits(chars))
        );
    }

    #[test]
    fn the_text_of_a_chapter_runs_from_its_heading_to_the_next_chapter() {
        let pdf = spec_doc("wp58-chapter");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.read_at(tab, 3, 300.0);
        let text = rig.copy_via(Action::CopyChapterText(tab));
        assert!(text.starts_with("2 Functional specification"), "{text}");
        for part in [
            "alpha two body",
            "2.1 States",
            "specification text",
            "2.2 Errors",
            "error body",
            "error body page three",
        ] {
            assert!(text.contains(part), "{part} is in {text:?}");
        }
        assert!(
            !text.contains("annex") && !text.contains("3 Annex"),
            "{text}"
        );
        assert!(!text.contains("intro body"), "{text}");
        assert!(rig.notice().contains("2 Functional specification"));

        // The last chapter ends with the document.
        rig.read_at(tab, 5, 300.0);
        let text = rig.copy_via(Action::CopyChapterText(tab));
        assert_eq!(text, "3 Annex\nannex body\nannex last page");
    }

    #[test]
    fn a_table_of_contents_entry_copies_its_own_range() {
        let pdf = spec_doc("wp58-toc-copy");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        let info = rig.doc(tab).info().expect("info");
        let item = info
            .outline
            .items
            .iter()
            .position(|i| i.title == "2.2 Errors")
            .expect("item") as u32;
        // The view is elsewhere: the entry decides, not the place being read.
        rig.read_at(tab, 0, 0.0);
        let text = rig.copy_via(Action::CopyOutlineItemText { tab, item });
        assert_eq!(text, "2.2 Errors\nerror body\nerror body page three");
        assert!(rig.notice().contains("2.2 Errors"));
    }

    #[test]
    fn a_range_over_two_hundred_pages_is_copied_whole_and_the_notice_counts_the_pages() {
        let mut pdf = PdfBuilder::new();
        for n in 0..205 {
            pdf = pdf.page(300.0, 300.0, |p| {
                p.text(20.0, 60.0, 10.0, &format!("page {n}"));
            });
        }
        let pdf = pdf
            .outline_leaf("Everything", 1, 0.0)
            .write_temp("wp58-long");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.read_at(tab, 100, 10.0);
        let text = rig.copy_via(Action::CopyChapterText(tab));
        assert!(
            text.starts_with("page 0\n") && text.ends_with("page 204"),
            "{text:.40}"
        );
        assert_eq!(text.lines().count(), 205);
        let notice = rig.notice();
        assert!(notice.contains("205"), "{notice}");
        assert!(
            notice.contains(&group_digits(text.chars().count())),
            "{notice}"
        );
    }

    #[test]
    fn a_range_without_text_says_so_and_copies_nothing() {
        let pdf = PdfBuilder::new()
            .page(300.0, 300.0, |_| {})
            .page(300.0, 300.0, |_| {})
            .outline_leaf("Scanned", 1, 0.0)
            .write_temp("wp58-empty");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        dispatch(&mut rig.state, Action::CopyChapterText(tab), &rig.ctx);
        rig.frames_until("the notice", |rig| rig.state.status_notice.is_some());
        assert!(rig.copied.is_empty());
        let expected = tr("status.range_empty").replace("{title}", "Scanned");
        assert_eq!(rig.notice(), expected);
    }

    #[test]
    fn a_place_before_the_first_section_says_there_is_none() {
        let pdf = spec_doc("wp58-before");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.read_at(tab, 0, 5.0);
        dispatch(&mut rig.state, Action::CopySectionText(tab), &rig.ctx);
        assert_eq!(rig.notice(), tr("status.no_section"));
        rig.frame();
        assert!(rig.copied.is_empty());
    }

    #[test]
    fn copying_section_text_is_disabled_without_a_table_of_contents() {
        let mut rig = Rig::new();
        let plain = plain_doc("wp58-plain");
        rig.open(&plain);
        for id in [
            "edit.copy_section_text",
            "edit.copy_chapter_text",
            "edit.find_in_chapter",
        ] {
            let command = commands::command(id).expect(id);
            assert!(!(command.enabled)(&rig.state), "{id} needs an outline");
            assert_eq!(command.run(&rig.state), None);
        }
        // The location commands do not.
        for id in ["edit.copy_location", "edit.copy_citation"] {
            let command = commands::command(id).expect(id);
            assert!(
                command.run(&rig.state).is_some(),
                "{id} works without an outline"
            );
        }

        let spec = spec_doc("wp58-enabled");
        let tab_spec = rig.open(&spec);
        for (id, action) in [
            ("edit.copy_section_text", Action::CopySectionText(tab_spec)),
            ("edit.copy_chapter_text", Action::CopyChapterText(tab_spec)),
            ("edit.find_in_chapter", Action::FindInChapter(tab_spec)),
            ("edit.copy_location", Action::CopyLocation(tab_spec)),
            ("edit.copy_citation", Action::CopyCitation(tab_spec)),
        ] {
            let command = commands::command(id).expect(id);
            assert_eq!(command.run(&rig.state), Some(action), "{id}");
        }
    }

    // --- the command table -------------------------------------------------------------------

    #[test]
    fn the_new_commands_are_in_the_edit_menu_with_the_right_keys() {
        let edit: Vec<&str> = commands::in_menu(MenuGroup::Edit).map(|c| c.id).collect();
        for id in [
            "edit.copy_location",
            "edit.copy_citation",
            "edit.copy_section_text",
            "edit.copy_chapter_text",
            "edit.find_in_chapter",
        ] {
            assert!(edit.contains(&id), "{id} is in the Edit menu");
            let command = commands::command(id).expect(id);
            for lang in [crate::i18n::Lang::Ko, crate::i18n::Lang::En] {
                assert!(lookup(lang, command.name_key).is_some(), "{id}");
            }
            if id != "edit.copy_location" {
                assert_eq!(command.shortcut, None, "{id} has no key");
            }
        }
        let key = commands::command("edit.copy_location")
            .and_then(|c| c.shortcut)
            .expect("a key");
        assert_eq!(
            key,
            egui::KeyboardShortcut::new(
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                egui::Key::L
            )
        );
    }

    #[test]
    fn command_identifiers_and_shortcuts_are_still_unique() {
        let mut ids = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        for c in commands::COMMANDS {
            assert!(ids.insert(c.id), "duplicate identifier {}", c.id);
            for k in c.all_shortcuts() {
                assert!(
                    keys.insert((
                        k.modifiers.ctrl,
                        k.modifiers.shift,
                        k.modifiers.alt,
                        k.logical_key
                    )),
                    "{} repeats the key {k:?}",
                    c.id
                );
            }
        }
    }

    #[test]
    fn the_location_is_in_the_tab_menu_and_the_view_menu() {
        let entry = commands::TAB_MENU
            .iter()
            .find(|e| e.name_key == "cmd.edit.copy_location")
            .expect("in the tab menu");
        assert_eq!(entry.command_id, Some("edit.copy_location"));
        let pdf = plain_doc("wp58-tabmenu");
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        assert!((entry.enabled)(&rig.state, tab));
        assert_eq!((entry.action)(tab), Action::CopyLocation(tab));

        let view = commands::VIEW_MENU
            .iter()
            .find(|e| e.name_key == "cmd.edit.copy_location")
            .expect("in the view menu");
        let tab_state = rig.pdf_mut(tab).clone();
        assert!((view.enabled)(&tab_state));
        assert_eq!((view.action)(tab), Action::CopyLocation(tab));
    }
}
