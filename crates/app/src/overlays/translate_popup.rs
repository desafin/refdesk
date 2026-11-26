//! The translation popup of one PDF view (`docs/ARCHITECTURE.md` §5.12, TR-2 ~ 4).
//!
//! A word popup opens when the pointer rests on a word for [`SHOW_DELAY`] in translation mode
//! and closes [`CLOSE_DELAY`] after the pointer left both the word and the popup, or at once on
//! scrolling, a click elsewhere or a key. A selection popup opens when a selection is finished
//! in translation mode (or by "Translate Selection") and stays until the selection goes or
//! changes, Esc, or its close button.
//!
//! The decisions are plain functions and data, tested without egui (R-10): the timing
//! ([`WordTimer`]), which word counts ([`word_target`]), what is asked ([`word_requests`],
//! [`selection_requests`]), the size and place ([`clamp_size`], [`popup_rect`]) and the texts
//! shown ([`part_text`]). The UI thread never calls a backend: every request is a
//! [`TranslateJob`] (R-1), or a cache hit.

use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use egui::{Color32, Id, Key, Order, RichText, Ui, vec2};
use refdesk_core::translate::{prepare_passage, sentence_around};
use refdesk_core::{
    PageText, Point, Rect, Selection, Size, TranslateBackend, TranslateError, TranslateJob,
    TranslateRequest, TranslateState, TranslationCache,
};

use crate::action::{Action, notifier};
use crate::i18n::tr;
use crate::overlays::link_preview::{interrupted_by, place, visible_bounds};
use crate::views::links;
use crate::views::pdf_view::PdfViewUiState;
use crate::views::selection::{self, PageTexts};

use super::translate::{FailureHint, Readiness, error_text};

/// The pointer has to rest on a word this long (seconds) before its popup opens.
pub const SHOW_DELAY: f64 = 0.5;
/// A word popup closes this long (seconds) after the pointer left the word and the popup.
pub const CLOSE_DELAY: f64 = 0.2;
/// The popup is at most this fraction of the view width wide...
pub const WIDTH_FRACTION: f32 = 0.6;
/// ...but that limit is at least this many logical pixels...
pub const MIN_WIDTH: f32 = 220.0;
/// ...and at most this many.
pub const MAX_WIDTH: f32 = 560.0;
/// The popup is at most this fraction of the view height high (then it scrolls).
pub const HEIGHT_FRACTION: f32 = 0.5;
/// A word with more characters than this is not translated as a word.
pub const MAX_WORD_CHARS: usize = 40;
/// The source line of a selection popup shows at most this many characters.
pub const SOURCE_CHARS: usize = 80;
/// The size assumed before the popup was drawn once.
const FIRST_GUESS: Size = Size { w: 300.0, h: 90.0 };
/// Room for the buttons below the scrolled text.
const CHROME_HEIGHT: f32 = 44.0;

// ------------------------------------------------------------------------------------------------
// Which word, and when
// ------------------------------------------------------------------------------------------------

/// A word of a page: its page and character range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WordKey {
    /// Zero-based page.
    pub page: u32,
    /// First character.
    pub start: u32,
    /// One past the last character.
    pub end: u32,
}

impl WordKey {
    /// The character range.
    pub fn range(&self) -> Range<u32> {
        self.start..self.end
    }
}

/// Whether `c` is a letter of the Latin script (ASCII, Latin-1 and Latin Extended).
fn is_latin_letter(c: char) -> bool {
    c.is_ascii_alphabetic() || (c.is_alphabetic() && ('\u{00C0}'..='\u{024F}').contains(&c))
}

/// The word around character `ch` that a popup may translate: `PageText::word_at` without
/// punctuation at its ends, 2 to 40 characters with at least one Latin letter. Numbers alone,
/// symbols and Korean words are not translated. Gives the range and the text.
pub fn word_target(page: &PageText, ch: u32) -> Option<(Range<u32>, String)> {
    let range = page.word_at(ch);
    let chars: Vec<char> = page
        .chars
        .get(range.start as usize..range.end as usize)?
        .iter()
        .map(|c| c.ch)
        .collect();
    let keep = |c: &char| c.is_alphanumeric() || *c == '_';
    let first = chars.iter().position(keep)?;
    let last = chars.iter().rposition(keep)?;
    let word: String = chars[first..=last].iter().collect();
    let count = last + 1 - first;
    if !(2..=MAX_WORD_CHARS).contains(&count) || !word.chars().any(is_latin_letter) {
        return None;
    }
    let start = range.start + first as u32;
    Some((start..start + count as u32, word))
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Waiting {
        key: WordKey,
        since: f64,
    },
    Open {
        key: WordKey,
        away_since: Option<f64>,
    },
}

/// What one frame says about the pointer, for [`WordTimer::step`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    /// The time of the frame, in seconds.
    pub now: f64,
    /// The word under the pointer that may open a popup.
    pub over: Option<WordKey>,
    /// The pointer is over the popup.
    pub over_popup: bool,
    /// Scrolling, a click outside the popup or a key.
    pub interrupted: bool,
    /// No word popup now: translation mode off, a link under the pointer, a drag, a menu, the
    /// search field, the palette, the capture mode, a selection popup.
    pub blocked: bool,
}

/// The timing of the word popup: [`SHOW_DELAY`] to open, [`CLOSE_DELAY`] to close.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WordTimer {
    phase: Phase,
    /// A word whose popup was closed by scrolling, a click or a key: it does not open again
    /// until the pointer has left it.
    suppressed: Option<WordKey>,
}

impl WordTimer {
    /// Advances to the frame `f`. Returns the word whose popup is open after it.
    pub fn step(&mut self, f: Frame) -> Option<WordKey> {
        if f.blocked {
            self.phase = Phase::Idle;
            return None;
        }
        if f.interrupted {
            self.suppressed = f.over.or(self.open_key());
            self.phase = Phase::Idle;
            return None;
        }
        if self.suppressed.is_some() && self.suppressed != f.over {
            self.suppressed = None;
        }
        let over = f.over.filter(|k| Some(*k) != self.suppressed);
        self.phase = match self.phase {
            Phase::Idle => match over {
                Some(key) => Phase::Waiting { key, since: f.now },
                None => Phase::Idle,
            },
            Phase::Waiting { key, since } => match over {
                Some(k) if k == key && f.now - since >= SHOW_DELAY => Phase::Open {
                    key,
                    away_since: None,
                },
                Some(k) if k == key => Phase::Waiting { key, since },
                Some(k) => Phase::Waiting {
                    key: k,
                    since: f.now,
                },
                None => Phase::Idle,
            },
            Phase::Open { key, away_since } => match over {
                Some(k) if k == key => Phase::Open {
                    key,
                    away_since: None,
                },
                Some(k) => Phase::Waiting {
                    key: k,
                    since: f.now,
                },
                None if f.over_popup => Phase::Open {
                    key,
                    away_since: None,
                },
                None => {
                    let away = away_since.unwrap_or(f.now);
                    if f.now - away >= CLOSE_DELAY {
                        Phase::Idle
                    } else {
                        Phase::Open {
                            key,
                            away_since: Some(away),
                        }
                    }
                }
            },
        };
        self.open_key()
    }

    /// The word whose popup is open.
    pub fn open_key(&self) -> Option<WordKey> {
        match self.phase {
            Phase::Open { key, .. } => Some(key),
            _ => None,
        }
    }

    /// Closes the popup now, and keeps it closed while the pointer stays on that word.
    pub fn close(&mut self) {
        self.suppressed = self.open_key().or(self.suppressed);
        self.phase = Phase::Idle;
    }

    /// Time until the popup opens or closes by itself; `None` when nothing is pending.
    pub fn remaining(&self, now: f64) -> Option<Duration> {
        let left = match self.phase {
            Phase::Waiting { since, .. } => SHOW_DELAY - (now - since),
            Phase::Open {
                away_since: Some(away),
                ..
            } => CLOSE_DELAY - (now - away),
            _ => return None,
        };
        Some(Duration::from_secs_f64(left.max(0.0)))
    }
}

/// Whether a finished press should open a selection popup: a press just ended in translation
/// mode, and it left a selection that is not the one already shown.
pub fn selection_finished(
    was_dragging: bool,
    dragging: bool,
    mode: bool,
    selection: Option<Selection>,
    shown: Option<Selection>,
) -> bool {
    was_dragging && !dragging && mode && selection.is_some() && selection != shown
}

// ------------------------------------------------------------------------------------------------
// What is asked
// ------------------------------------------------------------------------------------------------

/// The requests of a word popup: the meaning of `word` in `sentence`, and the translation of the
/// sentence when `with_sentence` is set and there is one.
pub fn word_requests(
    word: &str,
    sentence: &str,
    with_sentence: bool,
) -> (TranslateRequest, Option<TranslateRequest>) {
    let main = TranslateRequest::word(word, sentence);
    let (prepared, _) = prepare_passage(sentence);
    let second =
        (with_sentence && !prepared.is_empty()).then(|| TranslateRequest::passage(prepared));
    (main, second)
}

