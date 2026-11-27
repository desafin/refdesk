//! Screen capture (SHOT-1 .. SHOT-4, `docs/ARCHITECTURE.md` §5.7).
//!
//! The capture command puts the focused PDF view into capture mode. A drag on a page draws a
//! rectangle; on release the rectangle becomes a page-space [`Rect`] (rule R-5) and a background
//! thread renders that region again through `render_region_wait` (rule R-1). The pixels of the
//! screen are never used, so the result is sharp whatever the zoom is. The image goes to the
//! clipboard, a notice with a "Save" button appears, and saving writes a PNG file.
//!
//! The decisions are plain functions on plain data, tested without egui (rule R-10): screen
//! rectangle to page rectangle, render scale and its limits, file names, a path that does not
//! overwrite anything, which clipboard method to try, and the transitions of the capture mode.
//! The egui part reads input and paints. The clipboard sits behind the [`Clipboard`] trait and
//! the save folder comes from the state, so tests use a fake clipboard and a temporary folder.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CursorIcon, Key, Modifiers, PointerButton, Stroke, StrokeKind, Ui};
use refdesk_core::{DocumentHandle, Location, PageLayout, Point, Rect, RenderedImage, Size};

use crate::action::Action;
use crate::i18n::tr;
use crate::state::{AppState, Tab, TabId};
use crate::views::selection;

/// A side of the dragged rectangle shorter than this many logical pixels cancels the capture.
pub const MIN_SIDE: f32 = 4.0;
/// The longest side of a captured image in pixels; the scale is lowered to stay within it.
pub const MAX_SIDE_PX: u32 = 8000;
/// The engine renders the whole page and crops it; it refuses pages above this many pixels.
const MAX_PAGE_PIXELS: u64 = 120_000_000;
/// The capture scale relative to the screen scale when the setting is unusable (SHOT-2).
pub const DEFAULT_FACTOR: f32 = 2.0;
/// Seconds a notice stays up.
pub const NOTICE_SECS: f64 = 6.0;
/// How long `wl-copy` may take before the next clipboard method is tried.
pub const WL_COPY_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest part of a document name used in a file name, in characters.
const MAX_NAME_CHARS: usize = 80;

// ------------------------------------------------------------------------------------------------
// Rectangles
// ------------------------------------------------------------------------------------------------

/// What a drag selects: a rectangle on one page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureRegion {
    /// The page the drag started on (zero-based).
    pub page: u32,
    /// The rectangle in page space (pt), inside the page.
    pub rect: Rect,
    /// The same rectangle relative to the top-left corner of the view, in logical pixels.
    pub screen: Rect,
}

impl CaptureRegion {
    /// Whether a side is shorter than [`MIN_SIDE`] logical pixels: the drag was a click or a
    /// slip, not a selection.
    pub fn is_too_small(&self) -> bool {
        self.screen.width() < MIN_SIDE || self.screen.height() < MIN_SIDE
    }
}

/// The rectangle a drag covers. `start` is where the button went down (page space, so scrolling
/// during the drag keeps it on the same spot of the page); `pointer` is where the pointer is now,
/// relative to the top-left corner of the view. The rectangle is limited to the page the drag
/// started on, even when the pointer is on the next page. `None` for a page the layout does not
/// have.
pub fn region_from_drag(
    layout: &PageLayout,
    scroll: Point,
    start: Location,
    pointer: Point,
) -> Option<CaptureRegion> {
    let scale = layout.scale();
    if start.page >= layout.page_count() || !(scale.is_finite() && scale > 0.0) {
        return None;
    }
    let page = layout.page_rect(start.page);
    let size = Size::new(page.width() / scale, page.height() / scale);
    // The pointer in the coordinates of the start page, wherever the pointer really is.
    let end = Point::new(
        (scroll.x + pointer.x - page.x0) / scale,
        (scroll.y + pointer.y - page.y0) / scale,
    );
    let a = Point::new(start.x, start.y);
    let rect = clamp_rect(Rect::from_points(a, end), size);
    let screen = Rect::new(
        page.x0 + rect.x0 * scale - scroll.x,
        page.y0 + rect.y0 * scale - scroll.y,
        page.x0 + rect.x1 * scale - scroll.x,
        page.y0 + rect.y1 * scale - scroll.y,
    );
    Some(CaptureRegion {
        page: start.page,
        rect,
        screen,
    })
}

/// `rect` limited to the page `[0, size.w] × [0, size.h]`. A rectangle outside the page becomes a
/// zero-width or zero-height one on its edge.
fn clamp_rect(rect: Rect, size: Size) -> Rect {
    let cx = |v: f32| v.clamp(0.0, size.w.max(0.0));
    let cy = |v: f32| v.clamp(0.0, size.h.max(0.0));
    Rect::new(cx(rect.x0), cy(rect.y0), cx(rect.x1), cy(rect.y1))
}

/// The whole of a page as a capture rectangle (SHOT-4). `None` for a page the document does not
/// have.
pub fn whole_page(page_sizes: &[Size], page: u32) -> Option<Rect> {
    let size = page_sizes.get(page as usize)?;
    Some(Rect::new(0.0, 0.0, size.w, size.h))
}

// ------------------------------------------------------------------------------------------------
// Scale
// ------------------------------------------------------------------------------------------------

/// The setting `capture.scale` made safe to multiply with: a positive finite value, at most 16.
pub fn sanitize_factor(factor: f32) -> f32 {
    if factor.is_finite() && factor > 0.0 {
        factor.min(16.0)
    } else {
        DEFAULT_FACTOR
    }
}

/// The size in pixels of the image the engine makes for `rect` (page space) at `scale_milli`:
/// the same rounding as the engine's crop, with its tolerance for float noise.
pub fn pixel_size(rect: Rect, scale_milli: u32) -> (u32, u32) {
    let scale = scale_milli as f32 / 1000.0;
    let lo = |v: f32| (v * scale + 1e-3).floor().max(0.0) as u32;
    let hi = |v: f32| (v * scale - 1e-3).ceil().max(0.0) as u32;
    (
        hi(rect.x1).saturating_sub(lo(rect.x0)),
        hi(rect.y1).saturating_sub(lo(rect.y0)),
    )
}

/// The render scale (times 1000) for capturing `region` of a page of size `page`.
///
/// `screen_scale` is the scale the view renders at now (layout scale × pixels per point) and
/// `factor` the `capture.scale` setting (SHOT-2). The result is lowered so that the longest side
/// of the image stays within [`MAX_SIDE_PX`] and the engine can render the page behind it. It is
/// rounded up when it is not lowered, so an image is never smaller than the factor promises.
pub fn capture_scale_milli(screen_scale: f32, factor: f32, region: Rect, page: Size) -> u32 {
    let wanted = screen_scale.max(0.0) * sanitize_factor(factor);
    let mut milli = ((wanted * 1000.0).ceil() as u32).max(1);
    // The largest scale the limits allow, from their formulas; the loop below makes it exact.
    let longest = region.width().max(region.height());
    let mut limit = f32::INFINITY;
    if longest > 0.0 {
        limit = limit.min(MAX_SIDE_PX as f32 / longest);
    }
    let page_area = page.w * page.h;
    if page_area > 0.0 {
        limit = limit.min((MAX_PAGE_PIXELS as f32 / page_area).sqrt());
    }
    if limit.is_finite() {
        milli = milli.min(((limit * 1000.0).floor() as u32).max(1));
    }
    // Rounding up to whole pixels can add one; step down until the limits hold. The step count
    // is bounded: the limits are met within a few steps of the formula's answer.
    for _ in 0..20_000 {
        if milli <= 1 || within_limits(region, page, milli) {
            break;
        }
        milli -= 1;
    }
    milli.max(1)
}

fn within_limits(region: Rect, page: Size, milli: u32) -> bool {
    let (w, h) = pixel_size(region, milli);
    let scale = milli as f32 / 1000.0;
    let page_pixels = (page.w * scale).ceil() as u64 * (page.h * scale).ceil() as u64;
    w.max(h) <= MAX_SIDE_PX && page_pixels <= MAX_PAGE_PIXELS
}

// ------------------------------------------------------------------------------------------------
// File names and saving
// ------------------------------------------------------------------------------------------------

/// A local date and time, down to the second. Passed in so tests choose the time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// Year.
    pub year: i32,
    /// Month, 1 to 12.
    pub month: u8,
    /// Day of the month, 1 to 31.
    pub day: u8,
    /// Hour, 0 to 23.
    pub hour: u8,
    /// Minute.
    pub minute: u8,
    /// Second.
    pub second: u8,
}

impl Stamp {
    /// The current local time; UTC when the system time zone cannot be found.
    pub fn now() -> Stamp {
        let now = jiff::Zoned::now();
        // The fields of a `Zoned` are in range, so the conversions cannot fail.
        let small = |v: i8| u8::try_from(v).unwrap_or(0);
        Stamp {
            year: i32::from(now.year()),
            month: small(now.month()),
            day: small(now.day()),
            hour: small(now.hour()),
            minute: small(now.minute()),
            second: small(now.second()),
        }
    }
}

/// `name` with the characters a file name cannot hold replaced by `_`: the path separator, the
/// characters Windows and some file systems reject, and control characters. A name that is empty
/// or only dots becomes `capture`; a long one is cut.
pub fn sanitize_file_name(name: &str) -> String {
    let bad = |c: char| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    };
    let clean: String = name
        .chars()
        .map(|c| if bad(c) { '_' } else { c })
        .take(MAX_NAME_CHARS)
        .collect();
    let clean = clean.trim().to_string();
    if clean.chars().all(|c| c == '.') {
        "capture".to_string()
    } else {
        clean
    }
}

/// The file name of a capture without the extension: `name-p3-20260102-030405` for page index 2
/// (pages count from 1 in names).
pub fn capture_file_stem(doc_name: &str, page: u32, at: Stamp) -> String {
    format!(
        "{}-p{}-{:04}{:02}{:02}-{:02}{:02}{:02}",
        sanitize_file_name(doc_name),
        page + 1,
        at.year,
        at.month,
        at.day,
        at.hour,
        at.minute,
        at.second
    )
}

/// The path of the `n`-th candidate for `stem`: `stem.png`, then `stem-2.png`, `stem-3.png`.
fn numbered_path(dir: &Path, stem: &str, n: u32) -> PathBuf {
    if n <= 1 {
        dir.join(format!("{stem}.png"))
    } else {
        dir.join(format!("{stem}-{n}.png"))
    }
}

/// A path in `dir` for a capture named `stem` that `exists` does not know: `stem.png`, or
/// `stem-2.png`, `stem-3.png` … when taken.
pub fn unique_path(dir: &Path, stem: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let mut n = 1;
    loop {
        let path = numbered_path(dir, stem, n);
        if n == u32::MAX || !exists(&path) {
            return path;
        }
        n += 1;
    }
}