/// What a selected text asks: a selection of one word (no white space, at most
/// [`MAX_WORD_CHARS`] characters) is a word in `sentence`; anything else a passage, cut to
/// 2,000 characters. The flag says whether it was cut.
pub fn selection_requests(
    text: &str,
    sentence: impl FnOnce() -> String,
    with_sentence: bool,
) -> (TranslateRequest, Option<TranslateRequest>, bool) {
    let trimmed = text.trim();
    let one_word = !trimmed.is_empty()
        && !trimmed.contains(char::is_whitespace)
        && trimmed.chars().count() <= MAX_WORD_CHARS;
    if one_word {
        let (main, second) = word_requests(trimmed, &sentence(), with_sentence);
        return (main, second, false);
    }
    let (prepared, cut) = prepare_passage(trimmed);
    (TranslateRequest::passage(prepared), None, cut)
}

/// The first [`SOURCE_CHARS`] characters of a text on one line, with "…" when there was more.
pub fn source_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SOURCE_CHARS {
        flat
    } else {
        let mut s: String = flat.chars().take(SOURCE_CHARS).collect();
        s.push('…');
        s
    }
}

// ------------------------------------------------------------------------------------------------
// Size and place
// ------------------------------------------------------------------------------------------------

/// The widest a popup may be in a view of this size: [`WIDTH_FRACTION`] of the view, limited to
/// [`MIN_WIDTH`] ..= [`MAX_WIDTH`], never wider than the view.
pub fn max_width(view: Size) -> f32 {
    (view.w * WIDTH_FRACTION)
        .clamp(MIN_WIDTH, MAX_WIDTH)
        .min(view.w)
        .max(0.0)
}

/// The size of a popup whose content wants `content`: as wide as the content between
/// [`MIN_WIDTH`] and [`max_width`], at most [`HEIGHT_FRACTION`] of the view high.
pub fn clamp_size(content: Size, view: Size) -> Size {
    let max_w = max_width(view);
    let w = content.w.clamp(MIN_WIDTH.min(max_w), max_w);
    let h = content.h.min(view.h * HEIGHT_FRACTION).max(0.0);
    Size::new(w, h)
}

/// The popup for a target (the word, or the last line of the selection) whose visible part is
/// `target`, in a view of size `view`: just below it, above it when there is no room, inside
/// the view and clear of the target (the placing of the link preview).
pub fn popup_rect(target: Rect, view: Size, content: Size) -> Rect {
    place(target, view, clamp_size(content, view)).0
}

// ------------------------------------------------------------------------------------------------
// The popup's state
// ------------------------------------------------------------------------------------------------

/// What the app hands every PDF view for one frame (`AppState.translate`, cloned cheaply).
#[derive(Clone)]
pub struct ViewEnv {
    /// Translation mode is on.
    pub mode: bool,
    /// The backend; only jobs call it.
    pub backend: Arc<dyn TranslateBackend>,
    /// Finished translations.
    pub cache: Arc<Mutex<TranslationCache>>,
    /// Whether the model is loaded.
    pub readiness: Readiness,
    /// Word popups also translate the sentence.
    pub word_with_sentence: bool,
    /// Bumped when the backend is replaced: older popups go.
    pub generation: u64,
    /// The command palette is open: no word popup.
    pub overlay_open: bool,
    /// Names for the texts that explain a failure.
    pub hint: FailureHint,
}

impl std::fmt::Debug for ViewEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewEnv")
            .field("mode", &self.mode)
            .field("readiness", &self.readiness)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// Where a part of a popup is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartState {
    /// Waiting for (more of) the answer.
    Running,
    /// Complete.
    Done,
    /// Failed.
    Failed(TranslateError),
}

/// One request of a popup and what came of it.
#[derive(Debug)]
pub struct Part {
    /// What was asked.
    pub req: TranslateRequest,
    job: Option<TranslateJob>,
    /// The translation so far.
    pub text: String,
    /// Running, done or failed.
    pub state: PartState,
    /// It came from the cache: no backend was asked.
    #[cfg_attr(not(test), allow(dead_code))]
    pub cached: bool,
}

/// The text a part shows: the translation, else "Translating…" (or "Preparing the model…"
/// while the model loads), else the reason it failed.
pub fn part_text(
    text: &str,
    state: &PartState,
    readiness: &Readiness,
    hint: &FailureHint,
) -> String {
    if !text.is_empty() {
        return text.to_string();
    }
    match state {
        PartState::Failed(e) => error_text(e, hint),
        _ => match readiness {
            Readiness::Off | Readiness::Loading => tr("translate.loading_model").to_owned(),
            _ => tr("translate.working").to_owned(),
        },
    }
}

/// What the popup is about.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A word under the pointer.
    Word(WordKey),
    /// A selection.
    Selection(Selection),
}

/// An open popup.
#[derive(Debug)]
pub struct Popup {
    /// What it is about.
    pub target: Target,
    /// The original text (the word, or the start of the selection).
    pub source: String,
    /// The meaning of the word, or the translation of the passage.
    pub main: Part,
    /// The translation of the word's sentence.
    pub sentence: Option<Part>,
    /// The passage was cut to 2,000 characters.
    pub truncated: bool,
    /// The page the popup sits at and the lines it stays clear of (page space, R-5).
    anchor_page: u32,
    anchor_rects: Vec<Rect>,
    /// The word's lines, tinted while the popup is open.
    highlight: Vec<Rect>,
    /// The size the popup took when last drawn.
    measured: Option<Size>,
    env_generation: u64,
    doc_generation: u32,
}

impl Popup {
    /// Whether this is a word popup (from hovering).
    pub fn is_word(&self) -> bool {
        matches!(self.target, Target::Word(_))
    }

    /// The parts, main first.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn parts(&self) -> impl Iterator<Item = &Part> {
        std::iter::once(&self.main).chain(self.sentence.iter())
    }

    fn parts_mut(&mut self) -> impl Iterator<Item = &mut Part> {
        std::iter::once(&mut self.main).chain(self.sentence.iter_mut())
    }

    /// Whether some part is still waiting for its job.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn running(&self) -> bool {
        self.parts().any(|p| p.job.is_some())
    }
}

/// The translation state of one view. Not saved; a duplicated view starts without a popup.
#[derive(Default)]
pub struct TranslateView {
    /// Set by the window before the view is drawn. `None` (views drawn elsewhere, tests of other
    /// features): no translation at all.
    pub env: Option<ViewEnv>,
    timer: WordTimer,
    popup: Option<Popup>,
    was_dragging: bool,
}

impl Clone for TranslateView {
    fn clone(&self) -> TranslateView {
        TranslateView::default()
    }
}

impl std::fmt::Debug for TranslateView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranslateView")
            .field("popup", &self.popup)
            .field("timer", &self.timer)
            .finish_non_exhaustive()
    }
}

impl TranslateView {
    /// The open popup.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn popup(&self) -> Option<&Popup> {
        self.popup.as_ref()
    }

    /// Closes the popup and drops its jobs (which cancels them).
    pub fn close(&mut self) {
        if self.popup.as_ref().is_some_and(Popup::is_word) {
            self.timer.close();
        }
        self.popup = None;
    }
}

/// Starts the request of a part, or takes it from the cache. Returns the part and whether the
/// backend was asked while the model is known to be unavailable (the caller retries loading it).
fn start_part(env: &ViewEnv, req: TranslateRequest, ctx: &egui::Context) -> (Part, bool) {
    let identity = env.backend.identity();
    let hit = env
        .cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&identity, &req)
        .map(str::to_owned);
    if let Some(text) = hit {
        return (
            Part {
                req,
                job: None,
                text,
                state: PartState::Done,
                cached: true,
            },
            false,
        );
    }
    let job = TranslateJob::start(Arc::clone(&env.backend), req.clone(), notifier(ctx));
    let retry = matches!(env.readiness, Readiness::Failed(_));
    (
        Part {
            req,
            job: Some(job),
            text: String::new(),
            state: PartState::Running,
            cached: false,
        },
        retry,
    )
}

/// Takes what the jobs of the popup have so far; finished translations go into the cache.
fn poll(popup: &mut Popup, env: &ViewEnv) {
    let identity = env.backend.identity();
    for part in popup.parts_mut() {
        let Some(job) = &part.job else {
            continue;
        };
        let snap = job.snapshot();
        part.text.clone_from(&snap.text);
        match &snap.state {
            TranslateState::Running => {}
            TranslateState::Done => {
                part.state = PartState::Done;
                part.job = None;
                if !part.text.trim().is_empty() {
                    env.cache
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(&identity, &part.req, part.text.clone());
                }
            }
            TranslateState::Failed(e) => {
                part.state = PartState::Failed(e.clone());
                part.job = None;
            }
        }
    }
}