/// Writes `png` into `dir` (created when missing) under a name that is free, and returns the
/// path. Never overwrites a file, also when another capture takes the name meanwhile.
pub fn save_png(dir: &Path, stem: &str, png: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for _ in 0..1000 {
        let path = unique_path(dir, stem, |p| p.exists());
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match file {
            Ok(mut file) => {
                let written = file.write_all(png).and_then(|()| file.flush());
                if let Err(e) = written {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                return Ok(path);
            }
            // Someone else took the name since we looked; look again.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free file name"))
}

/// The part of a document's file name used in capture names: the name without its extension.
pub fn document_name(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Encodes an RGB image as PNG.
pub fn encode_png(image: &RenderedImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("png header: {e}"))?;
        writer
            .write_image_data(&image.rgb)
            .map_err(|e| format!("png data: {e}"))?;
        writer.finish().map_err(|e| format!("png end: {e}"))?;
    }
    Ok(out)
}

// ------------------------------------------------------------------------------------------------
// Clipboard
// ------------------------------------------------------------------------------------------------

/// A way of putting an image on the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// The PNG piped to `wl-copy --type image/png`: reaches other Wayland applications.
    WlCopy,
    /// egui's `copy_image`, which goes to the clipboard of the windowing library.
    EguiImage,
}

/// The methods to try, in order. `wl-copy` first when it exists, because egui's copy lands in the
/// X11 clipboard on this machine and other applications do not see it (`docs/ARCHITECTURE.md` §6).
pub fn plan_methods(wl_copy_available: bool) -> Vec<Method> {
    if wl_copy_available {
        vec![Method::WlCopy, Method::EguiImage]
    } else {
        vec![Method::EguiImage]
    }
}

/// The clipboard the capture copies to. The application uses [`SystemClipboard`]; tests use a
/// fake.
pub trait Clipboard: Send + Sync {
    /// Whether `wl-copy` can be run.
    fn wl_copy_available(&self) -> bool;
    /// Copies a PNG with `wl-copy`. An error, or taking longer than [`WL_COPY_TIMEOUT`], means
    /// the next method is tried. Blocks.
    fn copy_png_with_wl_copy(&self, png: &[u8]) -> Result<(), String>;
    /// Copies an image through egui.
    fn copy_image_with_egui(&self, image: &RenderedImage) -> Result<(), String>;
}

/// Copies with the first method of [`plan_methods`] that works and returns it. When none works,
/// the error lists what each reported. Blocks (R-1: not for the UI thread).
pub fn copy_with_fallback(
    clipboard: &dyn Clipboard,
    png: &[u8],
    image: &RenderedImage,
) -> Result<Method, String> {
    let mut errors = Vec::new();
    for method in plan_methods(clipboard.wl_copy_available()) {
        let result = match method {
            Method::WlCopy => clipboard.copy_png_with_wl_copy(png),
            Method::EguiImage => clipboard.copy_image_with_egui(image),
        };
        match result {
            Ok(()) => return Ok(method),
            Err(e) => {
                log::warn!("clipboard method {method:?} failed: {e}");
                errors.push(e);
            }
        }
    }
    Err(errors.join("; "))
}

/// The first directory of `path_var` (a `PATH` value) that has an executable file `name`.
pub fn find_in_path(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The real clipboard: `wl-copy` when the session is Wayland and the program is installed, else
/// egui.
pub struct SystemClipboard {
    ctx: egui::Context,
    wl_copy: Option<PathBuf>,
}

impl SystemClipboard {
    /// Looks for `wl-copy` on `PATH`; nothing is run.
    pub fn new(ctx: egui::Context) -> SystemClipboard {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
        let wl_copy = std::env::var_os("PATH")
            .filter(|_| wayland)
            .and_then(|path| find_in_path("wl-copy", &path));
        SystemClipboard { ctx, wl_copy }
    }
}

impl Clipboard for SystemClipboard {
    fn wl_copy_available(&self) -> bool {
        self.wl_copy.is_some()
    }

    fn copy_png_with_wl_copy(&self, png: &[u8]) -> Result<(), String> {
        let program = self.wl_copy.as_deref().ok_or("wl-copy was not found")?;
        run_wl_copy(program, png, WL_COPY_TIMEOUT)
    }

    fn copy_image_with_egui(&self, image: &RenderedImage) -> Result<(), String> {
        let size = [image.width as usize, image.height as usize];
        self.ctx
            .copy_image(egui::ColorImage::from_rgb(size, &image.rgb));
        Ok(())
    }
}

/// Runs `program --type image/png` with `png` on its standard input and waits for it to exit,
/// at most `timeout`. `wl-copy` puts a copy of the data in the background and exits, so it is
/// done quickly; one that hangs is killed.
fn run_wl_copy(program: &Path, png: &[u8], timeout: Duration) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run wl-copy: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("wl-copy has no input")?;
    // A separate thread writes, so a program that does not read cannot hold us past the timeout.
    let data = png.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("wl-copy took too long".to_string());
            }
            Err(e) => return Err(format!("wl-copy: {e}")),
        }
    };
    if !status.success() {
        return Err(format!("wl-copy exited with {status}"));
    }
    match writer.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("wl-copy input: {e}")),
        Err(_) => Err("wl-copy input thread failed".to_string()),
    }
}

// ------------------------------------------------------------------------------------------------
// The job on the background thread
// ------------------------------------------------------------------------------------------------

/// A PNG file in memory, shared between the notice and a save.
#[derive(Clone)]
pub struct Png(pub Arc<[u8]>);

impl std::fmt::Debug for Png {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Png({} bytes)", self.0.len())
    }
}

/// Everything a capture job needs besides the document and the clipboard.
#[derive(Clone, Debug)]
pub struct JobParams {
    /// The page (zero-based).
    pub page: u32,
    /// The region in page space.
    pub region: Rect,
    /// The render scale times 1000.
    pub scale_milli: u32,
    /// The document's name for file names.
    pub doc_name: String,
    /// Where PNG files go; `None` disables saving.
    pub save_dir: Option<PathBuf>,
    /// Save without being asked (`capture.always_save`).
    pub always_save: bool,
    /// The time in the file name.
    pub stamp: Stamp,
}

/// A finished capture.
#[derive(Clone, Debug)]
pub struct Shot {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The image as PNG, for saving.
    pub png: Png,
    /// The file name without extension.
    pub stem: String,
    /// Why the clipboard did not get the image; `None` when it did.
    pub copy_error: Option<String>,
    /// Saving is possible: there is a folder to save to.
    pub can_save: bool,
    /// The outcome of saving: the path, or the error. `None` before any save.
    pub saved: Option<Result<PathBuf, String>>,
    /// A save is running.
    pub saving: bool,
}

/// What a background thread reports to the view.
#[derive(Clone, Debug)]
pub enum Event {
    /// A capture is done.
    Captured(Shot),
    /// A capture failed.
    Failed(String),
    /// A save is done.
    Saved(Result<PathBuf, String>),
}

/// Renders the region, copies it to the clipboard and saves it when asked to. Blocks: run it on a
/// background thread (R-1).
pub fn run_job(doc: &DocumentHandle, clipboard: &dyn Clipboard, params: &JobParams) -> Event {
    // Captures go into other documents: always the page's own colours (VIEW-7).
    let colors = refdesk_core::PageColors::Original;
    let image = match doc.render_region_wait(params.page, params.region, params.scale_milli, colors)
    {
        Ok(image) => image,
        Err(e) => return Event::Failed(e.to_string()),
    };
    let png = match encode_png(&image) {
        Ok(png) => png,
        Err(e) => return Event::Failed(e),
    };
    let copy_error = copy_with_fallback(clipboard, &png, &image).err();
    let (width, height) = (image.width, image.height);
    drop(image);
    let stem = capture_file_stem(&params.doc_name, params.page, params.stamp);
    let saved = match (&params.save_dir, params.always_save) {
        (Some(dir), true) => Some(save_png(dir, &stem, &png).map_err(|e| e.to_string())),
        _ => None,
    };
    Event::Captured(Shot {
        width,
        height,
        png: Png(png.into()),
        stem,
        copy_error,
        can_save: params.save_dir.is_some(),
        saved,
        saving: false,
    })
}

// ------------------------------------------------------------------------------------------------
// The state of a view
// ------------------------------------------------------------------------------------------------

/// What the capture mode is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Not capturing.
    Off,
    /// Waiting for a drag.
    Armed,
    /// A drag is going on.
    Dragging {
        /// Where the button went down, in page space.
        start: Location,
        /// The pointer relative to the top-left corner of the view.
        pointer: Point,
    },
}

/// How a button release ended the drag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Released {
    /// There was no drag.
    Ignored,
    /// The rectangle was too small: nothing is captured.
    Cancelled,
    /// Capture this.
    Region(CaptureRegion),
}

/// The notice at the bottom of the view.
#[derive(Clone, Debug)]
pub enum Notice {
    /// A capture finished.
    Captured(Shot),
    /// A capture failed.
    Failed(String),
}

/// Shared between the capture threads and the view.
#[derive(Clone, Debug, Default)]
pub struct Inbox(Arc<Mutex<Vec<Event>>>);

impl Inbox {
    /// Leaves an event for the view.
    pub fn post(&self, event: Event) {
        if let Ok(mut events) = self.0.lock() {
            events.push(event);
        }
    }

    fn take(&self) -> Vec<Event> {
        self.0
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }
}

/// The capture state of one PDF view. Not saved; a copy of a view starts without it.
#[derive(Debug)]
pub struct CaptureState {
    mode: Mode,
    /// `capture.scale` as of the last time the mode was started.
    factor: f32,
    /// Jobs started and not yet reported.
    pending: u32,
    /// The notice and the time (seconds) it goes away.
    notice: Option<(Notice, f64)>,
    inbox: Inbox,
}

impl Default for CaptureState {
    fn default() -> CaptureState {
        CaptureState {
            mode: Mode::Off,
            factor: DEFAULT_FACTOR,
            pending: 0,
            notice: None,
            inbox: Inbox::default(),
        }
    }
}

impl Clone for CaptureState {
    /// A duplicated view is not capturing and shows no notice; it must not share the inbox.
    fn clone(&self) -> CaptureState {
        CaptureState::default()
    }
}

impl CaptureState {
    /// The mode.
    #[cfg(test)]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the capture mode is on (armed or dragging).
    pub fn is_active(&self) -> bool {
        self.mode != Mode::Off
    }

    /// The notice, if one is up.
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref().map(|(notice, _)| notice)
    }

    /// Captures that were started and have not reported yet.
    #[cfg(test)]
    pub fn pending(&self) -> u32 {
        self.pending
    }

    /// The inbox a capture thread reports to; clones share it.
    pub fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    /// The capture command: turns the mode on, or off when it is on. `factor` is the
    /// `capture.scale` setting.
    pub fn toggle(&mut self, factor: f32) {
        if self.is_active() {
            self.mode = Mode::Off;
        } else {
            self.factor = sanitize_factor(factor);
            self.mode = Mode::Armed;
        }
    }

    /// Leaves the capture mode, abandoning a drag.
    pub fn cancel(&mut self) {
        self.mode = Mode::Off;
    }

    /// The button went down at `start` (page space); `pointer` is relative to the view. Only an
    /// armed mode starts a drag.
    pub fn press(&mut self, start: Location, pointer: Point) {
        if self.mode == Mode::Armed {
            self.mode = Mode::Dragging { start, pointer };
        }
    }

    /// The pointer moved.
    pub fn drag(&mut self, pointer: Point) {
        if let Mode::Dragging { start, .. } = self.mode {
            self.mode = Mode::Dragging { start, pointer };
        }
    }

    /// The button went up at `pointer`. A rectangle that is too small cancels; any other is
    /// returned to be captured. The mode is off afterwards either way.
    pub fn release(&mut self, layout: &PageLayout, scroll: Point, pointer: Point) -> Released {
        let Mode::Dragging { start, .. } = self.mode else {
            return Released::Ignored;
        };
        self.mode = Mode::Off;
        match region_from_drag(layout, scroll, start, pointer) {
            Some(region) if !region.is_too_small() => Released::Region(region),
            _ => Released::Cancelled,
        }
    }

    /// The region being dragged now, for drawing.
    pub fn dragged_region(&self, layout: &PageLayout, scroll: Point) -> Option<CaptureRegion> {
        match self.mode {
            Mode::Dragging { start, pointer } => region_from_drag(layout, scroll, start, pointer),
            _ => None,
        }
    }

    /// The render scale for a region of this view (see [`capture_scale_milli`]).
    pub fn scale_milli(
        &self,
        layout: &PageLayout,
        screen_scale: f32,
        region: &CaptureRegion,
    ) -> u32 {
        let scale = layout.scale();
        let page = layout.page_rect(region.page);
        let size = Size::new(page.width() / scale, page.height() / scale);
        capture_scale_milli(screen_scale, self.factor, region.rect, size)
    }

    /// A capture was started.
    pub fn begin_job(&mut self) {
        self.pending += 1;
    }

    /// Takes the notice's PNG for saving and marks the save as running. `None` when there is
    /// nothing to save, saving is not possible, the file is already saved or a save is running.
    pub fn begin_save(&mut self) -> Option<(Png, String)> {
        let Some((Notice::Captured(shot), _)) = &mut self.notice else {
            return None;
        };
        if !shot.can_save || shot.saving || matches!(shot.saved, Some(Ok(_))) {
            return None;
        }
        shot.saving = true;
        Some((shot.png.clone(), shot.stem.clone()))
    }

    /// Takes in what the threads reported and drops a notice that has run out. Returns how long
    /// the notice that is still up has to go.
    pub fn poll(&mut self, now: f64) -> Option<Duration> {
        for event in self.inbox.take() {
            self.apply(event, now);
        }
        let (_, until) = self.notice.as_ref()?;
        let left = until - now;
        if left <= 0.0 {
            self.notice = None;
            return None;
        }
        Some(Duration::from_secs_f64(left))
    }

    fn apply(&mut self, event: Event, now: f64) {
        let until = now + NOTICE_SECS;
        match event {
            Event::Captured(shot) => {
                self.pending = self.pending.saturating_sub(1);
                self.notice = Some((Notice::Captured(shot), until));
            }
            Event::Failed(message) => {
                self.pending = self.pending.saturating_sub(1);
                self.notice = Some((Notice::Failed(message), until));
            }
            Event::Saved(result) => {
                if let Some((Notice::Captured(shot), time)) = &mut self.notice {
                    shot.saving = false;
                    shot.saved = Some(result);
                    // Long enough to read where the file went.
                    *time = until;
                }
            }
        }
    }
}

/// The text of a notice.
pub fn notice_text(notice: &Notice) -> String {
    match notice {
        Notice::Failed(message) => tr("shot.failed").replace("{}", message),
        Notice::Captured(shot) => {
            let size = |text: &str| {
                text.replace("{w}", &shot.width.to_string())
                    .replace("{h}", &shot.height.to_string())
            };
            let mut text = match &shot.copy_error {
                None => size(tr("shot.copied")),
                Some(error) => size(tr("shot.copy_failed")).replace("{err}", error),
            };
            match &shot.saved {
                Some(Ok(path)) => {
                    text.push_str(" · ");
                    text.push_str(&tr("shot.saved").replace("{}", &path.display().to_string()));
                }
                Some(Err(error)) => {
                    text.push_str(" · ");
                    text.push_str(&tr("shot.save_failed").replace("{}", error));
                }
                None => {}
            }
            text
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Commands (called by `dispatch`)
// ------------------------------------------------------------------------------------------------

/// "Capture area": turns the capture mode of a PDF tab on, or off when it is on. Whatever was
/// selected is deselected, since the drag no longer selects text.
pub fn start(state: &mut AppState, tab: TabId) {
    let factor = state.config.capture.scale;
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        pdf.ui.selection.clear();
        pdf.ui.capture.toggle(factor);
    }
}

/// "Capture page": captures the whole page the view shows (SHOT-4).
pub fn capture_page(state: &mut AppState, tab: TabId, ctx: &egui::Context) {
    let factor = state.config.capture.scale;
    let Some(Tab::Pdf(pdf)) = state.tabs.get(tab) else {
        return;
    };
    let Some(info) = state.docs.get(pdf.doc).and_then(|d| d.info()) else {
        return;
    };
    let page = pdf
        .ui
        .current_page
        .unwrap_or(pdf.view.anchor.page)
        .min((info.page_sizes.len() as u32).saturating_sub(1));
    let Some(region) = whole_page(&info.page_sizes, page) else {
        return;
    };
    let layout_scale = pdf
        .ui
        .layout()
        .map_or(refdesk_core::layout::PIXELS_PER_POINT_AT_100, |l| l.scale());
    let screen_scale = layout_scale * ctx.pixels_per_point();
    let size = info.page_sizes[page as usize];
    let scale_milli = capture_scale_milli(screen_scale, factor, region, size);
    launch(state, tab, page, region, scale_milli, ctx);
}

/// Captures a region the user dragged (SHOT-1). The rectangle is in page space.
pub fn capture_region(
    state: &mut AppState,
    tab: TabId,
    page: u32,
    rect: Rect,
    scale_milli: u32,
    ctx: &egui::Context,
) {
    launch(state, tab, page, rect, scale_milli, ctx);
}

/// Starts the background thread of a capture.
fn launch(
    state: &mut AppState,
    tab: TabId,
    page: u32,
    region: Rect,
    scale_milli: u32,
    ctx: &egui::Context,
) {
    let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) else {
        return;
    };
    let Some(doc) = state.docs.get(pdf.doc) else {
        return;
    };
    let params = JobParams {
        page,
        region,
        scale_milli,
        doc_name: document_name(doc.path()),
        save_dir: state
            .paths
            .as_ref()
            .map(|paths| state.config.capture_dir(paths)),
        always_save: state.config.capture.always_save,
        stamp: Stamp::now(),
    };
    let clipboard: Arc<dyn Clipboard> = match &state.capture_clipboard {
        Some(clipboard) => Arc::clone(clipboard),
        None => Arc::new(SystemClipboard::new(ctx.clone())),
    };
    pdf.ui.capture.begin_job();
    let inbox = pdf.ui.capture.inbox();
    let doc = doc.clone();
    let thread_ctx = ctx.clone();
    let thread_inbox = inbox.clone();
    let spawned = std::thread::Builder::new()
        .name("capture".to_string())
        .spawn(move || {
            let event = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_job(&doc, clipboard.as_ref(), &params)
            }))
            .unwrap_or_else(|_| Event::Failed("internal error".to_string()));
            thread_inbox.post(event);
            thread_ctx.request_repaint();
        });
    if let Err(e) = spawned {
        inbox.post(Event::Failed(e.to_string()));
        ctx.request_repaint();
    }
}

/// The "Save" button of the notice: writes the PNG of the last capture in the background.
pub fn save(state: &mut AppState, tab: TabId, ctx: &egui::Context) {
    let Some(paths) = &state.paths else {
        return;
    };
    let dir = state.config.capture_dir(paths);
    let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) else {
        return;
    };
    let Some((png, stem)) = pdf.ui.capture.begin_save() else {
        return;
    };
    let inbox = pdf.ui.capture.inbox();
    let ctx = ctx.clone();
    let thread_inbox = inbox.clone();
    let thread_ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("capture-save".to_string())
        .spawn(move || {
            let result = save_png(&dir, &stem, &png.0).map_err(|e| e.to_string());
            thread_inbox.post(Event::Saved(result));
            thread_ctx.request_repaint();
        });
    if let Err(e) = spawned {
        inbox.post(Event::Saved(Err(e.to_string())));
        ctx.request_repaint();
    }
}

// ------------------------------------------------------------------------------------------------
// The view
// ------------------------------------------------------------------------------------------------

fn viewport_pos(view_rect: egui::Rect, pos: egui::Pos2) -> Point {
    let v = pos - view_rect.min;
    Point::new(v.x, v.y)
}

impl CaptureState {
    /// Reads this frame's input for the capture mode. Returns true when the mode was on, in which
    /// case the pointer events are taken away from the rest of the view: no text is selected and
    /// no link is followed.
    ///
    /// A press on the page starts the rectangle, the release captures it (an action is queued),
    /// Esc or a right click cancels, and so does the view losing the focus.
    #[allow(clippy::too_many_arguments)]
    pub fn input(
        &mut self,
        ui: &Ui,
        response: &egui::Response,
        view_rect: egui::Rect,
        focused: bool,
        layout: &PageLayout,
        scroll: Point,
        tab: TabId,
        queue: &mut Vec<Action>,
    ) -> bool {
        if !self.is_active() {
            return false;
        }
        if !focused {
            self.cancel();
            return false;
        }
        let escape = !ui.ctx().text_edit_focused()
            && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        if escape {
            self.cancel();
        }
        let screen_scale = layout.scale() * ui.ctx().pixels_per_point();
        let events = ui.input(|i| i.events.clone());
        for event in events {
            if !self.is_active() {
                break;
            }
            match event {
                egui::Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    ..
                } if response.hovered() && view_rect.contains(pos) => {
                    let vp = viewport_pos(view_rect, pos);
                    if let Some(hit) = selection::locate(layout, scroll, vp) {
                        self.press(hit.loc, vp);
                    }
                }
                egui::Event::PointerMoved(pos) => self.drag(viewport_pos(view_rect, pos)),
                egui::Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    ..
                } => {
                    let vp = viewport_pos(view_rect, pos);
                    if let Released::Region(region) = self.release(layout, scroll, vp) {
                        let scale_milli = self.scale_milli(layout, screen_scale, &region);
                        queue.push(Action::CaptureRegion {
                            tab,
                            page: region.page,
                            rect: region.rect,
                            scale_milli,
                        });
                    }
                }
                egui::Event::PointerButton {
                    pos,
                    button: PointerButton::Secondary,
                    pressed: true,
                    ..
                } if view_rect.contains(pos) => self.cancel(),
                _ => {}
            }
        }
        if matches!(self.mode, Mode::Dragging { .. }) {
            // The pointer may be still while the view scrolls under it.
            if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                self.drag(viewport_pos(view_rect, pos));
            }
            // The release happened where this view saw no event (another window had the pointer).
            if !ui.input(|i| i.pointer.primary_down()) {
                self.cancel();
            }
        }
        ui.input_mut(|i| {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::PointerButton { .. } | egui::Event::PointerMoved(_)
                )
            });
        });
        true
    }

    /// Draws the capture mode: the hint above the view, the dimmed surroundings of the rectangle,
    /// the rectangle and the size of the image it makes. Sets the crosshair cursor.
    pub fn paint(
        &self,
        ui: &Ui,
        view_rect: egui::Rect,
        hovered: bool,
        layout: &PageLayout,
        scroll: Point,
    ) {
        if !self.is_active() {
            return;
        }
        if hovered {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        }
        let painter = ui.painter_at(view_rect);
        if let Some(region) = self.dragged_region(layout, scroll) {
            let r = egui::Rect::from_min_max(
                view_rect.min + egui::vec2(region.screen.x0, region.screen.y0),
                view_rect.min + egui::vec2(region.screen.x1, region.screen.y1),
            );
            let dim = Color32::from_black_alpha(110);
            let all = view_rect;
            // Four bands around the rectangle.
            for band in [
                egui::Rect::from_min_max(all.min, egui::pos2(all.max.x, r.min.y)),
                egui::Rect::from_min_max(egui::pos2(all.min.x, r.max.y), all.max),
                egui::Rect::from_min_max(
                    egui::pos2(all.min.x, r.min.y),
                    egui::pos2(r.min.x, r.max.y),
                ),
                egui::Rect::from_min_max(
                    egui::pos2(r.max.x, r.min.y),
                    egui::pos2(all.max.x, r.max.y),
                ),
            ] {
                if band.is_positive() {
                    painter.rect_filled(band, 0.0, dim);
                }
            }
            painter.rect_stroke(
                r,
                0.0,
                Stroke::new(1.5, Color32::from_rgb(90, 170, 255)),
                StrokeKind::Outside,
            );
            let screen_scale = layout.scale() * ui.ctx().pixels_per_point();
            let (w, h) = pixel_size(region.rect, self.scale_milli(layout, screen_scale, &region));
            let label = tr("shot.size")
                .replace("{w}", &w.to_string())
                .replace("{h}", &h.to_string());
            paint_label(
                &painter,
                view_rect,
                r.right_bottom() + egui::vec2(6.0, 6.0),
                &label,
            );
        }
        let hint = painter.layout_no_wrap(
            tr("shot.hint").to_string(),
            egui::FontId::proportional(14.0),
            Color32::WHITE,
        );
        let pad = egui::vec2(12.0, 6.0);
        let size = hint.size() + pad * 2.0;
        let rect = egui::Rect::from_min_size(
            egui::pos2(view_rect.center().x - size.x / 2.0, view_rect.min.y + 12.0),
            size,
        );
        painter.rect_filled(rect, 6.0, Color32::from_black_alpha(210));
        painter.galley(rect.min + pad, hint, Color32::WHITE);
    }

    /// Takes in what the capture threads reported and shows the notice at the bottom of the view:
    /// "captured" with a Save button, an error, or "capturing" while a job runs. A click on Save
    /// queues [`Action::SaveCapture`].
    pub fn show_notice(
        &mut self,
        ui: &Ui,
        view_rect: egui::Rect,
        now: f64,
        tab: TabId,
        queue: &mut Vec<Action>,
    ) {
        if let Some(left) = self.poll(now) {
            ui.ctx().request_repaint_after(left);
        }
        let working = self.notice.is_none() && self.pending > 0;
        let (text, savable, saving) = match self.notice() {
            Some(notice) => {
                let (savable, saving) = match notice {
                    Notice::Captured(shot) => (
                        shot.can_save && !matches!(shot.saved, Some(Ok(_))),
                        shot.saving,
                    ),
                    Notice::Failed(_) => (false, false),
                };
                (notice_text(notice), savable, saving)
            }
            None if working => (tr("shot.working").to_string(), false, false),
            None => return,
        };
        let id = ui.scope_id().with(("capture_notice", tab));
        let max_width = (view_rect.width() - 32.0).max(120.0);
        let mut save_clicked = false;
        egui::Area::new(id)
            .order(egui::Order::Foreground)
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(egui::pos2(view_rect.center().x, view_rect.max.y - 16.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(max_width);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(text);
                        if saving {
                            ui.label(tr("shot.saving"));
                        } else if savable {
                            save_clicked = ui.button(tr("shot.save")).clicked();
                        }
                    });
                });
            });
        if save_clicked {
            queue.push(Action::SaveCapture(tab));
        }
    }
}