/// Opens a word popup for `key`. Returns whether a failed model should be loaded again.
fn open_word(
    view: &mut TranslateView,
    env: &ViewEnv,
    key: WordKey,
    texts: PageTexts,
    doc_generation: u32,
    ctx: &egui::Context,
) -> bool {
    let Some(page) = texts(key.page) else {
        return false;
    };
    let word: String = page
        .chars
        .get(key.start as usize..key.end as usize)
        .unwrap_or(&[])
        .iter()
        .map(|c| c.ch)
        .collect();
    let sentence = sentence_around(&page, key.range());
    let (main_req, second_req) = word_requests(&word, &sentence, env.word_with_sentence);
    let (main, retry_a) = start_part(env, main_req, ctx);
    let (sentence_part, retry_b) = match second_req {
        Some(r) => {
            let (p, retry) = start_part(env, r, ctx);
            (Some(p), retry)
        }
        None => (None, false),
    };
    let rects = page.rects(key.range());
    view.popup = Some(Popup {
        target: Target::Word(key),
        source: word,
        main,
        sentence: sentence_part,
        truncated: false,
        anchor_page: key.page,
        anchor_rects: rects.clone(),
        highlight: rects,
        measured: None,
        env_generation: env.generation,
        doc_generation,
    });
    retry_a || retry_b
}

/// Opens a selection popup for `sel`. Returns whether a failed model should be loaded again.
/// Nothing opens for a selection without text.
pub fn open_selection(
    view: &mut TranslateView,
    env: &ViewEnv,
    sel: Selection,
    texts: PageTexts,
    doc_generation: u32,
    ctx: &egui::Context,
) -> bool {
    let text = selection::selection_string(&sel, texts);
    if text.trim().is_empty() {
        return false;
    }
    let (first, last) = sel.ordered();
    let sentence = || {
        texts(first.page)
            .map(|p| {
                let end = if last.page == first.page {
                    last.ch
                } else {
                    p.chars.len() as u32
                };
                sentence_around(&p, first.ch..end.max(first.ch + 1))
            })
            .unwrap_or_default()
    };
    let (main_req, second_req, truncated) =
        selection_requests(&text, sentence, env.word_with_sentence);
    let (main, retry_a) = start_part(env, main_req, ctx);
    let (sentence_part, retry_b) = match second_req {
        Some(r) => {
            let (p, retry) = start_part(env, r, ctx);
            (Some(p), retry)
        }
        None => (None, false),
    };
    // The popup sits at the last line of the selection.
    let anchor_rects = texts(last.page)
        .and_then(|p| {
            let range = sel.range_on(last.page, p.chars.len() as u32)?;
            p.rects(range).last().copied()
        })
        .map(|r| vec![r])
        .unwrap_or_default();
    view.timer.close();
    view.popup = Some(Popup {
        target: Target::Selection(sel),
        source: source_line(&text),
        main,
        sentence: sentence_part,
        truncated,
        anchor_page: last.page,
        anchor_rects,
        highlight: Vec::new(),
        measured: None,
        env_generation: env.generation,
        doc_generation,
    });
    retry_a || retry_b
}

// ------------------------------------------------------------------------------------------------
// egui
// ------------------------------------------------------------------------------------------------

/// What [`update`] needs from the view for one frame.
pub struct UpdateEnv<'a> {
    /// The view on screen.
    pub view_rect: egui::Rect,
    /// The time of the frame, in seconds.
    pub now: f64,
    /// The pointer is over the view (not over another layer).
    pub hovered: bool,
    /// A link is under the pointer (its preview wins).
    pub over_link: bool,
    /// Something else has the attention: a drag, a menu, the search field, the capture mode.
    pub blocked: bool,
    /// The page texts.
    pub texts: PageTexts<'a>,
    /// The document generation (R-7).
    pub doc_generation: u32,
}

fn to_egui(view_rect: egui::Rect, r: Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        view_rect.min + vec2(r.x0, r.y0),
        view_rect.min + vec2(r.x1, r.y1),
    )
}

/// The popup's rectangle in the view as it is now (view coordinates); `None` when its target is
/// out of sight.
fn geometry(popup: &Popup, state: &PdfViewUiState, view: Size) -> Option<(Rect, Vec<Rect>)> {
    let layout = state.layout()?;
    if popup.anchor_page >= layout.page_count() {
        return None;
    }
    let anchor = links::screen_rects(
        layout,
        state.scroll(),
        popup.anchor_page,
        &popup.anchor_rects,
    );
    let target = visible_bounds(&anchor, view)?;
    let rect = popup_rect(target, view, popup.measured.unwrap_or(FIRST_GUESS));
    let highlight =
        links::screen_rects(layout, state.scroll(), popup.anchor_page, &popup.highlight);
    Some((rect, highlight))
}

/// The popup on screen, if it is shown.
pub fn popup_screen_rect(state: &PdfViewUiState, view_rect: egui::Rect) -> Option<egui::Rect> {
    let popup = state.translate.popup.as_ref()?;
    let view = Size::new(view_rect.width(), view_rect.height());
    geometry(popup, state, view).map(|(r, _)| to_egui(view_rect, r))
}

/// The word under the pointer that may open a popup.
fn word_under_pointer(ui: &Ui, state: &PdfViewUiState, env: &UpdateEnv) -> Option<WordKey> {
    if !env.hovered {
        return None;
    }
    let pos = ui.input(|i| i.pointer.hover_pos())?;
    if !env.view_rect.contains(pos) {
        return None;
    }
    let layout = state.layout()?;
    let v = pos - env.view_rect.min;
    let hit = selection::locate(layout, state.scroll(), Point::new(v.x, v.y))?;
    if !hit.on_page {
        return None;
    }
    let text = (env.texts)(hit.loc.page)?;
    let ch = text.char_at(Point::new(hit.loc.x, hit.loc.y))?;
    let (range, _) = word_target(&text, ch)?;
    Some(WordKey {
        page: hit.loc.page,
        start: range.start,
        end: range.end,
    })
}

/// Runs the popup for one frame, after the pointer was handled: opens and closes word popups on
/// the timer, opens a selection popup when a selection was finished, closes one whose selection
/// went, and takes what the jobs have. Asks for a frame when the timer is pending.
pub fn update(ui: &Ui, state: &mut PdfViewUiState, env: &UpdateEnv, queue: &mut Vec<Action>) {
    let Some(view_env) = state.translate.env.clone() else {
        return;
    };
    let ctx = ui.ctx().clone();
    let dragging = state.selection.is_dragging();
    let selection = state.selection.active();
    let popup_rect = popup_screen_rect(state, env.view_rect);
    let pointer = ui.input(|i| i.pointer.hover_pos());
    let over_popup = popup_rect.zip(pointer).is_some_and(|(r, p)| r.contains(p));
    let over = if over_popup || !view_env.mode {
        None
    } else {
        word_under_pointer(ui, state, env)
    };
    let interrupted = ui.input(|i| interrupted_by(&i.events, popup_rect));
    let escape = ui.input(|i| i.key_pressed(Key::Escape));

    let tv = &mut state.translate;
    let mut retry = false;

    // A popup from another backend or another version of the document goes.
    if tv.popup.as_ref().is_some_and(|p| {
        p.env_generation != view_env.generation || p.doc_generation != env.doc_generation
    }) {
        tv.close();
    }

    // The selection popup: a finished press opens one; a gone or changed selection, or Esc,
    // closes it.
    let was_dragging = std::mem::replace(&mut tv.was_dragging, dragging);
    let shown_selection = match tv.popup.as_ref().map(|p| &p.target) {
        Some(Target::Selection(s)) => Some(*s),
        _ => None,
    };
    if selection_finished(
        was_dragging,
        dragging,
        view_env.mode,
        selection,
        shown_selection,
    ) && let Some(sel) = selection
    {
        retry |= open_selection(tv, &view_env, sel, env.texts, env.doc_generation, &ctx);
    } else if let Some(shown) = shown_selection
        && (selection != Some(shown) || escape)
    {
        tv.close();
    }

    // The word popup.
    let selection_popup = tv.popup.as_ref().is_some_and(|p| !p.is_word());
    let blocked = !view_env.mode
        || env.blocked
        || env.over_link
        || view_env.overlay_open
        || selection_popup
        || dragging;
    let open = tv.timer.step(Frame {
        now: env.now,
        over,
        over_popup,
        interrupted,
        blocked,
    });
    let word_shown = match tv.popup.as_ref().map(|p| &p.target) {
        Some(Target::Word(k)) => Some(*k),
        _ => None,
    };
    if word_shown.is_some() && word_shown != open {
        tv.popup = None;
    }
    if let Some(key) = open
        && word_shown != Some(key)
    {
        retry |= open_word(tv, &view_env, key, env.texts, env.doc_generation, &ctx);
        if tv.popup.is_none() {
            tv.timer.close();
        }
    }

    if let Some(popup) = &mut tv.popup {
        poll(popup, &view_env);
    }
    if let Some(left) = tv.timer.remaining(env.now) {
        ctx.request_repaint_after(left);
    }
    if retry {
        queue.push(Action::RetryTranslateWarmUp);
    }
}

/// Paints the open popup above everything, and the tint of its word.
pub fn show(ui: &Ui, state: &mut PdfViewUiState, view_rect: egui::Rect, tab: crate::state::TabId) {
    let Some(env) = state.translate.env.clone() else {
        return;
    };
    let view = Size::new(view_rect.width(), view_rect.height());
    let Some((rect, highlight)) = state
        .translate
        .popup
        .as_ref()
        .and_then(|p| geometry(p, state, view))
    else {
        return;
    };
    let Some(popup) = state.translate.popup.as_mut() else {
        return;
    };
    let painter = ui.painter_at(view_rect);
    let tint = Color32::from_rgba_unmultiplied(255, 196, 0, 60);
    for r in &highlight {
        painter.rect_filled(to_egui(view_rect, *r), 2.0, tint);
    }

    let screen = to_egui(view_rect, rect);
    let max_w = max_width(view);
    let max_h = (view.h * HEIGHT_FRACTION - CHROME_HEIGHT).max(40.0);
    let main_text = part_text(
        &popup.main.text,
        &popup.main.state,
        &env.readiness,
        &env.hint,
    );
    let failed = matches!(popup.main.state, PartState::Failed(_)) && popup.main.text.is_empty();
    let sentence_text = popup
        .sentence
        .as_ref()
        .map(|p| part_text(&p.text, &p.state, &env.readiness, &env.hint));
    let copy_text = popup.main.text.clone();
    let is_word = popup.main.req.is_word();
    let closable = !popup.is_word();
    let mut close = false;
    let response = egui::Area::new(Id::unique(("translate_popup", tab)))
        .order(Order::Tooltip)
        .fixed_pos(screen.min)
        .constrain(false)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                let inner = (max_w - 16.0).max(MIN_WIDTH - 16.0);
                ui.set_max_width(inner);
                ui.set_min_width((MIN_WIDTH - 16.0).min(inner));
                egui::ScrollArea::vertical()
                    .max_height(max_h)
                    .auto_shrink([true, true])
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&popup.source).small().weak()).wrap(),
                        );
                        let mut main = RichText::new(main_text);
                        if is_word && !failed {
                            main = main.strong();
                        }
                        if failed {
                            main = main.color(ui.visuals().warn_fg_color);
                        }
                        ui.add(egui::Label::new(main).wrap());
                        if let Some(text) = sentence_text {
                            ui.separator();
                            ui.add(egui::Label::new(RichText::new(text).small()).wrap());
                        }
                        if popup.truncated {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(tr("translate.truncated")).small().italics(),
                                )
                                .wrap(),
                            );
                        }
                    });
                ui.horizontal(|ui| {
                    let copy = ui.add_enabled(
                        !copy_text.is_empty(),
                        egui::Button::new(tr("translate.copy")).small(),
                    );
                    if copy.clicked() {
                        ui.ctx().copy_text(copy_text.clone());
                    }
                    if closable && ui.small_button(tr("translate.close")).clicked() {
                        close = true;
                    }
                });
            });
        });
    let size = response.response.rect.size();
    let measured = Size::new(size.x, size.y);
    if popup.measured != Some(measured) {
        popup.measured = Some(measured);
        ui.ctx().request_repaint();
    }
    if close {
        state.translate.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refdesk_core::{Quad, TextBlock, TextChar, TextLine, TextPos, TranslateKind};

    use crate::overlays::translate::FailureHint;

    // ---- which word ----

    /// One line of text; characters 10 pt wide.
    fn line(text: &str) -> PageText {
        let mut pt = PageText::default();
        for (i, ch) in text.chars().enumerate() {
            let x = 72.0 + i as f32 * 10.0;
            pt.chars.push(TextChar {
                ch,
                quad: Quad {
                    ul: Point::new(x, 100.0),
                    ur: Point::new(x + 10.0, 100.0),
                    ll: Point::new(x, 112.0),
                    lr: Point::new(x + 10.0, 112.0),
                },
                size: 12.0,
            });
        }
        let n = pt.chars.len() as u32;
        pt.lines.push(TextLine {
            chars: 0..n,
            bounds: Rect::new(72.0, 100.0, 72.0 + n as f32 * 10.0, 112.0),
        });
        pt.blocks.push(TextBlock {
            lines: 0..1,
            bounds: Rect::new(72.0, 100.0, 600.0, 112.0),
        });
        pt
    }

    /// The target around the first character of `needle` in `text`.
    fn target_in(text: &str, needle: &str) -> Option<String> {
        let pt = line(text);
        let byte = text.find(needle).expect("needle");
        let ch = text[..byte].chars().count() as u32;
        word_target(&pt, ch).map(|(_, w)| w)
    }

    #[test]
    fn which_words_are_translated() {
        assert_eq!(
            target_in("the watchdog runs", "watchdog").as_deref(),
            Some("watchdog")
        );
        // Punctuation around a word is not part of it.
        assert_eq!(
            target_in("(watchdog), next", "watchdog").as_deref(),
            Some("watchdog")
        );
        assert_eq!(
            target_in("end of process.", "process").as_deref(),
            Some("process")
        );
        // An identifier has Latin letters.
        assert_eq!(
            target_in("see SWS_EM_01004 here", "SWS").as_deref(),
            Some("SWS_EM_01004")
        );
        assert_eq!(target_in("café au lait", "café").as_deref(), Some("café"));
        // Numbers, symbols, Korean, one letter, too long.
        assert_eq!(target_in("value 12345 ms", "12345"), None);
        assert_eq!(target_in("a -> b", "->"), None);
        assert_eq!(target_in("영어 감시자 단어", "감시자"), None);
        assert_eq!(target_in("x a y", " a ").or(target_in("x a y", "a")), None);
        let long = "a".repeat(41);
        assert_eq!(target_in(&format!("x {long} y"), &long), None);
        let ok = "a".repeat(40);
        assert_eq!(target_in(&format!("x {ok} y"), &ok), Some(ok));
    }

    #[test]
    fn the_range_of_a_word_target_leaves_out_the_punctuation() {
        let pt = line("(watchdog),");
        let (range, word) = word_target(&pt, 3).expect("a word");
        assert_eq!(word, "watchdog");
        assert_eq!(range, 1..9);
    }

    // ---- the timer ----

    fn key(start: u32) -> WordKey {
        WordKey {
            page: 0,
            start,
            end: start + 4,
        }
    }

    fn at(now: f64, over: Option<WordKey>) -> Frame {
        Frame {
            now,
            over,
            over_popup: false,
            interrupted: false,
            blocked: false,
        }
    }

    #[test]
    fn a_word_popup_opens_after_half_a_second_and_not_before() {
        let a = key(0);
        let mut t = WordTimer::default();
        assert_eq!(t.step(at(10.0, Some(a))), None);
        assert_eq!(t.step(at(10.3, Some(a))), None);
        assert_eq!(t.step(at(10.499, Some(a))), None);
        assert_eq!(t.step(at(10.5, Some(a))), Some(a));
        assert!(t.remaining(10.5).is_none());
    }

    #[test]
    fn another_word_starts_the_clock_again_and_the_same_word_keeps_it() {
        let (a, b) = (key(0), key(10));
        let mut t = WordTimer::default();
        t.step(at(1.0, Some(a)));
        // Moving inside the same word is resting on it.
        t.step(at(1.2, Some(a)));
        assert_eq!(t.step(at(1.5, Some(a))), Some(a));
        let mut t = WordTimer::default();
        t.step(at(1.0, Some(a)));
        assert_eq!(t.step(at(1.4, Some(b))), None);
        assert_eq!(t.step(at(1.8, Some(b))), None);
        assert_eq!(t.step(at(1.9, Some(b))), Some(b));
        assert_eq!(t.remaining(1.95), None);
        let mut t = WordTimer::default();
        t.step(at(1.0, Some(a)));
        let left = t.remaining(1.2).expect("pending");
        assert!((left.as_secs_f64() - 0.3).abs() < 1e-6);
    }

    fn opened(a: WordKey) -> WordTimer {
        let mut t = WordTimer::default();
        t.step(at(1.0, Some(a)));
        assert_eq!(t.step(at(1.5, Some(a))), Some(a));
        t
    }

    #[test]
    fn leaving_the_word_closes_after_a_fifth_of_a_second_unless_the_pointer_is_on_the_popup() {
        let a = key(0);
        let mut t = opened(a);
        assert_eq!(t.step(at(2.0, None)), Some(a));
        assert_eq!(t.step(at(2.19, None)), Some(a));
        assert_eq!(t.step(at(2.2, None)), None);

        let mut t = opened(a);
        t.step(at(2.0, None));
        let on_popup = Frame {
            over_popup: true,
            ..at(2.15, None)
        };
        assert_eq!(t.step(on_popup), Some(a));
        assert_eq!(
            t.step(Frame {
                over_popup: true,
                ..at(9.0, None)
            }),
            Some(a)
        );
        assert_eq!(t.step(at(9.1, None)), Some(a));
        assert_eq!(t.step(at(9.3, None)), None);
    }

    #[test]
    fn scrolling_clicking_or_typing_closes_at_once_and_keeps_it_closed_on_that_word() {
        let a = key(0);
        let mut t = opened(a);
        let hit = Frame {
            interrupted: true,
            ..at(2.0, Some(a))
        };
        assert_eq!(t.step(hit), None);
        // Still on the word: it does not come back.
        assert_eq!(t.step(at(3.0, Some(a))), None);
        assert_eq!(t.step(at(9.0, Some(a))), None);
        // Away and back: the clock runs again.
        t.step(at(9.1, None));
        t.step(at(9.2, Some(a)));
        assert_eq!(t.step(at(9.7, Some(a))), Some(a));
    }

    #[test]
    fn blocked_frames_keep_the_popup_away() {
        let a = key(0);
        let mut t = WordTimer::default();
        t.step(at(1.0, Some(a)));
        let blocked = Frame {
            blocked: true,
            ..at(1.6, Some(a))
        };
        assert_eq!(t.step(blocked), None);
        let mut t = opened(a);
        assert_eq!(
            t.step(Frame {
                blocked: true,
                ..at(2.0, Some(a))
            }),
            None
        );
    }

    fn sel(a: u32, b: u32) -> Selection {
        Selection {
            anchor: TextPos { page: 0, ch: a },
            focus: TextPos { page: 0, ch: b },
        }
    }

    #[test]
    fn a_finished_press_with_a_new_selection_opens_a_selection_popup() {
        let s = Some(sel(0, 5));
        assert!(selection_finished(true, false, true, s, None));
        assert!(
            !selection_finished(true, true, true, s, None),
            "still dragging"
        );
        assert!(
            !selection_finished(false, false, true, s, None),
            "no press ended"
        );
        assert!(!selection_finished(true, false, false, s, None), "mode off");
        assert!(
            !selection_finished(true, false, true, None, None),
            "a click"
        );
        assert!(
            !selection_finished(true, false, true, s, s),
            "already shown"
        );
        assert!(selection_finished(true, false, true, s, Some(sel(0, 3))));
    }

    // ---- what is asked ----

    #[test]
    fn a_word_asks_for_its_meaning_in_the_sentence_and_the_sentence() {
        let (main, second) = word_requests("liveness", "The liveness  of it.", true);
        assert_eq!(
            main,
            TranslateRequest::word("liveness", "The liveness  of it.")
        );
        assert_eq!(
            second,
            Some(TranslateRequest::passage("The liveness of it."))
        );
        let (_, second) = word_requests("liveness", "The liveness of it.", false);
        assert_eq!(second, None);
        let (main, second) = word_requests("liveness", "", true);
        assert_eq!(
            main.kind,
            TranslateKind::Word {
                context: String::new()
            }
        );
        assert_eq!(second, None);
    }

    #[test]
    fn a_selection_of_one_word_is_a_word_and_a_longer_one_a_passage() {
        let (main, second, cut) =
            selection_requests(" watchdog ", || "The watchdog runs.".into(), true);
        assert_eq!(
            main,
            TranslateRequest::word("watchdog", "The watchdog runs.")
        );
        assert_eq!(
            second,
            Some(TranslateRequest::passage("The watchdog runs."))
        );
        assert!(!cut);
        let (main, second, cut) =
            selection_requests("The watchdog\nruns.", || unreachable!(), true);
        assert_eq!(main, TranslateRequest::passage("The watchdog runs."));
        assert_eq!(second, None);
        assert!(!cut);
        let long = "word ".repeat(500);
        let (main, _, cut) = selection_requests(&long, String::new, true);
        assert!(cut);
        assert!(main.text.chars().count() <= 2000);
        let one_long = "x".repeat(41);
        let (main, _, _) = selection_requests(&one_long, String::new, true);
        assert!(!main.is_word());
    }

    #[test]
    fn the_source_line_shows_the_start_of_a_long_selection() {
        assert_eq!(source_line("a\n b"), "a b");
        let long = "x".repeat(100);
        let s = source_line(&long);
        assert_eq!(s.chars().count(), SOURCE_CHARS + 1);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn placeholders_and_failures_in_the_popup() {
        let hint = FailureHint {
            backend: refdesk_core::TranslateBackendKind::Ollama,
            endpoint: "http://127.0.0.1:11434".into(),
            model: "m".into(),
        };
        let running = PartState::Running;
        assert_eq!(
            part_text("감시", &running, &Readiness::Ready, &hint),
            "감시"
        );
        assert_eq!(
            part_text("", &running, &Readiness::Ready, &hint),
            tr("translate.working")
        );
        assert_eq!(
            part_text("", &running, &Readiness::Loading, &hint),
            tr("translate.loading_model")
        );
        // Without translation mode the model loads with the request.
        assert_eq!(
            part_text("", &running, &Readiness::Off, &hint),
            tr("translate.loading_model")
        );
        let failed = PartState::Failed(TranslateError::ModelMissing("m".into()));
        assert!(part_text("", &failed, &Readiness::Ready, &hint).contains("ollama pull m"));
    }

    // ---- size and place ----

    const VIEW: Size = Size {
        w: 1000.0,
        h: 800.0,
    };

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn the_width_follows_the_content_between_the_limits() {
        assert!(close(clamp_size(Size::new(300.0, 80.0), VIEW).w, 300.0));
        assert!(close(clamp_size(Size::new(100.0, 80.0), VIEW).w, MIN_WIDTH));
        assert!(close(clamp_size(Size::new(900.0, 80.0), VIEW).w, MAX_WIDTH));
        // 60% of a narrow view.
        let narrow = Size::new(500.0, 800.0);
        assert!(close(max_width(narrow), 300.0));
        assert!(close(clamp_size(Size::new(900.0, 80.0), narrow).w, 300.0));
        // A view narrower than the lower limit.
        let tiny = Size::new(200.0, 800.0);
        assert!(close(clamp_size(Size::new(900.0, 80.0), tiny).w, 200.0));
        // At most half the view high.
        assert!(close(clamp_size(Size::new(300.0, 900.0), VIEW).h, 400.0));
    }

    #[test]
    fn the_popup_is_below_the_word_or_above_when_there_is_no_room() {
        let word = Rect::new(300.0, 100.0, 380.0, 114.0);
        let r = popup_rect(word, VIEW, Size::new(300.0, 100.0));
        assert!(r.y0 >= word.y1 && !r.intersects(&word));
        assert!(close(r.width(), 300.0) && close(r.height(), 100.0));
        let low = Rect::new(300.0, 760.0, 380.0, 774.0);
        let r = popup_rect(low, VIEW, Size::new(300.0, 100.0));
        assert!(r.y1 <= low.y0 && !r.intersects(&low));
        assert!(r.y0 >= 0.0);
    }

    #[test]
    fn the_popup_stays_in_the_view_at_the_edges_and_off_its_target() {
        let left = Rect::new(0.0, 100.0, 30.0, 114.0);
        let r = popup_rect(left, VIEW, Size::new(400.0, 100.0));
        assert!(close(r.x0, 0.0));
        let right = Rect::new(970.0, 100.0, 1000.0, 114.0);
        let r = popup_rect(right, VIEW, Size::new(400.0, 100.0));
        assert!(close(r.x1, 1000.0));
        for target in [left, right, Rect::new(400.0, 390.0, 600.0, 404.0)] {
            let r = popup_rect(target, VIEW, Size::new(560.0, 600.0));
            assert!(!r.intersects(&target), "{target:?} {r:?}");
            assert!(r.x0 >= 0.0 && r.x1 <= VIEW.w + 1e-3);
            assert!(r.y0 >= -1e-3 && r.y1 <= VIEW.h + 1e-3);
        }
    }
}

/// The popup on real documents, frame by frame, with a fake backend.
#[cfg(test)]
mod widget_tests {
    use super::*;
    use std::time::Instant;

    use egui::{Event, Modifiers, PointerButton, Pos2, pos2};
    use refdesk_core::testutil::{PdfBuilder, TempPdf};
    use refdesk_core::{TranslateKind, TranslateRequest};

    use crate::action::{OpenTarget, dispatch, dispatch_queued};
    use crate::overlays::palette::PaletteMode;
    use crate::overlays::translate::fake::{self, Log};
    use crate::overlays::translate::{self, TranslateApp};
    use crate::panels::status_bar;
    use crate::state::{AppState, PdfTab, Tab, TabId};
    use crate::views::preload;
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};
    use crate::views::textures::collect_rendered;

    const SCREEN: egui::Vec2 = egui::vec2(900.0, 700.0);
    const SENTENCE: &str = "The watchdog supervises the liveness of each process.";

    struct Harness {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        log: Arc<Log>,
        time: f64,
        copied: Vec<String>,
        /// Labels and values of the accessibility tree of the last frame, with their bounds.
        nodes: Vec<(String, egui::Rect)>,
    }

    impl Harness {
        fn new() -> Harness {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            let log = Log::new();
            let mut state = AppState::new();
            state.translate = TranslateApp::new(fake::factory(&log));
            translate::start(&mut state, &ctx);
            Harness {
                ctx,
                repaints,
                state,
                log,
                time: 1.0,
                copied: Vec::new(),
                nodes: Vec::new(),
            }
        }

        fn open(&mut self, path: &std::path::Path) -> TabId {
            let target = OpenTarget {
                path: path.to_path_buf(),
                page: None,
            };
            dispatch(&mut self.state, Action::OpenFiles(vec![target]), &self.ctx);
            self.state.focus.expect("focused")
        }

        fn act(&mut self, action: Action) {
            dispatch(&mut self.state, action, &self.ctx);
        }

        /// A frame at time `at` with these events, drawn like a window: the status bar and the
        /// focused tab with its translation environment.
        fn frame_at(&mut self, at: f64, events: Vec<Event>) {
            self.time = at;
            self.ctx.enable_accesskit();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, SCREEN)),
                time: Some(at),
                events,
                ..Default::default()
            };
            collect_rendered(&self.ctx, &self.state.docs, &mut self.state.textures);
            preload::step(&self.ctx, &mut self.state);
            let state = &mut self.state;
            let mut queue = Vec::new();
            let output = self.ctx.run_ui(input, |ui| {
                translate::tick(state, ui.ctx());
                queue.extend(crate::commands::handle_shortcuts(ui.ctx(), state));
                egui::Panel::bottom("status").show(ui, |ui| {
                    status_bar::show(ui, state, state.focus, &mut queue);
                });
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| draw_active(ui, state, &mut queue));
            });
            for command in &output.platform_output.commands {
                if let egui::OutputCommand::CopyText(text) = command {
                    self.copied.push(text.clone());
                }
            }
            self.nodes = output
                .platform_output
                .accesskit_update
                .clone()
                .map(|u| u.nodes)
                .unwrap_or_default()
                .into_iter()
                .flat_map(|(_, n)| {
                    let rect = n.bounds().map_or(egui::Rect::NOTHING, |b| {
                        egui::Rect::from_min_max(
                            pos2(b.x0 as f32, b.y0 as f32),
                            pos2(b.x1 as f32, b.y1 as f32),
                        )
                    });
                    [n.label().map(str::to_owned), n.value().map(str::to_owned)]
                        .into_iter()
                        .flatten()
                        .map(move |t| (t, rect))
                })
                .collect();
            output.drop_without_applying_deltas();
            self.state.actions.append(&mut queue);
            dispatch_queued(&mut self.state, &self.ctx);
        }

        /// Frames at the same time until `check` holds, waiting for repaints in between. The
        /// clock does not move, so no delay passes while waiting.
        fn frames_until(&mut self, what: &str, mut check: impl FnMut(&Harness) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.frame_at(self.time, Vec::new());
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        fn tab(&self, tab: TabId) -> &PdfTab {
            self.state.tabs.pdf(tab).expect("pdf tab")
        }

        fn popup(&self, tab: TabId) -> Option<&Popup> {
            self.tab(tab).ui.translate.popup()
        }

        fn main_text(&self, tab: TabId) -> Option<String> {
            self.popup(tab).map(|p| p.main.text.clone())
        }

        fn page_text(&self, tab: TabId, page: u32) -> Arc<PageText> {
            let doc = self.state.docs.get(self.tab(tab).doc).expect("doc");
            doc.page_text_wait(page).expect("page text")
        }

        /// The character range of the first `needle` on `page`.
        fn find(&self, tab: TabId, page: u32, needle: &str) -> Range<u32> {
            let text = self.page_text(tab, page);
            let all: String = text.chars.iter().map(|c| c.ch).collect();
            let byte = all.find(needle).expect("needle on the page");
            let start = all[..byte].chars().count() as u32;
            start..start + needle.chars().count() as u32
        }

        /// The screen position of the middle of character `i` of `page`.
        fn char_pos(&self, tab: TabId, page: u32, i: u32) -> Pos2 {
            let b = self.page_text(tab, page).chars[i as usize].quad.bounds();
            let ui = &self.tab(tab).ui;
            let layout = ui.layout().expect("layout");
            let r = layout.page_rect(page);
            let s = layout.scale();
            let scroll = ui.scroll();
            pos2(
                r.x0 + (b.x0 + b.x1) / 2.0 * s - scroll.x,
                r.y0 + (b.y0 + b.y1) / 2.0 * s - scroll.y,
            )
        }

        fn has_node(&self, text: &str) -> bool {
            self.nodes.iter().any(|(t, _)| t == text)
        }

        fn node_rect(&self, text: &str) -> Option<egui::Rect> {
            self.nodes.iter().find(|(t, _)| t == text).map(|(_, r)| *r)
        }

        fn select(&mut self, tab: TabId, selection: Selection) {
            if let Some(Tab::Pdf(pdf)) = self.state.tabs.get_mut(tab) {
                pdf.ui.selection.set_for_test(selection);
            }
        }

        fn mode_on(&mut self) {
            self.act(Action::ToggleTranslateMode);
            self.frames_until("the model", |h| {
                h.state.translate.readiness == translate::Readiness::Ready
            });
        }

        fn click(&mut self, at: Pos2) {
            self.frame_at(
                self.time,
                vec![Event::PointerMoved(at), pointer_button(at, true)],
            );
            self.frame_at(self.time, vec![pointer_button(at, false)]);
        }
    }

    fn pointer_button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    fn draw_active(ui: &mut Ui, state: &mut AppState, queue: &mut Vec<Action>) {
        let Some(active) = state.main_window().dock.focused_tab() else {
            return;
        };
        let overlay_open = state.palette.open.is_some();
        let env = state.translate.view_env(overlay_open);
        let AppState {
            docs,
            tabs,
            textures,
            focus,
            ..
        } = state;
        if let Some(Tab::Pdf(tab)) = tabs.get_mut(active)
            && let Some(doc) = docs.get(tab.doc)
        {
            tab.ui.translate.env = Some(env);
            let cx = crate::views::pdf_view::ViewContext {
                tab_id: active,
                focused: *focus == Some(active),
                textures,
                queue,
            };
            let history = refdesk_core::SearchHistory::default();
            crate::windows::draw_pdf_tab(ui, tab, doc, cx, false, overlay_open, &history);
        }
    }

    fn document(stem: &str) -> TempPdf {
        PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, SENTENCE);
                p.text(72.0, 160.0, 12.0, "https://example.com/");
                p.link_uri((72.0, 150.0, 250.0, 164.0), "https://example.com/");
                p.text(72.0, 240.0, 12.0, "A long speci-");
                p.text(72.0, 256.0, 12.0, "fication of the thing.");
            })
            .write_temp(stem)
    }

    /// A harness with `document` open, laid out, its first page's text and links there.
    fn opened(stem: &str) -> (Harness, TabId, TempPdf) {
        let pdf = document(stem);
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the page", |h| {
            let doc = h.state.docs.get(h.tab(tab).doc).expect("doc");
            h.tab(tab).ui.layout().is_some()
                && doc.page_text(0).is_some()
                && doc.page_links(0).is_some()
        });
        (h, tab, pdf)
    }

    /// Rests the pointer on the middle of `word` from `t`; gives its position.
    fn rest_on(h: &mut Harness, tab: TabId, word: &str, t: f64) -> Pos2 {
        let range = h.find(tab, 0, word);
        let pos = h.char_pos(tab, 0, range.start + 1);
        h.frame_at(t, vec![Event::PointerMoved(pos)]);
        pos
    }

    /// Opens the popup of "watchdog" and waits for its answers.
    fn open_watchdog(h: &mut Harness, tab: TabId) -> Pos2 {
        let pos = rest_on(h, tab, "watchdog", 10.0);
        h.frame_at(10.6, Vec::new());
        assert!(h.popup(tab).is_some(), "the popup opened");
        pos
    }

    // ---- hovering ----

    #[test]
    fn resting_on_a_word_asks_for_the_word_in_its_sentence_and_the_sentence() {
        let (mut h, tab, _pdf) = opened("tr-hover");
        h.mode_on();
        rest_on(&mut h, tab, "watchdog", 10.0);
        h.frame_at(10.3, Vec::new());
        h.frame_at(10.49, Vec::new());
        assert!(h.popup(tab).is_none());
        assert!(
            h.log.requests().is_empty(),
            "nothing asked before half a second"
        );
        h.frame_at(10.51, Vec::new());
        assert!(h.popup(tab).is_some());
        h.log.wait("two requests", |i| i.requests.len() == 2);
        let reqs: Vec<TranslateRequest> = h.log.requests().into_iter().map(|(_, r)| r).collect();
        assert!(
            reqs.contains(&TranslateRequest::word("watchdog", SENTENCE)),
            "{reqs:?}"
        );
        assert!(
            reqs.contains(&TranslateRequest::passage(SENTENCE)),
            "{reqs:?}"
        );
        h.frames_until("the answers", |h| {
            h.popup(tab).is_some_and(|p| !p.running())
        });
        let popup = h.popup(tab).expect("popup");
        assert_eq!(popup.main.text, "뜻[watchdog]");
        assert_eq!(
            popup.sentence.as_ref().map(|p| p.text.clone()),
            Some(format!("번역[{SENTENCE}]"))
        );
        // The popup's texts are in the accessibility tree.
        h.frame_at(h.time, Vec::new());
        assert!(h.has_node("뜻[watchdog]"), "{:?}", h.nodes);
        assert!(h.has_node(&format!("번역[{SENTENCE}]")));
    }

    #[test]
    fn without_the_sentence_setting_only_the_word_is_asked() {
        let (mut h, tab, _pdf) = opened("tr-word-only");
        h.state.translate.settings.word_with_sentence = false;
        h.mode_on();
        open_watchdog(&mut h, tab);
        h.log.wait("one request", |i| !i.requests.is_empty());
        h.frames_until("the answer", |h| h.popup(tab).is_some_and(|p| !p.running()));
        assert_eq!(h.log.requests().len(), 1);
        assert!(h.popup(tab).expect("popup").sentence.is_none());
    }

    #[test]
    fn moving_inside_a_word_keeps_the_clock_and_another_word_restarts_it() {
        let (mut h, tab, _pdf) = opened("tr-move");
        h.mode_on();
        let range = h.find(tab, 0, "watchdog");
        h.frame_at(
            10.0,
            vec![Event::PointerMoved(h.char_pos(tab, 0, range.start))],
        );
        h.frame_at(
            10.3,
            vec![Event::PointerMoved(h.char_pos(tab, 0, range.end - 1))],
        );
        h.frame_at(10.51, Vec::new());
        assert!(h.popup(tab).is_some(), "the same word: opened on time");
        // Another word starts again.
        let other = h.find(tab, 0, "liveness");
        h.frame_at(
            11.0,
            vec![Event::PointerMoved(h.char_pos(tab, 0, other.start + 2))],
        );
        h.frame_at(11.3, Vec::new());
        assert!(h.popup(tab).is_none());
        h.frame_at(11.51, Vec::new());
        assert!(matches!(
            h.popup(tab).map(|p| &p.target),
            Some(Target::Word(k)) if k.start == other.start
        ));
    }

    #[test]
    fn no_word_popup_on_a_link_while_dragging_with_the_palette_or_without_the_mode() {
        let (mut h, tab, _pdf) = opened("tr-blocked");
        // Mode off.
        rest_on(&mut h, tab, "watchdog", 10.0);
        h.frame_at(11.0, Vec::new());
        assert!(h.popup(tab).is_none());
        h.mode_on();
        // On a link.
        let link = h.find(tab, 0, "example");
        h.frame_at(
            20.0,
            vec![Event::PointerMoved(h.char_pos(tab, 0, link.start))],
        );
        h.frame_at(21.0, Vec::new());
        assert!(h.popup(tab).is_none());
        // While the button is held.
        let range = h.find(tab, 0, "liveness");
        let at = h.char_pos(tab, 0, range.start + 1);
        h.frame_at(
            30.0,
            vec![Event::PointerMoved(at), pointer_button(at, true)],
        );
        h.frame_at(31.0, Vec::new());
        assert!(h.popup(tab).is_none());
        h.frame_at(31.0, vec![pointer_button(at, false)]);
        // With the palette open.
        h.act(Action::ShowPalette(PaletteMode::Commands));
        rest_on(&mut h, tab, "watchdog", 40.0);
        h.frame_at(41.0, Vec::new());
        assert!(h.popup(tab).is_none());
        assert!(h.log.requests().is_empty());
    }

    // ---- closing ----

    #[test]
    fn the_pointer_may_move_onto_the_popup_and_leaving_both_closes_it_and_cancels() {
        let (mut h, tab, _pdf) = opened("tr-close");
        h.log.set(|i| i.manual = true);
        h.mode_on();
        open_watchdog(&mut h, tab);
        h.log.wait("the requests", |i| i.requests.len() == 2);
        // Draw once so that the popup has its size.
        h.frame_at(10.61, Vec::new());
        let view = h.tab(tab).ui.viewport();
        let view_rect = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(view.w, view.h));
        let popup = popup_screen_rect(&h.tab(tab).ui, view_rect).expect("popup on screen");
        // Off the word, onto the popup within 0.2 s: it stays.
        let empty = pos2(view_rect.max.x - 5.0, view_rect.max.y - 5.0);
        h.frame_at(11.0, vec![Event::PointerMoved(empty)]);
        h.frame_at(11.1, vec![Event::PointerMoved(popup.center())]);
        h.frame_at(13.0, Vec::new());
        assert!(h.popup(tab).is_some(), "kept while on the popup");
        // Off both for more than 0.2 s: closed, and the jobs are dropped.
        h.frame_at(14.0, vec![Event::PointerMoved(empty)]);
        h.frame_at(14.1, Vec::new());
        assert!(h.popup(tab).is_some());
        h.frame_at(14.25, Vec::new());
        assert!(h.popup(tab).is_none());
        h.log.wait("the cancels", |i| i.cancelled == 2);
    }

    #[test]
    fn scrolling_clicking_or_a_key_closes_a_word_popup_at_once() {
        let events: [fn(Pos2) -> Vec<Event>; 3] = [
            |_| {
                vec![Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -40.0),
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Unknown,
                    modifiers: Modifiers::NONE,
                }]
            },
            |at| vec![pointer_button(at, true), pointer_button(at, false)],
            |_| {
                vec![Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }]
            },
        ];
        for (n, make) in events.into_iter().enumerate() {
            let (mut h, tab, _pdf) = opened(&format!("tr-interrupt-{n}"));
            h.mode_on();
            let at = open_watchdog(&mut h, tab);
            h.frame_at(10.7, make(at));
            assert!(h.popup(tab).is_none(), "event {n}");
        }
    }

    // ---- selections ----

    #[test]
    fn finishing_a_drag_in_translation_mode_translates_the_selection() {
        let (mut h, tab, _pdf) = opened("tr-drag");
        h.mode_on();
        let range = h.find(tab, 0, "watchdog supervises");
        let from = h.char_pos(tab, 0, range.start);
        let to = h.char_pos(tab, 0, range.end - 1);
        h.frame_at(
            10.0,
            vec![Event::PointerMoved(from), pointer_button(from, true)],
        );
        h.frame_at(10.1, vec![Event::PointerMoved(to)]);
        h.frames_until("the selection", |h| h.tab(tab).ui.has_selection());
        assert!(h.popup(tab).is_none(), "not while dragging");
        h.frame_at(10.2, vec![pointer_button(to, false)]);
        assert!(matches!(
            h.popup(tab).map(|p| &p.target),
            Some(Target::Selection(_))
        ));
        h.log.wait("the request", |i| !i.requests.is_empty());
        let (_, req) = h.log.requests()[0].clone();
        assert_eq!(req.kind, TranslateKind::Passage);
        assert!(req.text.contains("watchdog supervise"), "{}", req.text);
        // The selection popup stays, even long after.
        h.frame_at(30.0, Vec::new());
        assert!(h.popup(tab).is_some());
        // Esc closes it.
        h.frame_at(
            30.1,
            vec![Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        assert!(h.popup(tab).is_none());
    }

    #[test]
    fn a_selection_over_lines_is_one_paragraph_and_one_word_is_a_word() {
        let (mut h, tab, _pdf) = opened("tr-lines");
        let start = h.find(tab, 0, "A long").start;
        let end = h.find(tab, 0, "thing.").end;
        h.select(
            tab,
            Selection {
                anchor: refdesk_core::TextPos { page: 0, ch: start },
                focus: refdesk_core::TextPos { page: 0, ch: end },
            },
        );
        // Translation mode is off: the command still translates once.
        h.act(Action::TranslateSelection(tab));
        h.log.wait("the passage", |i| !i.requests.is_empty());
        assert_eq!(
            h.log.requests()[0].1,
            TranslateRequest::passage("A long specification of the thing.")
        );

        let word = h.find(tab, 0, "liveness");
        h.select(
            tab,
            Selection {
                anchor: refdesk_core::TextPos {
                    page: 0,
                    ch: word.start,
                },
                focus: refdesk_core::TextPos {
                    page: 0,
                    ch: word.end,
                },
            },
        );
        h.act(Action::TranslateSelection(tab));
        // The word and its sentence go out on two threads, in either order.
        h.log
            .wait("the word and its sentence", |i| i.requests.len() >= 3);
        assert!(
            h.log
                .requests()
                .iter()
                .any(|(_, r)| *r == TranslateRequest::word("liveness", SENTENCE)),
            "{:?}",
            h.log.requests()
        );
        // The selection goes: so does the popup.
        h.frame_at(5.0, Vec::new());
        assert!(h.popup(tab).is_some());
        if let Some(Tab::Pdf(pdf)) = h.state.tabs.get_mut(tab) {
            pdf.ui.selection.clear();
        }
        h.frame_at(5.1, Vec::new());
        assert!(h.popup(tab).is_none());
    }

    #[test]
    fn a_selection_over_two_thousand_characters_is_cut_and_says_so() {
        let mut b = PdfBuilder::new();
        b = b.page(612.0, 792.0, |p| {
            for i in 0..40 {
                let text = format!("line{i:02} alpha beta gamma delta epsilon zeta eta theta iota");
                // Close together so that the last line, where the popup sits, is on screen.
                p.text(40.0, 20.0 + i as f32 * 8.0, 7.0, &text);
            }
        });
        let pdf = b.write_temp("tr-long");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the page", |h| {
            let doc = h.state.docs.get(h.tab(tab).doc).expect("doc");
            h.tab(tab).ui.layout().is_some() && doc.page_text(0).is_some()
        });
        let n = h.page_text(tab, 0).chars.len() as u32;
        assert!(n > 2000, "{n}");
        h.select(
            tab,
            Selection {
                anchor: refdesk_core::TextPos { page: 0, ch: 0 },
                focus: refdesk_core::TextPos { page: 0, ch: n },
            },
        );
        h.act(Action::TranslateSelection(tab));
        h.log.wait("the passage", |i| !i.requests.is_empty());
        let (_, req) = h.log.requests()[0].clone();
        assert!(req.text.chars().count() <= 2000);
        assert!(h.popup(tab).expect("popup").truncated);
        h.frame_at(2.0, Vec::new());
        h.frame_at(2.0, Vec::new());
        assert!(h.has_node(tr("translate.truncated")), "{:?}", h.nodes);
    }

    // ---- streaming and the cache ----

    #[test]
    fn the_text_grows_as_pieces_come_and_the_result_is_remembered() {
        let (mut h, tab, _pdf) = opened("tr-stream");
        h.log.set(|i| i.manual = true);
        let range = h.find(tab, 0, "watchdog supervises");
        let selection = Selection {
            anchor: refdesk_core::TextPos {
                page: 0,
                ch: range.start,
            },
            focus: refdesk_core::TextPos {
                page: 0,
                ch: range.end,
            },
        };
        h.select(tab, selection);
        h.act(Action::TranslateSelection(tab));
        h.log.wait("the request", |i| i.requests.len() == 1);
        h.frame_at(2.0, Vec::new());
        assert_eq!(h.main_text(tab).as_deref(), Some(""));
        h.log.feed("감시자는");
        h.frames_until("the first piece", |h| {
            h.main_text(tab).as_deref() == Some("감시자는")
        });
        h.log.feed(" 감독한다");
        h.frames_until("the second piece", |h| {
            h.main_text(tab).as_deref() == Some("감시자는 감독한다")
        });
        h.log.end();
        h.frames_until("the end", |h| h.popup(tab).is_some_and(|p| !p.running()));
        let req = h.popup(tab).expect("popup").main.req.clone();
        let identity = h.state.translate.backend.identity();
        assert!(
            h.state
                .translate
                .cache
                .lock()
                .unwrap()
                .contains(&identity, &req)
        );

        // Asked again: from the cache, at once, without the backend.
        h.act(Action::TranslateSelection(tab));
        let popup = h.popup(tab).expect("popup");
        assert!(popup.main.cached);
        assert_eq!(popup.main.text, "감시자는 감독한다");
        assert_eq!(h.log.requests().len(), 1);
    }

    #[test]
    fn a_failed_translation_shows_why_and_is_not_remembered() {
        let (mut h, tab, _pdf) = opened("tr-fail");
        h.log
            .set(|i| i.fail = Some(refdesk_core::TranslateError::ModelMissing("m".into())));
        let range = h.find(tab, 0, "watchdog supervises");
        h.select(
            tab,
            Selection {
                anchor: refdesk_core::TextPos {
                    page: 0,
                    ch: range.start,
                },
                focus: refdesk_core::TextPos {
                    page: 0,
                    ch: range.end,
                },
            },
        );
        h.act(Action::TranslateSelection(tab));
        h.frames_until("the failure", |h| {
            h.popup(tab)
                .is_some_and(|p| matches!(p.main.state, PartState::Failed(_)))
        });
        let req = h.popup(tab).expect("popup").main.req.clone();
        let identity = h.state.translate.backend.identity();
        assert!(
            !h.state
                .translate
                .cache
                .lock()
                .unwrap()
                .contains(&identity, &req)
        );
        h.frame_at(h.time, Vec::new());
        assert!(
            h.nodes
                .iter()
                .any(|(t, _)| t.contains("ollama pull translategemma:4b")),
            "{:?}",
            h.nodes
        );
    }

    #[test]
    fn the_copy_button_puts_the_translation_on_the_clipboard() {
        let (mut h, tab, _pdf) = opened("tr-copy");
        h.mode_on();
        open_watchdog(&mut h, tab);
        h.frames_until("the answer", |h| h.popup(tab).is_some_and(|p| !p.running()));
        h.frame_at(10.7, Vec::new());
        let button = h.node_rect(tr("translate.copy")).expect("the copy button");
        h.frame_at(10.75, vec![Event::PointerMoved(button.center())]);
        h.click(button.center());
        assert!(
            h.copied.contains(&"뜻[watchdog]".to_string()),
            "{:?}",
            h.copied
        );
    }

    // ---- the mode and the status bar ----

    #[test]
    fn the_status_bar_shows_the_button_and_the_readiness() {
        let (mut h, tab, _pdf) = opened("tr-status");
        h.frame_at(2.0, Vec::new());
        assert!(h.has_node(tr("translate.button_off")), "{:?}", h.nodes);
        h.log.set(|i| i.warm_hold = true);
        h.act(Action::ToggleTranslateMode);
        h.frame_at(2.1, Vec::new());
        assert!(h.has_node(tr("translate.button_on")), "{:?}", h.nodes);
        assert!(h.has_node(tr("translate.loading_model")), "{:?}", h.nodes);
        h.log.set(|i| i.warm_hold = false);
        h.frames_until("ready", |h| {
            h.state.translate.readiness == translate::Readiness::Ready
        });
        h.frame_at(2.2, Vec::new());
        assert!(!h.has_node(tr("translate.loading_model")));
        // Clicking the button switches the mode off again.
        let button = h.node_rect(tr("translate.button_on")).expect("button");
        h.click(button.center());
        assert!(!h.state.translate.mode);
        let _ = tab;
    }

    #[test]
    fn switching_the_mode_off_closes_the_popup_and_drops_its_jobs() {
        let (mut h, tab, _pdf) = opened("tr-off");
        h.log.set(|i| i.manual = true);
        h.mode_on();
        open_watchdog(&mut h, tab);
        h.log.wait("the requests", |i| i.requests.len() == 2);
        h.act(Action::ToggleTranslateMode);
        assert!(h.popup(tab).is_none());
        h.log.wait("the cancels", |i| i.cancelled == 2);
    }

    #[test]
    fn a_new_model_closes_the_popup_and_the_next_request_uses_it() {
        let (mut h, tab, _pdf) = opened("tr-model");
        h.mode_on();
        open_watchdog(&mut h, tab);
        h.log.wait("the requests", |i| i.requests.len() == 2);
        h.act(Action::SetTranslateModel(Some("qwen2.5:7b".into())));
        assert!(h.popup(tab).is_none());
        h.frames_until("ready", |h| {
            h.state.translate.readiness == translate::Readiness::Ready
        });
        // The same word again (away first, so the timer starts over).
        h.frame_at(20.0, vec![Event::PointerMoved(pos2(5.0, 5.0))]);
        rest_on(&mut h, tab, "watchdog", 21.0);
        h.frame_at(21.6, Vec::new());
        h.log.wait("new requests", |i| i.requests.len() == 4);
        let models: Vec<String> = h.log.requests().into_iter().map(|(m, _)| m).collect();
        assert_eq!(&models[2..], ["qwen2.5:7b", "qwen2.5:7b"]);
    }

    #[test]
    fn the_shortcuts_run_the_two_commands() {
        let (mut h, tab, _pdf) = opened("tr-keys");
        let key = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        h.frame_at(2.0, vec![key(egui::Key::F2, Modifiers::NONE)]);
        assert!(h.state.translate.mode);
        let range = h.find(tab, 0, "watchdog supervises");
        h.select(
            tab,
            Selection {
                anchor: refdesk_core::TextPos {
                    page: 0,
                    ch: range.start,
                },
                focus: refdesk_core::TextPos {
                    page: 0,
                    ch: range.end,
                },
            },
        );
        h.frame_at(2.1, vec![key(egui::Key::F2, Modifiers::SHIFT)]);
        assert!(h.state.translate.mode, "Shift+F2 is not F2");
        assert!(matches!(
            h.popup(tab).map(|p| &p.target),
            Some(Target::Selection(_))
        ));
    }
}