/// Draws a small label with a dark background at `at`, moved so that it stays inside `view_rect`.
fn paint_label(painter: &egui::Painter, view_rect: egui::Rect, at: egui::Pos2, text: &str) {
    let galley = painter.layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(13.0),
        Color32::WHITE,
    );
    let pad = egui::vec2(6.0, 3.0);
    let size = galley.size() + pad * 2.0;
    let min = egui::pos2(
        at.x.min(view_rect.max.x - size.x).max(view_rect.min.x),
        at.y.min(view_rect.max.y - size.y).max(view_rect.min.y),
    );
    let rect = egui::Rect::from_min_size(min, size);
    painter.rect_filled(rect, 4.0, Color32::from_black_alpha(210));
    painter.galley(rect.min + pad, galley, Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use refdesk_core::{LayoutParams, ZoomMode};

    use crate::views::pdf_view::{MARGIN, PAGE_GAP};

    fn layout(zoom: f32, pages: &[Size]) -> PageLayout {
        PageLayout::new(
            pages,
            LayoutParams {
                viewport: Size::new(700.0, 500.0),
                zoom: ZoomMode::Custom(zoom),
                gap: PAGE_GAP,
                margin: MARGIN,
                fit_page: 0,
            },
        )
    }

    fn letter() -> Size {
        Size::new(612.0, 792.0)
    }

    /// Where a page-space point is relative to the top-left corner of the view.
    fn viewport_of(layout: &PageLayout, scroll: Point, page: u32, x: f32, y: f32) -> Point {
        let r = layout.page_rect(page);
        let s = layout.scale();
        Point::new(r.x0 + x * s - scroll.x, r.y0 + y * s - scroll.y)
    }

    fn near(a: Rect, b: Rect) -> bool {
        let close = |x: f32, y: f32| (x - y).abs() < 0.01;
        close(a.x0, b.x0) && close(a.y0, b.y0) && close(a.x1, b.x1) && close(a.y1, b.y1)
    }

    fn region(layout: &PageLayout, scroll: Point, start: Location, to: Point) -> CaptureRegion {
        region_from_drag(layout, scroll, start, to).expect("region")
    }

    // ---- rectangles ----

    #[test]
    fn a_drag_becomes_a_page_space_rectangle() {
        let layout = layout(1.0, &[letter(), letter()]);
        let scroll = Point::new(0.0, 0.0);
        let start = Location::new(0, 100.0, 120.0);
        let to = viewport_of(&layout, scroll, 0, 300.0, 200.0);
        let r = region(&layout, scroll, start, to);
        assert_eq!(r.page, 0);
        assert!(near(r.rect, Rect::new(100.0, 120.0, 300.0, 200.0)), "{r:?}");
        // The screen rectangle is where the drag was, in logical pixels.
        let from = viewport_of(&layout, scroll, 0, 100.0, 120.0);
        assert!((r.screen.x0 - from.x).abs() < 0.01 && (r.screen.y0 - from.y).abs() < 0.01);
        assert!((r.screen.x1 - to.x).abs() < 0.01 && (r.screen.y1 - to.y).abs() < 0.01);
    }

    #[test]
    fn zoom_and_scroll_change_the_screen_but_not_the_page_rectangle() {
        for zoom in [0.5, 1.0, 2.5] {
            let layout = layout(zoom, &[letter(), letter(), letter()]);
            let scroll = Point::new(0.0, layout.page_rect(1).y0 + 30.0);
            let start = Location::new(1, 50.0, 60.0);
            let to = viewport_of(&layout, scroll, 1, 180.0, 140.0);
            let r = region(&layout, scroll, start, to);
            assert_eq!(r.page, 1);
            assert!(
                near(r.rect, Rect::new(50.0, 60.0, 180.0, 140.0)),
                "zoom {zoom}: {r:?}"
            );
        }
    }

    #[test]
    fn scrolling_during_the_drag_keeps_the_start_on_the_page() {
        let layout = layout(1.0, &[letter(), letter()]);
        let start = Location::new(0, 100.0, 300.0);
        // The pointer stands still while the view scrolls by 60 logical pixels.
        let pointer = Point::new(400.0, 200.0);
        let before = region(&layout, Point::new(0.0, 0.0), start, pointer);
        let after = region(&layout, Point::new(0.0, 60.0), start, pointer);
        // The start stays where it was pressed (y 300); the pointer now points 60 px lower.
        assert!((before.rect.y1 - 300.0).abs() < 0.01 && (after.rect.y1 - 300.0).abs() < 0.01);
        let s = layout.scale();
        assert!(
            (after.rect.y0 - before.rect.y0 - 60.0 / s).abs() < 0.01,
            "{before:?} {after:?}"
        );
    }

    #[test]
    fn dragging_backwards_gives_the_same_rectangle() {
        let layout = layout(1.0, &[letter()]);
        let scroll = Point::new(0.0, 0.0);
        let forward = region(
            &layout,
            scroll,
            Location::new(0, 100.0, 120.0),
            viewport_of(&layout, scroll, 0, 300.0, 200.0),
        );
        let backward = region(
            &layout,
            scroll,
            Location::new(0, 300.0, 200.0),
            viewport_of(&layout, scroll, 0, 100.0, 120.0),
        );
        assert!(near(forward.rect, backward.rect));
        assert!(forward.rect.x0 <= forward.rect.x1 && forward.rect.y0 <= forward.rect.y1);
    }

    #[test]
    fn a_rectangle_is_cut_to_the_page() {
        let layout = layout(1.0, &[letter()]);
        let scroll = Point::new(0.0, 0.0);
        // Dragging out past the right and bottom edges.
        let to = viewport_of(&layout, scroll, 0, 900.0, 1000.0);
        let r = region(&layout, scroll, Location::new(0, 500.0, 700.0), to);
        assert!(near(r.rect, Rect::new(500.0, 700.0, 612.0, 792.0)), "{r:?}");
        // Dragging out past the top-left corner.
        let to = viewport_of(&layout, scroll, 0, -50.0, -20.0);
        let r = region(&layout, scroll, Location::new(0, 40.0, 30.0), to);
        assert!(near(r.rect, Rect::new(0.0, 0.0, 40.0, 30.0)), "{r:?}");
        // The screen rectangle follows the cut.
        let corner = viewport_of(&layout, scroll, 0, 0.0, 0.0);
        assert!((r.screen.x0 - corner.x).abs() < 0.01 && (r.screen.y0 - corner.y).abs() < 0.01);
    }

    #[test]
    fn a_drag_over_two_pages_keeps_the_part_on_the_page_it_started_on() {
        let layout = layout(1.0, &[letter(), letter()]);
        let scroll = Point::new(0.0, layout.page_rect(0).y0 + 500.0);
        let start = Location::new(0, 100.0, 700.0);
        // The pointer ends on the second page.
        let to = viewport_of(&layout, scroll, 1, 400.0, 100.0);
        let r = region(&layout, scroll, start, to);
        assert_eq!(r.page, 0);
        assert!(near(r.rect, Rect::new(100.0, 700.0, 400.0, 792.0)), "{r:?}");
        // And the other way round: start on page 1, end on page 0.
        let start = Location::new(1, 100.0, 50.0);
        let to = viewport_of(&layout, scroll, 0, 400.0, 700.0);
        let r = region(&layout, scroll, start, to);
        assert_eq!(r.page, 1);
        assert!(near(r.rect, Rect::new(100.0, 0.0, 400.0, 50.0)), "{r:?}");
    }

    #[test]
    fn a_page_the_layout_does_not_have_gives_no_region() {
        let layout = layout(1.0, &[letter()]);
        assert!(
            region_from_drag(
                &layout,
                Point::new(0.0, 0.0),
                Location::new(3, 0.0, 0.0),
                Point::new(10.0, 10.0)
            )
            .is_none()
        );
    }

    #[test]
    fn a_side_under_four_logical_pixels_is_too_small() {
        let layout = layout(1.0, &[letter()]);
        let scroll = Point::new(0.0, 0.0);
        let start = Location::new(0, 100.0, 100.0);
        let s = layout.scale();
        let wide = |w: f32, h: f32| {
            region(
                &layout,
                scroll,
                start,
                viewport_of(&layout, scroll, 0, 100.0 + w / s, 100.0 + h / s),
            )
        };
        assert!(wide(3.9, 50.0).is_too_small());
        assert!(wide(50.0, 3.9).is_too_small());
        assert!(wide(0.0, 0.0).is_too_small());
        assert!(!wide(4.1, 4.1).is_too_small());
        // A rectangle cut to nothing by the page edge is too small too.
        let outside = region(
            &layout,
            scroll,
            Location::new(0, 700.0, 100.0),
            viewport_of(&layout, scroll, 0, 800.0, 200.0),
        );
        assert!(outside.is_too_small());
    }

    #[test]
    fn the_whole_page_rectangle_is_the_page_size() {
        let sizes = [letter(), Size::new(300.0, 400.0)];
        assert_eq!(
            whole_page(&sizes, 0),
            Some(Rect::new(0.0, 0.0, 612.0, 792.0))
        );
        assert_eq!(
            whole_page(&sizes, 1),
            Some(Rect::new(0.0, 0.0, 300.0, 400.0))
        );
        assert_eq!(whole_page(&sizes, 2), None);
    }

    // ---- scale ----

    fn full(size: Size) -> Rect {
        Rect::new(0.0, 0.0, size.w, size.h)
    }

    #[test]
    fn the_scale_is_the_screen_scale_times_the_setting() {
        let page = letter();
        let region = Rect::new(10.0, 10.0, 200.0, 100.0);
        // 100% zoom is 96/72; the default factor is 2.
        let milli = capture_scale_milli(96.0 / 72.0, 2.0, region, page);
        assert!((2666..=2668).contains(&milli), "{milli}");
        assert_eq!(capture_scale_milli(1.0, 3.0, region, page), 3000);
        assert_eq!(capture_scale_milli(0.5, 1.0, region, page), 500);
        // Never below what the factor promises: rounding goes up.
        let screen = 0.5 * 96.0 / 72.0;
        assert!(capture_scale_milli(screen, 2.0, region, page) as f32 >= screen * 2.0 * 1000.0);
    }

    #[test]
    fn the_longest_side_never_passes_8000_pixels() {
        let page = Size::new(2000.0, 2000.0);
        for (screen, factor) in [(2.0, 2.0), (5.0, 2.0), (1.3333, 16.0), (0.1, 2.0)] {
            for region in [
                full(page),
                Rect::new(0.0, 0.0, 1999.5, 10.0),
                Rect::new(7.3, 11.7, 1500.2, 1900.9),
            ] {
                let milli = capture_scale_milli(screen, factor, region, page);
                let (w, h) = pixel_size(region, milli);
                assert!(w.max(h) <= MAX_SIDE_PX, "{w}x{h} at {milli} for {region:?}");
                // It is lowered only as far as needed.
                if (screen * factor * 1000.0).ceil() as u32 > milli {
                    let (w2, h2) = pixel_size(region, milli + 5);
                    assert!(w2.max(h2) > MAX_SIDE_PX - 20, "{w2}x{h2}");
                }
            }
        }
        // A letter page at 4x zoom and the default factor is cut to 8000 on the long side.
        let milli = capture_scale_milli(4.0 * 96.0 / 72.0, 2.0, full(letter()), letter());
        let (w, h) = pixel_size(full(letter()), milli);
        assert_eq!(h.max(w), h);
        assert!((7990..=8000).contains(&h), "{h}");
    }

    #[test]
    fn a_tiny_region_at_a_huge_scale_stays_within_what_the_engine_renders() {
        // 8x zoom, factor 2: the whole page at this scale would be 220 megapixels.
        let page = letter();
        let region = Rect::new(100.0, 100.0, 104.0, 103.0);
        let milli = capture_scale_milli(8.0 * 96.0 / 72.0, 2.0, region, page);
        let scale = milli as f32 / 1000.0;
        let pixels = (page.w * scale).ceil() as u64 * (page.h * scale).ceil() as u64;
        assert!(pixels <= MAX_PAGE_PIXELS, "{pixels}");
        let (w, h) = pixel_size(region, milli);
        assert!(w >= 1 && h >= 1);
        // A region of one point still gets at least one pixel at the lowest scale.
        assert!(capture_scale_milli(0.0, 2.0, region, page) >= 1);
    }

    #[test]
    fn unusable_settings_fall_back_to_the_default() {
        assert_eq!(sanitize_factor(2.0), 2.0);
        assert_eq!(sanitize_factor(0.0), DEFAULT_FACTOR);
        assert_eq!(sanitize_factor(-1.0), DEFAULT_FACTOR);
        assert_eq!(sanitize_factor(f32::NAN), DEFAULT_FACTOR);
        assert_eq!(sanitize_factor(f32::INFINITY), DEFAULT_FACTOR);
        assert_eq!(sanitize_factor(1000.0), 16.0);
    }

    #[test]
    fn the_predicted_pixel_size_follows_the_engines_rounding() {
        // 100 pt at scale 1.5 is 150 pixels; a fraction of a pixel still covers the pixel.
        assert_eq!(
            pixel_size(Rect::new(0.0, 0.0, 100.0, 40.0), 1500),
            (150, 60)
        );
        assert_eq!(pixel_size(Rect::new(0.5, 0.0, 0.75, 0.25), 1000), (1, 1));
        // Float noise (96.00001) does not add a pixel.
        assert_eq!(
            pixel_size(Rect::new(0.0, 0.0, 96.00001, 10.0), 1000),
            (96, 10)
        );
    }

    // ---- mode ----

    #[test]
    fn the_mode_goes_from_armed_through_dragging_to_a_capture() {
        let layout = layout(1.0, &[letter()]);
        let scroll = Point::new(0.0, 0.0);
        let mut c = CaptureState::default();
        assert_eq!(c.mode(), Mode::Off);
        // A press before the mode is on does nothing.
        c.press(Location::new(0, 10.0, 10.0), Point::new(0.0, 0.0));
        assert_eq!(c.mode(), Mode::Off);
        c.toggle(2.0);
        assert_eq!(c.mode(), Mode::Armed);
        assert!(c.is_active());
        let start = Location::new(0, 100.0, 100.0);
        let p0 = viewport_of(&layout, scroll, 0, 100.0, 100.0);
        c.press(start, p0);
        assert_eq!(c.mode(), Mode::Dragging { start, pointer: p0 });
        let p1 = viewport_of(&layout, scroll, 0, 250.0, 180.0);
        c.drag(p1);
        assert_eq!(c.mode(), Mode::Dragging { start, pointer: p1 });
        assert!(c.dragged_region(&layout, scroll).is_some());
        match c.release(&layout, scroll, p1) {
            Released::Region(r) => {
                assert_eq!(r.page, 0);
                assert!(near(r.rect, Rect::new(100.0, 100.0, 250.0, 180.0)));
            }
            other => panic!("expected a region, got {other:?}"),
        }
        // Back to the normal mode after the capture.
        assert_eq!(c.mode(), Mode::Off);
        assert_eq!(c.release(&layout, scroll, p1), Released::Ignored);
    }

    #[test]
    fn escape_and_a_small_rectangle_cancel() {
        let layout = layout(1.0, &[letter()]);
        let scroll = Point::new(0.0, 0.0);
        let start = Location::new(0, 100.0, 100.0);
        let p0 = viewport_of(&layout, scroll, 0, 100.0, 100.0);
        let mut c = CaptureState::default();
        c.toggle(2.0);
        c.press(start, p0);
        c.cancel();
        assert_eq!(c.mode(), Mode::Off);
        // The release that follows is nobody's.
        assert_eq!(c.release(&layout, scroll, p0), Released::Ignored);
        // Armed and cancelled before any drag.
        c.toggle(2.0);
        c.cancel();
        assert_eq!(c.mode(), Mode::Off);
        // A click without a drag is too small.
        c.toggle(2.0);
        c.press(start, p0);
        assert_eq!(c.release(&layout, scroll, p0), Released::Cancelled);
        assert_eq!(c.mode(), Mode::Off);
        // The command again while on turns the mode off.
        c.toggle(2.0);
        c.toggle(2.0);
        assert_eq!(c.mode(), Mode::Off);
        // A drag that moves after the mode ended changes nothing.
        c.drag(p0);
        assert_eq!(c.mode(), Mode::Off);
    }

    #[test]
    fn a_copy_of_a_view_does_not_inherit_the_capture() {
        let mut c = CaptureState::default();
        c.toggle(2.0);
        c.begin_job();
        let copy = c.clone();
        assert_eq!(copy.mode(), Mode::Off);
        assert_eq!(copy.pending(), 0);
        assert!(copy.notice().is_none());
    }

    // ---- notice ----

    fn shot(width: u32, height: u32) -> Shot {
        Shot {
            width,
            height,
            png: Png(Arc::from(vec![1u8, 2, 3])),
            stem: "doc-p1-20260102-030405".to_string(),
            copy_error: None,
            can_save: true,
            saved: None,
            saving: false,
        }
    }

    #[test]
    fn the_notice_goes_after_six_seconds_and_a_save_extends_it() {
        let mut c = CaptureState::default();
        c.begin_job();
        assert_eq!(c.pending(), 1);
        c.inbox().post(Event::Captured(shot(1234, 567)));
        let left = c.poll(10.0).expect("notice");
        assert!((left.as_secs_f64() - NOTICE_SECS).abs() < 1e-6);
        assert_eq!(c.pending(), 0);
        let Some(Notice::Captured(s)) = c.notice() else {
            panic!("no notice");
        };
        assert_eq!((s.width, s.height), (1234, 567));
        assert!(c.poll(15.9).is_some());
        // The Save button: one save at a time, with the PNG and the file name.
        let (png, stem) = c.begin_save().expect("save");
        assert_eq!(&*png.0, &[1, 2, 3]);
        assert_eq!(stem, "doc-p1-20260102-030405");
        assert!(c.begin_save().is_none(), "a save is running");
        c.inbox().post(Event::Saved(Ok(PathBuf::from("/x/y.png"))));
        let left = c.poll(15.95).expect("longer");
        assert!(left.as_secs_f64() > 5.9);
        assert!(c.begin_save().is_none(), "already saved");
        assert!(c.poll(22.0).is_none());
        assert!(c.notice().is_none());
    }

    #[test]
    fn a_failed_save_can_be_tried_again() {
        let mut c = CaptureState::default();
        c.begin_job();
        c.inbox().post(Event::Captured(shot(10, 10)));
        c.poll(0.0);
        assert!(c.begin_save().is_some());
        c.inbox().post(Event::Saved(Err("disk full".to_string())));
        c.poll(1.0);
        let text = notice_text(c.notice().expect("notice"));
        assert!(text.contains("disk full"), "{text}");
        assert!(c.begin_save().is_some());
    }

    #[test]
    fn nothing_to_save_without_a_folder_and_a_failure_shows_its_message() {
        let mut c = CaptureState::default();
        c.begin_job();
        let mut s = shot(10, 10);
        s.can_save = false;
        c.inbox().post(Event::Captured(s));
        c.poll(0.0);
        assert!(c.begin_save().is_none());
        c.begin_job();
        c.inbox().post(Event::Failed("no such page".to_string()));
        c.poll(1.0);
        assert!(notice_text(c.notice().expect("notice")).contains("no such page"));
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn the_notice_text_has_the_size_and_the_saved_path() {
        let text = notice_text(&Notice::Captured(shot(1234, 567)));
        assert!(text.contains("1234 × 567"), "{text}");
        let mut s = shot(1234, 567);
        s.saved = Some(Ok(PathBuf::from("/pics/a.png")));
        assert!(notice_text(&Notice::Captured(s)).contains("/pics/a.png"));
        let mut s = shot(1234, 567);
        s.copy_error = Some("no clipboard".to_string());
        let text = notice_text(&Notice::Captured(s));
        assert!(
            text.contains("no clipboard") && text.contains("1234 × 567"),
            "{text}"
        );
    }

    // ---- file names ----

    const STAMP: Stamp = Stamp {
        year: 2026,
        month: 1,
        day: 2,
        hour: 3,
        minute: 4,
        second: 5,
    };

    #[test]
    fn the_file_name_has_the_document_the_page_from_one_and_the_time() {
        assert_eq!(
            capture_file_stem("manual", 0, STAMP),
            "manual-p1-20260102-030405"
        );
        assert_eq!(
            capture_file_stem("manual", 41, STAMP),
            "manual-p42-20260102-030405"
        );
        let late = Stamp {
            year: 2026,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            second: 58,
        };
        assert_eq!(capture_file_stem("a", 2, late), "a-p3-20261231-235958");
    }

    #[test]
    fn characters_a_file_name_cannot_hold_become_underscores() {
        assert_eq!(
            sanitize_file_name("a/b\\c:d*e?f\"g<h>i|j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_file_name("tab\there\nnew"), "tab_here_new");
        assert_eq!(sanitize_file_name("한글 문서 (v2)"), "한글 문서 (v2)");
        assert_eq!(sanitize_file_name(""), "capture");
        assert_eq!(sanitize_file_name(".."), "capture");
        assert_eq!(capture_file_stem("a/b", 0, STAMP), "a_b-p1-20260102-030405");
        let long = "x".repeat(500);
        assert_eq!(sanitize_file_name(&long).chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn the_document_name_is_the_file_name_without_the_extension() {
        assert_eq!(document_name(Path::new("/docs/SWS_EM.pdf")), "SWS_EM");
        assert_eq!(document_name(Path::new("/docs/a.b.pdf")), "a.b");
        assert_eq!(document_name(Path::new("/docs/noext")), "noext");
    }

    #[test]
    fn a_name_that_is_taken_gets_a_number() {
        let dir = Path::new("/pics");
        let taken: Vec<PathBuf> = vec![dir.join("a.png"), dir.join("a-2.png"), dir.join("b.png")];
        let exists = |p: &Path| taken.iter().any(|t| t == p);
        assert_eq!(unique_path(dir, "c", exists), dir.join("c.png"));
        assert_eq!(unique_path(dir, "b", exists), dir.join("b-2.png"));
        assert_eq!(unique_path(dir, "a", exists), dir.join("a-3.png"));
    }

    // ---- saving ----

    /// A temporary folder that goes away with the test.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "refdesk-wp54-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn decode_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().expect("png header");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("png data");
        buf.truncate(info.buffer_size());
        assert_eq!(info.color_type, png::ColorType::Rgb);
        (info.width, info.height, buf)
    }

    fn gradient(w: u32, h: u32) -> RenderedImage {
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgb.extend_from_slice(&[(x * 7) as u8, (y * 13) as u8, (x + y) as u8]);
            }
        }
        RenderedImage {
            width: w,
            height: h,
            rgb,
        }
    }

    #[test]
    fn a_saved_png_reads_back_with_the_same_size_and_pixels() {
        let tmp = TempDir::new("save");
        let image = gradient(37, 21);
        let png = encode_png(&image).expect("png");
        // The folder does not exist yet: it is made.
        let dir = tmp.path().join("deep").join("captures");
        let path = save_png(&dir, "doc-p1-20260102-030405", &png).expect("saved");
        assert_eq!(path, dir.join("doc-p1-20260102-030405.png"));
        let bytes = std::fs::read(&path).expect("file");
        let (w, h, rgb) = decode_png(&bytes);
        assert_eq!((w, h), (37, 21));
        assert_eq!(rgb, image.rgb);
        // The same name again does not overwrite.
        let second = save_png(&dir, "doc-p1-20260102-030405", &png).expect("saved");
        assert_eq!(second, dir.join("doc-p1-20260102-030405-2.png"));
        let third = save_png(&dir, "doc-p1-20260102-030405", &png).expect("saved");
        assert_eq!(third, dir.join("doc-p1-20260102-030405-3.png"));
        assert_eq!(std::fs::read(&path).expect("file"), bytes);
    }

    #[test]
    fn saving_where_a_file_is_in_the_way_reports_an_error() {
        let tmp = TempDir::new("save-error");
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, b"x").expect("file");
        assert!(save_png(&blocker.join("sub"), "a", &[1]).is_err());
    }

    // ---- clipboard ----

    #[derive(Debug)]
    enum Call {
        WlCopy(Vec<u8>),
        Egui(RenderedImage),
    }

    /// A clipboard that only remembers what it was given.
    #[derive(Default)]
    struct FakeClipboard {
        wl_copy: bool,
        wl_copy_fails: bool,
        egui_fails: bool,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeClipboard {
        fn with_wl_copy() -> Arc<FakeClipboard> {
            Arc::new(FakeClipboard {
                wl_copy: true,
                ..FakeClipboard::default()
            })
        }

        fn egui_only() -> Arc<FakeClipboard> {
            Arc::new(FakeClipboard::default())
        }

        fn calls(&self) -> std::sync::MutexGuard<'_, Vec<Call>> {
            self.calls.lock().expect("calls")
        }
    }

    impl Clipboard for FakeClipboard {
        fn wl_copy_available(&self) -> bool {
            self.wl_copy
        }

        fn copy_png_with_wl_copy(&self, png: &[u8]) -> Result<(), String> {
            self.calls().push(Call::WlCopy(png.to_vec()));
            if self.wl_copy_fails {
                Err("wl-copy exited with 1".to_string())
            } else {
                Ok(())
            }
        }

        fn copy_image_with_egui(&self, image: &RenderedImage) -> Result<(), String> {
            self.calls().push(Call::Egui(image.clone()));
            if self.egui_fails {
                Err("no clipboard".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn wl_copy_is_tried_first_when_it_exists() {
        assert_eq!(plan_methods(true), vec![Method::WlCopy, Method::EguiImage]);
        assert_eq!(plan_methods(false), vec![Method::EguiImage]);
        let image = gradient(4, 4);
        let png = encode_png(&image).expect("png");
        let clipboard = FakeClipboard::with_wl_copy();
        assert_eq!(
            copy_with_fallback(clipboard.as_ref(), &png, &image),
            Ok(Method::WlCopy)
        );
        // egui was not needed.
        let calls = clipboard.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0], Call::WlCopy(bytes) if *bytes == png));
    }

    #[test]
    fn without_wl_copy_egui_gets_the_image() {
        let image = gradient(4, 3);
        let png = encode_png(&image).expect("png");
        let clipboard = FakeClipboard::egui_only();
        assert_eq!(
            copy_with_fallback(clipboard.as_ref(), &png, &image),
            Ok(Method::EguiImage)
        );
        let calls = clipboard.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0], Call::Egui(got) if *got == image));
    }

    #[test]
    fn a_failing_wl_copy_falls_back_to_egui_and_both_failing_is_an_error() {
        let image = gradient(4, 4);
        let png = encode_png(&image).expect("png");
        let clipboard = Arc::new(FakeClipboard {
            wl_copy: true,
            wl_copy_fails: true,
            ..FakeClipboard::default()
        });
        assert_eq!(
            copy_with_fallback(clipboard.as_ref(), &png, &image),
            Ok(Method::EguiImage)
        );
        assert_eq!(clipboard.calls().len(), 2);
        let broken = FakeClipboard {
            wl_copy: true,
            wl_copy_fails: true,
            egui_fails: true,
            ..FakeClipboard::default()
        };
        let error = copy_with_fallback(&broken, &png, &image).expect_err("both fail");
        assert!(
            error.contains("wl-copy") && error.contains("no clipboard"),
            "{error}"
        );
    }

    #[test]
    fn wl_copy_is_found_on_the_path_only_when_it_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("path");
        let with = tmp.path().join("with");
        let without = tmp.path().join("without");
        let plain = tmp.path().join("plain");
        for dir in [&with, &without, &plain] {
            std::fs::create_dir_all(dir).expect("dir");
        }
        let program = with.join("wl-copy");
        std::fs::write(&program, b"#!/bin/sh\n").expect("file");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("mode");
        // A file without the execute bit does not count.
        std::fs::write(plain.join("wl-copy"), b"x").expect("file");
        std::fs::set_permissions(
            plain.join("wl-copy"),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("mode");
        let path = |dirs: &[&Path]| std::env::join_paths(dirs).expect("join");
        assert_eq!(
            find_in_path("wl-copy", &path(&[&without, &plain, &with])),
            Some(program)
        );
        assert_eq!(find_in_path("wl-copy", &path(&[&without, &plain])), None);
        assert_eq!(find_in_path("wl-copy", OsStr::new("")), None);
    }

    /// Writes an executable shell script and returns its path.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
        path
    }

    /// `run_wl_copy` on a stand-in program; a fresh script can briefly be "busy" when another
    /// thread forks while it is written.
    fn run_stand_in(program: &Path, input: &[u8], timeout: Duration) -> Result<(), String> {
        let mut result = run_wl_copy(program, input, timeout);
        for _ in 0..20 {
            match &result {
                Err(e) if e.contains("busy") => {
                    std::thread::sleep(Duration::from_millis(50));
                    result = run_wl_copy(program, input, timeout);
                }
                _ => break,
            }
        }
        result
    }

    #[test]
    fn the_wl_copy_runner_pipes_the_png_in_and_reports_exit_timeout_and_absence() {
        // Stand-in programs only: the real wl-copy is never run by tests.
        let tmp = TempDir::new("runner");
        let ok = script(
            tmp.path(),
            "ok",
            "echo \"$@\" > \"$(dirname \"$0\")/args.txt\"\ncat > \"$(dirname \"$0\")/stdin.bin\"",
        );
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(run_stand_in(&ok, &data, Duration::from_secs(20)), Ok(()));
        assert_eq!(
            std::fs::read(tmp.path().join("stdin.bin")).expect("stdin"),
            data
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("args.txt"))
                .expect("args")
                .trim(),
            "--type image/png"
        );
        let fails = script(tmp.path(), "fails", "cat > /dev/null\nexit 3");
        let error = run_stand_in(&fails, b"x", Duration::from_secs(20)).expect_err("fails");
        assert!(error.contains("exited"), "{error}");
        let hangs = script(tmp.path(), "hangs", "sleep 30");
        let started = Instant::now();
        let error = run_stand_in(&hangs, b"x", Duration::from_millis(200)).expect_err("times out");
        assert!(error.contains("too long"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(20));
        let missing = tmp.path().join("missing");
        assert!(run_wl_copy(&missing, b"x", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn the_local_time_is_a_valid_date() {
        let now = Stamp::now();
        assert!(now.year >= 2024);
        assert!((1..=12).contains(&now.month) && (1..=31).contains(&now.day));
        assert!(now.hour < 24 && now.minute < 60 && now.second < 61);
    }

    // ---- commands ----

    #[test]
    fn the_capture_commands_are_in_the_edit_menu_with_a_free_shortcut() {
        use crate::commands::{self, MenuGroup};
        let edit: Vec<&str> = commands::in_menu(MenuGroup::Edit).map(|c| c.id).collect();
        assert!(edit.contains(&"edit.capture_region"));
        assert!(edit.contains(&"edit.capture_page"));
        let region = commands::command("edit.capture_region").expect("command");
        assert_eq!(
            region.shortcut,
            Some(egui::KeyboardShortcut::new(
                Modifiers::CTRL.plus(Modifiers::SHIFT),
                Key::C
            ))
        );
        // The table has no duplicate identifier or shortcut (also checked by the table's own
        // tests); only the capture commands matter here.
        let same = commands::COMMANDS
            .iter()
            .filter(|c| c.shortcut.is_some() && c.shortcut == region.shortcut)
            .count();
        assert_eq!(same, 1);
        assert_eq!(
            commands::COMMANDS
                .iter()
                .filter(|c| c.id.starts_with("edit.capture"))
                .count(),
            2
        );
    }

    #[test]
    fn the_capture_commands_are_off_unless_a_pdf_tab_has_the_focus() {
        use crate::commands;
        // A fresh state shows the start page.
        let state = AppState::new();
        for id in ["edit.capture_region", "edit.capture_page"] {
            let command = commands::command(id).expect("command");
            assert!(!(command.enabled)(&state), "{id}");
            assert!(command.run(&state).is_none(), "{id}");
        }
    }

    // ---- end to end: frames without a window, a fake clipboard, a temporary folder ----

    use crate::action::{OpenTarget, dispatch, dispatch_queued};
    use crate::state::PdfTab;
    use crate::views::pdf_view::{ViewContext, show};
    use crate::views::preload;
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};
    use crate::views::textures::collect_rendered;
    use refdesk_core::testutil::{PdfBuilder, TempPdf};

    const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);
    const PAGE: Size = Size { w: 595.0, h: 842.0 };

    struct Shell {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        time: f64,
        clipboard: Arc<FakeClipboard>,
    }

    impl Shell {
        fn new(clipboard: Arc<FakeClipboard>) -> Shell {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            let mut state = AppState::new();
            state.capture_clipboard = Some(clipboard.clone());
            Shell {
                ctx,
                repaints,
                state,
                time: 0.0,
                clipboard,
            }
        }

        fn open(&mut self, pdf: &TempPdf) -> TabId {
            let target = OpenTarget {
                path: pdf.path().to_path_buf(),
                page: None,
            };
            dispatch(&mut self.state, Action::OpenFiles(vec![target]), &self.ctx);
            let tab = self.state.focus.expect("the new tab has the focus");
            self.frames_until("the layout", |s| s.tab(tab).ui.layout().is_some());
            tab
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            self.time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            collect_rendered(&self.ctx, &self.state.docs, &mut self.state.textures);
            preload::step(&self.ctx, &mut self.state);
            let state = &mut self.state;
            let mut queue = Vec::new();
            let output = self.ctx.run_ui(input, |ui| {
                queue.extend(crate::commands::handle_shortcuts(ui.ctx(), state));
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| {
                        let Some(active) = state.main_window().dock.focused_tab() else {
                            return;
                        };
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
                            let cx = ViewContext {
                                tab_id: active,
                                focused: *focus == Some(active),
                                textures,
                                queue: &mut queue,
                            };
                            show(ui, tab, doc, cx);
                        }
                    });
            });
            output.drop_without_applying_deltas();
            self.state.actions.append(&mut queue);
            dispatch_queued(&mut self.state, &self.ctx);
        }

        fn frames_until(&mut self, what: &str, mut check: impl FnMut(&Shell) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.frame(Vec::new());
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        fn tab(&self, tab: TabId) -> &PdfTab {
            self.state.tabs.pdf(tab).expect("pdf tab")
        }

        /// The screen position of a page-space point of the view of `tab` as drawn last. The view
        /// starts at the top-left corner of the screen.
        fn screen_of(&self, tab: TabId, page: u32, x: f32, y: f32) -> egui::Pos2 {
            let ui = &self.tab(tab).ui;
            let p = viewport_of(ui.layout().expect("layout"), ui.scroll(), page, x, y);
            egui::pos2(p.x, p.y)
        }

        fn layout_scale(&self, tab: TabId) -> f32 {
            self.tab(tab).ui.layout().expect("layout").scale()
        }

        fn zoom(&mut self, tab: TabId, zoom: f32) {
            dispatch(
                &mut self.state,
                Action::SetZoom {
                    tab,
                    zoom: ZoomMode::Custom(zoom),
                },
                &self.ctx,
            );
            self.frames_until("the zoom", |s| {
                s.tab(tab)
                    .ui
                    .effective_zoom
                    .is_some_and(|z| (z - zoom).abs() < 1e-4)
            });
        }

        fn arm(&mut self, tab: TabId) {
            dispatch(&mut self.state, Action::StartCapture(tab), &self.ctx);
            self.frame(Vec::new());
            assert!(self.tab(tab).ui.capture.is_active());
        }

        /// Presses at `from`, moves over `to` in two steps and releases there.
        fn drag(&mut self, from: egui::Pos2, to: egui::Pos2) {
            self.frame(vec![
                egui::Event::PointerMoved(from),
                button(from, true, PointerButton::Primary),
            ]);
            self.frame(vec![egui::Event::PointerMoved(from + (to - from) * 0.5)]);
            self.frame(vec![egui::Event::PointerMoved(to)]);
            self.frame(vec![button(to, false, PointerButton::Primary)]);
        }

        /// Arms the capture and drags the page-space rectangle `r` of page `page`.
        fn capture_rect(&mut self, tab: TabId, page: u32, r: Rect) {
            let from = self.screen_of(tab, page, r.x0, r.y0);
            let to = self.screen_of(tab, page, r.x1, r.y1);
            self.arm(tab);
            self.drag(from, to);
        }

        /// Waits until the notice of the capture is up.
        fn wait_notice(&mut self, tab: TabId) -> Notice {
            self.frames_until("the capture notice", |s| {
                s.tab(tab).ui.capture.notice().is_some()
            });
            self.tab(tab).ui.capture.notice().cloned().expect("notice")
        }

        fn shot(&mut self, tab: TabId) -> Shot {
            match self.wait_notice(tab) {
                Notice::Captured(shot) => shot,
                other => panic!("expected a capture, got {other:?}"),
            }
        }

        /// The one image the fake clipboard got through egui.
        fn copied_image(&self) -> RenderedImage {
            let calls = self.clipboard.calls();
            assert_eq!(calls.len(), 1, "{calls:?}");
            match &calls[0] {
                Call::Egui(image) => image.clone(),
                other => panic!("expected an egui copy, got {other:?}"),
            }
        }

        fn selected(&self, tab: TabId) -> bool {
            self.tab(tab).ui.has_selection()
        }

        fn expected_size(&self, tab: TabId, rect: Rect) -> (u32, u32) {
            let milli = capture_scale_milli(self.layout_scale(tab), 2.0, rect, PAGE);
            pixel_size(rect, milli)
        }
    }

    fn button(pos: egui::Pos2, pressed: bool, button: PointerButton) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    fn key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// Two pages; the first has a line of text at (72, 100) and a link to the second page.
    fn document(stem: &str) -> TempPdf {
        let mut b = PdfBuilder::new();
        for n in 1..=3 {
            b = b.page(PAGE.w, PAGE.h, |p| {
                p.text(72.0, 100.0, 24.0, &format!("Capture me {n}"));
                if n == 1 {
                    p.link_to((72.0, 300.0, 260.0, 315.0), 2, Some((72.0, 40.0)));
                    p.text(72.0, 312.0, 12.0, "Go to the next page");
                }
            });
        }
        b.write_temp(stem)
    }

    fn all_white(image: &RenderedImage) -> bool {
        image.rgb.iter().all(|&b| b == 255)
    }

    /// A region with the text "Capture me" in it, and a region of empty paper.
    const TEXT: Rect = Rect {
        x0: 60.0,
        y0: 70.0,
        x1: 320.0,
        y1: 130.0,
    };
    const EMPTY: Rect = Rect {
        x0: 300.0,
        y0: 200.0,
        x1: 500.0,
        y1: 380.0,
    };

    #[test]
    fn capturing_a_region_with_text_puts_an_image_of_that_size_on_the_clipboard() {
        let pdf = document("wp54-text");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        shell.capture_rect(tab, 0, TEXT);
        let shot = shell.shot(tab);
        let image = shell.copied_image();
        let expected = shell.expected_size(tab, TEXT);
        assert_eq!((image.width, image.height), expected);
        assert_eq!((shot.width, shot.height), expected);
        assert_eq!(image.rgb.len(), (image.width * image.height * 3) as usize);
        assert!(!all_white(&image), "the text is in the image");
        assert!(shot.copy_error.is_none());
        // Back to the normal mode, and the job is accounted for.
        let capture = &shell.tab(tab).ui.capture;
        assert!(!capture.is_active());
        assert_eq!(capture.pending(), 0);
        // The PNG for saving is the same picture.
        let (w, h, rgb) = decode_png(&shot.png.0);
        assert_eq!((w, h), expected);
        assert_eq!(rgb, image.rgb);
    }

    #[test]
    fn an_empty_region_is_all_white() {
        let pdf = document("wp54-empty");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        shell.capture_rect(tab, 0, EMPTY);
        shell.shot(tab);
        let image = shell.copied_image();
        assert_eq!((image.width, image.height), shell.expected_size(tab, EMPTY));
        assert!(all_white(&image));
    }

    #[test]
    fn a_capture_at_50_percent_zoom_has_at_least_twice_the_pixels_of_the_screen() {
        let pdf = document("wp54-lowzoom");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        shell.zoom(tab, 0.5);
        let from = shell.screen_of(tab, 0, TEXT.x0, TEXT.y0);
        let to = shell.screen_of(tab, 0, TEXT.x1, TEXT.y1);
        let (screen_w, screen_h) = (to.x - from.x, to.y - from.y);
        shell.arm(tab);
        shell.drag(from, to);
        shell.shot(tab);
        let image = shell.copied_image();
        // Rendered again at 2x the screen scale, not cut out of the screen.
        assert!(
            image.width as f32 + 0.01 >= 2.0 * screen_w
                && image.height as f32 + 0.01 >= 2.0 * screen_h,
            "{}x{} for {screen_w}x{screen_h} on screen",
            image.width,
            image.height
        );
        assert!(!all_white(&image));
    }

    #[test]
    fn with_wl_copy_the_png_goes_there_and_egui_is_not_used() {
        let pdf = document("wp54-wl");
        let clipboard = FakeClipboard::with_wl_copy();
        let mut shell = Shell::new(clipboard.clone());
        let tab = shell.open(&pdf);
        shell.capture_rect(tab, 0, TEXT);
        let shot = shell.shot(tab);
        assert!(shot.copy_error.is_none());
        let calls = clipboard.calls();
        assert_eq!(calls.len(), 1);
        let Call::WlCopy(png) = &calls[0] else {
            panic!("expected wl-copy, got {calls:?}");
        };
        let (w, h, rgb) = decode_png(png);
        assert_eq!((w, h), shell.expected_size(tab, TEXT));
        assert!(rgb.iter().any(|&b| b != 255));
    }

    #[test]
    fn a_clipboard_that_fails_still_gives_a_capture_with_the_reason() {
        let pdf = document("wp54-nocopy");
        let clipboard = Arc::new(FakeClipboard {
            egui_fails: true,
            ..FakeClipboard::default()
        });
        let mut shell = Shell::new(clipboard);
        let tab = shell.open(&pdf);
        shell.capture_rect(tab, 0, TEXT);
        let shot = shell.shot(tab);
        assert_eq!(shot.copy_error.as_deref(), Some("no clipboard"));
        assert!(notice_text(&Notice::Captured(shot)).contains("no clipboard"));
    }

    #[test]
    fn a_drag_over_two_pages_captures_the_start_page_only() {
        let pdf = document("wp54-span");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        shell.zoom(tab, 0.5);
        // Put the end of the first page and the start of the second in the middle of the view.
        let boundary = shell.tab(tab).ui.layout().expect("layout").page_rect(1).y0;
        if let Some(Tab::Pdf(pdf)) = shell.state.tabs.get_mut(tab) {
            pdf.ui.scroll_to(Point::new(0.0, boundary - 300.0));
        }
        shell.frame(Vec::new());
        let from = shell.screen_of(tab, 0, 100.0, 700.0);
        let to = shell.screen_of(tab, 1, 400.0, 100.0);
        shell.arm(tab);
        shell.drag(from, to);
        shell.shot(tab);
        let image = shell.copied_image();
        let rect = Rect::new(100.0, 700.0, 400.0, PAGE.h);
        assert_eq!((image.width, image.height), shell.expected_size(tab, rect));
    }

    #[test]
    fn the_whole_page_command_captures_the_page_the_view_shows() {
        let pdf = document("wp54-page");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        dispatch(&mut shell.state, Action::CapturePage(tab), &shell.ctx);
        let shot = shell.shot(tab);
        let whole = Rect::new(0.0, 0.0, PAGE.w, PAGE.h);
        let expected = shell.expected_size(tab, whole);
        assert_eq!((shot.width, shot.height), expected);
        let image = shell.copied_image();
        assert_eq!((image.width, image.height), expected);
        assert!(!all_white(&image));
        assert!(shot.stem.contains("-p1-"), "{}", shot.stem);
    }

    #[test]
    fn dragging_in_capture_mode_selects_no_text_and_a_click_follows_no_link() {
        let pdf = document("wp54-nosel");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        let doc = shell.tab(tab).doc;
        let handle = shell.state.docs.get(doc).expect("doc").clone();
        let text = handle.page_text_wait(0).expect("text");
        let first = text.chars[0].quad.bounds();
        let last = text.chars[text.chars.len() - 1].quad.bounds();
        let (a, b) = (
            shell.screen_of(tab, 0, first.x0 + 0.3, (first.y0 + first.y1) / 2.0),
            shell.screen_of(tab, 0, last.x1 - 0.3, (last.y0 + last.y1) / 2.0),
        );
        shell.frames_until("the links", |s| {
            s.state
                .docs
                .get(doc)
                .is_some_and(|d| d.page_links(0).is_some())
        });
        // Control: in the normal mode the same drag selects text.
        shell.frame(vec![
            egui::Event::PointerMoved(a),
            button(a, true, PointerButton::Primary),
        ]);
        shell.frame(vec![egui::Event::PointerMoved(b)]);
        shell.frames_until("the selection", |s| s.selected(tab));
        shell.frame(vec![button(b, false, PointerButton::Primary)]);
        assert!(shell.selected(tab));
        // Capture mode: the drag over the text draws a rectangle and selects nothing. Turning
        // the mode on drops the selection.
        shell.arm(tab);
        assert!(!shell.selected(tab));
        shell.frame(vec![
            egui::Event::PointerMoved(a),
            button(a, true, PointerButton::Primary),
        ]);
        shell.frame(vec![egui::Event::PointerMoved(b)]);
        assert!(matches!(
            shell.tab(tab).ui.capture.mode(),
            Mode::Dragging { .. }
        ));
        assert!(!shell.selected(tab));
        shell.frame(vec![button(b, false, PointerButton::Primary)]);
        shell.shot(tab);
        assert!(!shell.selected(tab));
        // A click on the link while capturing cancels the capture and goes nowhere.
        let link = shell.screen_of(tab, 0, 150.0, 307.0);
        shell.arm(tab);
        shell.frame(vec![
            egui::Event::PointerMoved(link),
            button(link, true, PointerButton::Primary),
        ]);
        shell.frame(vec![button(link, false, PointerButton::Primary)]);
        shell.frame(Vec::new());
        assert!(!shell.tab(tab).ui.capture.is_active());
        assert_eq!(shell.tab(tab).view.anchor.page, 0);
        assert!(!shell.tab(tab).view.history.can_back());
        // Control: the same click in the normal mode follows the link.
        shell.frame(vec![
            egui::Event::PointerMoved(link),
            button(link, true, PointerButton::Primary),
        ]);
        shell.frame(vec![button(link, false, PointerButton::Primary)]);
        shell.frames_until("the jump", |s| s.tab(tab).view.history.can_back());
        // Only the first drag captured.
        assert_eq!(shell.clipboard.calls().len(), 1);
    }

    #[test]
    fn escape_a_right_click_and_a_tiny_rectangle_cancel_without_capturing() {
        let pdf = document("wp54-cancel");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        let from = shell.screen_of(tab, 0, 100.0, 300.0);
        let to = shell.screen_of(tab, 0, 300.0, 400.0);
        // Esc in the middle of a drag.
        shell.arm(tab);
        shell.frame(vec![
            egui::Event::PointerMoved(from),
            button(from, true, PointerButton::Primary),
        ]);
        shell.frame(vec![egui::Event::PointerMoved(to)]);
        assert!(matches!(
            shell.tab(tab).ui.capture.mode(),
            Mode::Dragging { .. }
        ));
        shell.frame(vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!shell.tab(tab).ui.capture.is_active());
        shell.frame(vec![button(to, false, PointerButton::Primary)]);
        // Esc while armed.
        shell.arm(tab);
        shell.frame(vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!shell.tab(tab).ui.capture.is_active());
        // A right click while armed, and in the middle of a drag.
        shell.arm(tab);
        shell.frame(vec![
            egui::Event::PointerMoved(from),
            button(from, true, PointerButton::Secondary),
        ]);
        assert!(!shell.tab(tab).ui.capture.is_active());
        shell.arm(tab);
        shell.frame(vec![
            egui::Event::PointerMoved(from),
            button(from, true, PointerButton::Primary),
        ]);
        shell.frame(vec![
            egui::Event::PointerMoved(to),
            button(to, true, PointerButton::Secondary),
        ]);
        assert!(!shell.tab(tab).ui.capture.is_active());
        shell.frame(vec![button(to, false, PointerButton::Primary)]);
        // A rectangle of 3 logical pixels.
        shell.arm(tab);
        shell.drag(from, from + egui::vec2(3.0, 60.0));
        assert!(!shell.tab(tab).ui.capture.is_active());
        shell.frame(Vec::new());
        // Nothing was captured by any of it.
        let capture = &shell.tab(tab).ui.capture;
        assert_eq!(capture.pending(), 0);
        assert!(capture.notice().is_none());
        assert!(shell.clipboard.calls().is_empty());
    }

    #[test]
    fn the_shortcut_turns_the_mode_on_and_off() {
        let pdf = document("wp54-shortcut");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        let chord = Modifiers::CTRL.plus(Modifiers::SHIFT);
        shell.frame(vec![key(Key::C, chord)]);
        assert!(shell.tab(tab).ui.capture.is_active());
        shell.frame(vec![key(Key::C, chord)]);
        assert!(!shell.tab(tab).ui.capture.is_active());
        // Ctrl+C alone is not the capture.
        shell.frame(vec![key(Key::C, Modifiers::CTRL)]);
        assert!(!shell.tab(tab).ui.capture.is_active());
    }

    #[test]
    fn a_capture_that_fails_shows_the_error() {
        let pdf = document("wp54-fail");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        dispatch(
            &mut shell.state,
            Action::CaptureRegion {
                tab,
                page: 99,
                rect: TEXT,
                scale_milli: 1000,
            },
            &shell.ctx,
        );
        assert_eq!(shell.tab(tab).ui.capture.pending(), 1);
        match shell.wait_notice(tab) {
            Notice::Failed(message) => assert!(message.contains("99"), "{message}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert_eq!(shell.tab(tab).ui.capture.pending(), 0);
        assert!(shell.clipboard.calls().is_empty());
    }

    #[test]
    fn the_notice_goes_away_after_six_seconds_of_frames() {
        let pdf = document("wp54-gone");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        shell.capture_rect(tab, 0, TEXT);
        shell.shot(tab);
        shell.time += 5.0;
        shell.frame(Vec::new());
        assert!(shell.tab(tab).ui.capture.notice().is_some());
        shell.time += 1.5;
        shell.frame(Vec::new());
        assert!(shell.tab(tab).ui.capture.notice().is_none());
    }

    #[test]
    fn the_save_button_writes_a_png_that_reads_back_the_same() {
        let tmp = TempDir::new("e2e-save");
        let pdf = document("wp54-save");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        shell.state.paths = Some(refdesk_core::Paths::under(tmp.path()));
        let tab = shell.open(&pdf);
        shell.capture_rect(tab, 0, TEXT);
        let shot = shell.shot(tab);
        assert!(shot.can_save && shot.saved.is_none());
        let captures = tmp.path().join("captures");
        assert!(!captures.exists(), "nothing is saved before the button");
        dispatch(&mut shell.state, Action::SaveCapture(tab), &shell.ctx);
        shell.frames_until("the save", |s| {
            matches!(
                s.tab(tab).ui.capture.notice(),
                Some(Notice::Captured(Shot { saved: Some(_), .. }))
            )
        });
        let Some(Notice::Captured(shot)) = shell.tab(tab).ui.capture.notice().cloned() else {
            panic!("no notice");
        };
        let path = shot.saved.clone().expect("saved").expect("save worked");
        assert_eq!(path.parent(), Some(captures.as_path()));
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert!(
            name.contains("wp54-save-p1-") && name.ends_with(".png"),
            "{name}"
        );
        // The notice shows where the file went.
        assert!(notice_text(&Notice::Captured(shot)).contains(&path.display().to_string()));
        let (w, h, rgb) = decode_png(&std::fs::read(&path).expect("file"));
        let image = shell.copied_image();
        assert_eq!((w, h), (image.width, image.height));
        assert_eq!(rgb, image.rgb);
        // A second press of the button saves nothing more.
        dispatch(&mut shell.state, Action::SaveCapture(tab), &shell.ctx);
        shell.frame(Vec::new());
        assert_eq!(std::fs::read_dir(&captures).expect("dir").count(), 1);
    }

    #[test]
    fn always_save_saves_every_capture_without_the_button() {
        let tmp = TempDir::new("e2e-always");
        let pdf = document("wp54-always");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        shell.state.paths = Some(refdesk_core::Paths::under(tmp.path()));
        shell.state.config.capture.always_save = true;
        let tab = shell.open(&pdf);
        shell.capture_rect(tab, 0, TEXT);
        let shot = shell.shot(tab);
        let path = shot.saved.clone().expect("saved").expect("save worked");
        assert!(path.starts_with(tmp.path().join("captures")));
        let (w, h, rgb) = decode_png(&std::fs::read(&path).expect("file"));
        let image = shell.copied_image();
        assert_eq!((w, h, rgb), (image.width, image.height, image.rgb));
        // A second capture does not overwrite the first, even in the same second.
        shell.time += 10.0;
        shell.capture_rect(tab, 0, TEXT);
        shell.frames_until("the second notice", |s| {
            matches!(
                s.tab(tab).ui.capture.notice(),
                Some(Notice::Captured(Shot { saved: Some(Ok(p)), .. })) if *p != path
            )
        });
        assert_eq!(
            std::fs::read_dir(tmp.path().join("captures"))
                .expect("dir")
                .count(),
            2
        );
    }

    #[test]
    fn the_configured_folder_is_used_instead_of_the_default() {
        let tmp = TempDir::new("e2e-dir");
        let pdf = document("wp54-dir");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        shell.state.paths = Some(refdesk_core::Paths::under(tmp.path()));
        let custom = tmp.path().join("my").join("shots");
        shell.state.config.capture.dir = Some(custom.clone());
        shell.state.config.capture.always_save = true;
        let tab = shell.open(&pdf);
        shell.capture_rect(tab, 0, TEXT);
        let shot = shell.shot(tab);
        let path = shot.saved.expect("saved").expect("save worked");
        assert_eq!(path.parent(), Some(custom.as_path()));
        assert!(!tmp.path().join("captures").exists());
    }

    #[test]
    fn without_a_save_location_saving_is_off() {
        let pdf = document("wp54-nopaths");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        // `paths` is None in a fresh state; always_save has nowhere to write.
        assert!(shell.state.paths.is_none());
        shell.state.config.capture.always_save = true;
        let tab = shell.open(&pdf);
        shell.capture_rect(tab, 0, TEXT);
        let shot = shell.shot(tab);
        assert!(!shot.can_save);
        assert!(shot.saved.is_none());
        dispatch(&mut shell.state, Action::SaveCapture(tab), &shell.ctx);
        shell.frame(Vec::new());
        assert!(matches!(
            shell.tab(tab).ui.capture.notice(),
            Some(Notice::Captured(Shot {
                saved: None,
                saving: false,
                ..
            }))
        ));
        // The image still reached the clipboard.
        assert_eq!(shell.clipboard.calls().len(), 1);
    }

    #[test]
    fn a_duplicated_tab_does_not_share_the_capture_state() {
        let pdf = document("wp54-dup");
        let mut shell = Shell::new(FakeClipboard::egui_only());
        let tab = shell.open(&pdf);
        shell.arm(tab);
        dispatch(&mut shell.state, Action::DuplicateTab(tab), &shell.ctx);
        let copy = shell.state.focus.expect("focus");
        assert_ne!(copy, tab);
        assert!(!shell.tab(copy).ui.capture.is_active());
    }
}
