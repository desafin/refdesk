//! `annotations.json`: bookmarks, one-letter marks and highlights (ETC-2, ETC-3, ETC-5).
//!
//! The PDF files are never touched; everything the reader marks lives here. A document's record
//! is found the way [`super::DocStateDb`] finds one: by path or by content fingerprint, the most
//! recently used first, and the same path is the same document even when its fingerprint
//! changed. When a file was moved or renamed, its fingerprint still finds the record and
//! [`Annotations::follow`] moves the record (and the global marks that point at it) to the new
//! path.
//!
//! All positions are page space [`Location`]s and [`Rect`]s (R-5). The rules (ordering, merging
//! highlights, finding a bookmark at the same spot) live here so that they are tested without a
//! window (R-10).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::atomic::{Loaded, atomic_write, load_with};
use super::{FORMAT_VERSION, json_version, warn_if_newer};
use crate::geom::Rect;
use crate::model::Location;
use crate::text::{CopyOptions, PageText};

/// A bookmark that is this close below or above another one on the same page counts as the
/// same spot (in points).
pub const SAME_SPOT_PT: f32 = 40.0;

/// The colours a highlight can have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HighlightColor {
    /// The first colour, used until another one is chosen.
    #[default]
    Yellow,
    /// Green.
    Green,
    /// Blue.
    Blue,
    /// Pink.
    Pink,
    /// Orange.
    Orange,
}

impl HighlightColor {
    /// Every colour, in menu order.
    pub const ALL: [HighlightColor; 5] = [
        HighlightColor::Yellow,
        HighlightColor::Green,
        HighlightColor::Blue,
        HighlightColor::Pink,
        HighlightColor::Orange,
    ];

    /// The name used in the file (and in accessible names): `yellow`, `green`, ….
    pub fn name(self) -> &'static str {
        match self {
            HighlightColor::Yellow => "yellow",
            HighlightColor::Green => "green",
            HighlightColor::Blue => "blue",
            HighlightColor::Pink => "pink",
            HighlightColor::Orange => "orange",
        }
    }

    /// The colour of a name written by [`HighlightColor::name`]; `None` for anything else.
    pub fn from_name(name: &str) -> Option<HighlightColor> {
        HighlightColor::ALL.into_iter().find(|c| c.name() == name)
    }
}

impl Serialize for HighlightColor {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for HighlightColor {
    /// An unknown colour (from a newer version) reads as yellow rather than losing the highlight.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        Ok(HighlightColor::from_name(&name).unwrap_or_default())
    }
}

/// A named place in a document (ETC-2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    /// Unique within the document's record.
    #[serde(default)]
    pub id: u64,
    /// What the reader called it.
    #[serde(default)]
    pub name: String,
    /// Where it is.
    pub loc: Location,
    /// When it was made (Unix seconds, from the caller).
    #[serde(default)]
    pub created: i64,
}

/// One page's part of a highlight (ETC-3). A highlight over several pages is one piece per page,
/// tied together by the same `group`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Highlight {
    /// Unique within the document's record.
    #[serde(default)]
    pub id: u64,
    /// The highlight this piece belongs to; the pieces of one highlight share it. Group numbers
    /// come from the same counter as ids.
    #[serde(default)]
    pub group: u64,
    /// The colour (the same for every piece of a group).
    #[serde(default)]
    pub color: HighlightColor,
    /// The page (zero-based).
    pub page: u32,
    /// The characters of the page, as character indices of its text.
    pub chars: Range<u32>,
    /// The areas to tint, one per line, in page space.
    pub rects: Vec<Rect>,
    /// The highlighted text of this page.
    #[serde(default)]
    pub text: String,
    /// The reader's note (the same for every piece of a group); empty when there is none.
    #[serde(default)]
    pub note: String,
    /// When it was made (Unix seconds, from the caller).
    #[serde(default)]
    pub created: i64,
}

impl Highlight {
    /// Where the piece starts: the top-left corner of its first line.
    pub fn loc(&self) -> Location {
        match self.rects.first() {
            Some(r) => Location::new(self.page, r.x0, r.y0),
            None => Location::new(self.page, 0.0, 0.0),
        }
    }
}

/// A mark with a capital letter: a place in some document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalMark {
    /// The document's path when the mark was set (moved along by [`Annotations::follow`]).
    pub path: PathBuf,
    /// Its content fingerprint, if it was known.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// The place.
    pub loc: Location,
}

/// What an [`Item`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    /// A bookmark; the id is its `id`.
    Bookmark,
    /// A highlight; the id is its `group`.
    Highlight,
}

/// A bookmark or a highlight of a document, for lists in document order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    /// Bookmark or highlight.
    pub kind: ItemKind,
    /// The bookmark's id or the highlight's group.
    pub id: u64,
    /// Where it is.
    pub loc: Location,
}

/// Orders locations in reading order: page, then height, then left to right.
pub fn loc_cmp(a: &Location, b: &Location) -> Ordering {
    a.page
        .cmp(&b.page)
        .then(a.y.total_cmp(&b.y))
        .then(a.x.total_cmp(&b.x))
}

fn highlight_cmp(a: &Highlight, b: &Highlight) -> Ordering {
    loc_cmp(&a.loc(), &b.loc()).then(a.id.cmp(&b.id))
}

/// Whether two character ranges share a character.
fn overlaps(a: &Range<u32>, b: &Range<u32>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Everything marked in one document.
#[derive(Clone, Debug, PartialEq)]
pub struct DocAnnotations {
    /// Where the file was when last used. Compared as given (the caller normalizes).
    pub path: PathBuf,
    /// XXH3-128 of the file contents, once known.
    pub fingerprint: Option<String>,
    /// When the record was last used (Unix seconds, from the caller).
    pub last_used: i64,
    /// In document order.
    bookmarks: Vec<Bookmark>,
    /// Lower-case letter → place.
    marks: BTreeMap<char, Location>,
    /// In document order (page, then top, then left).
    highlights: Vec<Highlight>,
    next_id: u64,
}

impl DocAnnotations {
    /// An empty record.
    pub fn new(path: PathBuf, fingerprint: Option<String>, now: i64) -> DocAnnotations {
        DocAnnotations {
            path,
            fingerprint,
            last_used: now,
            bookmarks: Vec::new(),
            marks: BTreeMap::new(),
            highlights: Vec::new(),
            next_id: 1,
        }
    }

    /// Nothing is marked.
    pub fn is_empty(&self) -> bool {
        self.bookmarks.is_empty() && self.marks.is_empty() && self.highlights.is_empty()
    }

    fn issue_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    // --- bookmarks -------------------------------------------------------------------------

    /// The bookmarks, in document order.
    pub fn bookmarks(&self) -> &[Bookmark] {
        &self.bookmarks
    }

    /// The bookmark with this id.
    pub fn bookmark(&self, id: u64) -> Option<&Bookmark> {
        self.bookmarks.iter().find(|b| b.id == id)
    }

    /// Adds a bookmark and returns its id.
    pub fn add_bookmark(&mut self, name: &str, loc: Location, now: i64) -> u64 {
        let id = self.issue_id();
        let bookmark = Bookmark {
            id,
            name: name.to_string(),
            loc,
            created: now,
        };
        let at = self
            .bookmarks
            .partition_point(|b| loc_cmp(&b.loc, &loc) != Ordering::Greater);
        self.bookmarks.insert(at, bookmark);
        id
    }

    /// Renames a bookmark. `false` when there is none with this id.
    pub fn rename_bookmark(&mut self, id: u64, name: &str) -> bool {
        match self.bookmarks.iter_mut().find(|b| b.id == id) {
            Some(b) => {
                b.name = name.to_string();
                true
            }
            None => false,
        }
    }

    /// Removes a bookmark. `false` when there is none with this id.
    pub fn remove_bookmark(&mut self, id: u64) -> bool {
        let before = self.bookmarks.len();
        self.bookmarks.retain(|b| b.id != id);
        self.bookmarks.len() != before
    }

    /// A bookmark at the same spot as `loc`: on the same page, at most [`SAME_SPOT_PT`] above
    /// or below. The nearest one when there are several.
    pub fn bookmark_near(&self, loc: Location) -> Option<u64> {
        self.bookmarks
            .iter()
            .filter(|b| b.loc.page == loc.page && (b.loc.y - loc.y).abs() <= SAME_SPOT_PT)
            .min_by(|a, b| (a.loc.y - loc.y).abs().total_cmp(&(b.loc.y - loc.y).abs()))
            .map(|b| b.id)
    }

    // --- marks -----------------------------------------------------------------------------

    /// The marks of the document: lower-case letter → place.
    pub fn marks(&self) -> &BTreeMap<char, Location> {
        &self.marks
    }

    /// Sets mark `letter` (a to z) to `loc`, replacing an older one. `false` for another letter.
    pub fn set_mark(&mut self, letter: char, loc: Location) -> bool {
        if !letter.is_ascii_lowercase() {
            return false;
        }
        self.marks.insert(letter, loc);
        true
    }

    /// The place of mark `letter`.
    pub fn mark(&self, letter: char) -> Option<Location> {
        self.marks.get(&letter).copied()
    }

    // --- highlights ------------------------------------------------------------------------

    /// Every highlight piece, in document order.
    pub fn highlights(&self) -> &[Highlight] {
        &self.highlights
    }

    /// The pieces on the pages `pages`, in document order. Found by bisection, so a document
    /// with thousands of highlights costs nothing per frame for the pages that have none.
    pub fn highlights_on(&self, pages: Range<u32>) -> &[Highlight] {
        let start = self.highlights.partition_point(|h| h.page < pages.start);
        let end = self.highlights.partition_point(|h| h.page < pages.end);
        &self.highlights[start..end.max(start)]
    }

    /// The pieces of highlight `group`, in document order.
    pub fn group(&self, group: u64) -> Vec<&Highlight> {
        self.highlights
            .iter()
            .filter(|h| h.group == group)
            .collect()
    }

    /// The first piece of every highlight, in document order (one per group).
    pub fn group_heads(&self) -> Vec<&Highlight> {
        let mut seen = HashSet::new();
        self.highlights
            .iter()
            .filter(|h| seen.insert(h.group))
            .collect()
    }

    /// The text of highlight `group`: the texts of its pieces joined with a blank.
    pub fn group_text(&self, group: u64) -> String {
        let parts: Vec<&str> = self
            .highlights
            .iter()
            .filter(|h| h.group == group)
            .map(|h| h.text.as_str())
            .filter(|t| !t.is_empty())
            .collect();
        parts.join(" ")
    }

    /// Highlights the characters `pieces` (page, character range on that page; one per page) in
    /// `color` and returns the new highlight's group. `texts` gives the text of a page, from
    /// which each piece's rectangles and text are made; a page without text or an empty range
    /// is left out. `None` when nothing is left.
    ///
    /// An existing highlight of the same colour that shares characters with the new one is
    /// merged into it: its pieces join the new group, and on a page where both have a piece the
    /// two become one covering both. Highlights of another colour are left as they are.
    pub fn add_highlight(
        &mut self,
        color: HighlightColor,
        pieces: &[(u32, Range<u32>)],
        texts: &dyn Fn(u32) -> Option<Arc<PageText>>,
        opts: CopyOptions,
        now: i64,
    ) -> Option<u64> {
        // One range per page: the new piece, widened by the same-colour pieces it touches.
        let mut ranges: BTreeMap<u32, Range<u32>> = BTreeMap::new();
        for (page, range) in pieces {
            if range.start >= range.end {
                continue;
            }
            ranges
                .entry(*page)
                .and_modify(|r| *r = r.start.min(range.start)..r.end.max(range.end))
                .or_insert_with(|| range.clone());
        }
        if ranges.is_empty() {
            return None;
        }
        let merged: HashSet<u64> = self
            .highlights
            .iter()
            .filter(|h| {
                h.color == color && ranges.get(&h.page).is_some_and(|r| overlaps(r, &h.chars))
            })
            .map(|h| h.group)
            .collect();
        let mut note_parts: Vec<String> = Vec::new();
        let mut created = now;
        let mut kept: Vec<Highlight> = Vec::new();
        for h in self.highlights.iter().filter(|h| merged.contains(&h.group)) {
            if !h.note.is_empty() && !note_parts.contains(&h.note) {
                note_parts.push(h.note.clone());
            }
            created = created.min(h.created);
            match ranges.get_mut(&h.page) {
                Some(r) => *r = r.start.min(h.chars.start)..r.end.max(h.chars.end),
                // A page of a merged highlight that the new one does not reach stays as it is.
                None => kept.push(h.clone()),
            }
        }
        let note = note_parts.join("\n");
        let group = self.issue_id();
        let mut new_pieces = Vec::new();
        for (page, range) in ranges {
            let Some(text) = texts(page) else {
                continue;
            };
            let rects = text.rects(range.clone());
            if rects.is_empty() {
                continue;
            }
            let id = self.issue_id();
            new_pieces.push(Highlight {
                id,
                group,
                color,
                page,
                text: text.text(range.clone(), opts),
                chars: range,
                rects,
                note: note.clone(),
                created,
            });
        }
        if new_pieces.is_empty() {
            return None;
        }
        self.highlights.retain(|h| !merged.contains(&h.group));
        for mut h in kept {
            h.group = group;
            h.note = note.clone();
            h.created = created;
            new_pieces.push(h);
        }
        self.highlights.extend(new_pieces);
        self.highlights.sort_by(highlight_cmp);
        Some(group)
    }

    /// Changes the colour of highlight `group`. `false` when there is none.
    pub fn set_highlight_color(&mut self, group: u64, color: HighlightColor) -> bool {
        let mut found = false;
        for h in self.highlights.iter_mut().filter(|h| h.group == group) {
            h.color = color;
            found = true;
        }
        found
    }

    /// Changes the note of highlight `group` (trimmed; empty removes it). `false` when there is
    /// no such highlight.
    pub fn set_highlight_note(&mut self, group: u64, note: &str) -> bool {
        let note = note.trim();
        let mut found = false;
        for h in self.highlights.iter_mut().filter(|h| h.group == group) {
            h.note = note.to_string();
            found = true;
        }
        found
    }

    /// Removes every piece of highlight `group`. `false` when there is none.
    pub fn remove_highlight(&mut self, group: u64) -> bool {
        let before = self.highlights.len();
        self.highlights.retain(|h| h.group != group);
        self.highlights.len() != before
    }

    // --- lists -----------------------------------------------------------------------------

    /// Every bookmark and highlight (one entry per highlight, at its first piece) in document
    /// order. Marks are not items.
    pub fn items(&self) -> Vec<Item> {
        let mut items: Vec<Item> = self
            .bookmarks
            .iter()
            .map(|b| Item {
                kind: ItemKind::Bookmark,
                id: b.id,
                loc: b.loc,
            })
            .chain(self.group_heads().into_iter().map(|h| Item {
                kind: ItemKind::Highlight,
                id: h.group,
                loc: h.loc(),
            }))
            .collect();
        items.sort_by(|a, b| loc_cmp(&a.loc, &b.loc));
        items
    }

    /// Puts everything in order after loading and makes the ids unique.
    fn tidy(&mut self) {
        let mut seen = HashSet::new();
        let mut max = 0;
        let mut needs_id = Vec::new();
        for (i, b) in self.bookmarks.iter().enumerate() {
            if b.id == 0 || !seen.insert(b.id) {
                needs_id.push(i);
            }
            max = max.max(b.id);
        }
        for h in &self.highlights {
            max = max.max(h.id).max(h.group);
        }
        self.next_id = self.next_id.max(max + 1);
        for i in needs_id {
            let id = self.issue_id();
            self.bookmarks[i].id = id;
            seen.insert(id);
        }
        // Highlight piece ids: unique among themselves and apart from the bookmarks.
        let mut piece_seen: HashSet<u64> = seen.clone();
        for i in 0..self.highlights.len() {
            let id = self.highlights[i].id;
            if id == 0 || !piece_seen.insert(id) {
                let id = self.issue_id();
                self.highlights[i].id = id;
                piece_seen.insert(id);
            }
            if self.highlights[i].group == 0 {
                let group = self.issue_id();
                self.highlights[i].group = group;
            }
        }
        self.bookmarks.sort_by(|a, b| loc_cmp(&a.loc, &b.loc));
        self.highlights.sort_by(highlight_cmp);
    }
}

/// Every document's marks and the global marks.
#[derive(Clone, Debug, PartialEq)]
pub struct Annotations {
    /// Format version of the file.
    pub version: u32,
    docs: Vec<DocAnnotations>,
    /// Upper-case letter → place in some document.
    global_marks: BTreeMap<char, GlobalMark>,
}

impl Default for Annotations {
    fn default() -> Annotations {
        Annotations {
            version: FORMAT_VERSION,
            docs: Vec::new(),
            global_marks: BTreeMap::new(),
        }
    }
}

impl Annotations {
    /// Reads the file. Never fails: a missing or empty file gives nothing, a damaged one gives
    /// nothing and is moved to `.bad`. A damaged document, bookmark, mark or highlight is left
    /// out alone (the original is then copied to `.bad`).
    pub fn load(path: &Path) -> Annotations {
        load_with(path, "annotations", parse_annotations)
    }

    /// The file's bytes. Records without anything in them are left out.
    pub fn to_bytes(&self) -> io::Result<Vec<u8>> {
        let docs: Vec<Value> = self
            .docs
            .iter()
            .filter(|d| !d.is_empty())
            .map(doc_to_value)
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
        let marks: serde_json::Map<String, Value> = self
            .global_marks
            .iter()
            .map(|(c, m)| Ok((c.to_string(), serde_json::to_value(m)?)))
            .collect::<Result<_, serde_json::Error>>()
            .map_err(io::Error::other)?;
        let out = serde_json::json!({
            "version": FORMAT_VERSION,
            "documents": docs,
            "global_marks": marks,
        });
        serde_json::to_vec_pretty(&out).map_err(io::Error::other)
    }

    /// Writes the file with [`atomic_write`].
    pub fn save(&self, path: &Path) -> io::Result<()> {
        atomic_write(path, &self.to_bytes()?)
    }

    /// Every document record, in no particular order.
    pub fn docs(&self) -> &[DocAnnotations] {
        &self.docs
    }

    /// The position in `docs` of the record of a document: among the records with the same
    /// `path` or the same `fingerprint`, the one used most recently (on a tie, the path match
    /// wins). The rule of `DocStateDb::lookup`.
    fn find(&self, path: &Path, fingerprint: Option<&str>) -> Option<usize> {
        self.docs
            .iter()
            .enumerate()
            .filter_map(|(i, rec)| {
                let by_path = rec.path == path;
                let by_fingerprint = matches!(
                    (fingerprint, rec.fingerprint.as_deref()),
                    (Some(a), Some(b)) if a == b
                );
                (by_path || by_fingerprint).then_some((i, rec.last_used, by_path))
            })
            .max_by_key(|(_, last_used, by_path)| (*last_used, *by_path))
            .map(|(i, _, _)| i)
    }

    /// The position in [`Annotations::docs`] of the record of a document (found as for
    /// [`Annotations::doc`]).
    pub fn position(&self, path: &Path, fingerprint: Option<&str>) -> Option<usize> {
        self.find(path, fingerprint)
    }

    /// The record of a document (see [`Annotations::follow`] for how it is found).
    pub fn doc(&self, path: &Path, fingerprint: Option<&str>) -> Option<&DocAnnotations> {
        self.find(path, fingerprint).map(|i| &self.docs[i])
    }

    /// The record of a document, made when there is none. The record takes `path` (and the
    /// fingerprint, when given) and is marked as used at `now`.
    pub fn doc_mut(
        &mut self,
        path: &Path,
        fingerprint: Option<&str>,
        now: i64,
    ) -> &mut DocAnnotations {
        let i = match self.find(path, fingerprint) {
            Some(i) => {
                self.adopt(i, path, fingerprint);
                i
            }
            None => {
                self.docs.push(DocAnnotations::new(
                    path.to_path_buf(),
                    fingerprint.map(str::to_string),
                    now,
                ));
                self.docs.len() - 1
            }
        };
        let rec = &mut self.docs[i];
        rec.last_used = rec.last_used.max(now);
        rec
    }

    /// Brings the record of the document at `path` with content `fingerprint` up to date: a
    /// record found by fingerprint under another path (the file was moved or renamed) takes the
    /// new path, and so do the global marks that pointed at the old one; a record found by path
    /// takes the new fingerprint (a new version at the same place). Returns whether anything
    /// changed.
    pub fn follow(&mut self, path: &Path, fingerprint: &str) -> bool {
        match self.find(path, Some(fingerprint)) {
            Some(i) => self.adopt(i, path, Some(fingerprint)),
            None => false,
        }
    }

    /// Gives record `i` the path and fingerprint of the document it was found for.
    fn adopt(&mut self, i: usize, path: &Path, fingerprint: Option<&str>) -> bool {
        let mut changed = false;
        let old_path = self.docs[i].path.clone();
        if old_path != path {
            let rec = &mut self.docs[i];
            rec.path = path.to_path_buf();
            changed = true;
            for mark in self.global_marks.values_mut() {
                let same_content = matches!(
                    (fingerprint, mark.fingerprint.as_deref()),
                    (Some(a), Some(b)) if a == b
                );
                if mark.path == old_path || same_content {
                    mark.path = path.to_path_buf();
                }
            }
        }
        if let Some(fp) = fingerprint
            && self.docs[i].fingerprint.as_deref() != Some(fp)
        {
            self.docs[i].fingerprint = Some(fp.to_string());
            changed = true;
        }
        if let Some(fp) = fingerprint {
            for mark in self.global_marks.values_mut() {
                if mark.path == path && mark.fingerprint.as_deref() != Some(fp) {
                    mark.fingerprint = Some(fp.to_string());
                    changed = true;
                }
            }
        }
        changed
    }

    /// Sets global mark `letter` (A to Z) to `loc` in the document at `path`. `false` for
    /// another letter.
    pub fn set_global_mark(
        &mut self,
        letter: char,
        path: &Path,
        fingerprint: Option<&str>,
        loc: Location,
    ) -> bool {
        if !letter.is_ascii_uppercase() {
            return false;
        }
        self.global_marks.insert(
            letter,
            GlobalMark {
                path: path.to_path_buf(),
                fingerprint: fingerprint.map(str::to_string),
                loc,
            },
        );
        true
    }

    /// Global mark `letter`.
    pub fn global_mark(&self, letter: char) -> Option<&GlobalMark> {
        self.global_marks.get(&letter)
    }

    /// Every global mark, by letter.
    pub fn global_marks(&self) -> &BTreeMap<char, GlobalMark> {
        &self.global_marks
    }
}

// ---------------------------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------------------------

fn doc_to_value(doc: &DocAnnotations) -> Result<Value, serde_json::Error> {
    let marks: serde_json::Map<String, Value> = doc
        .marks
        .iter()
        .map(|(c, loc)| Ok((c.to_string(), serde_json::to_value(loc)?)))
        .collect::<Result<_, serde_json::Error>>()?;
    Ok(serde_json::json!({
        "path": doc.path,
        "fingerprint": doc.fingerprint,
        "last_used": doc.last_used,
        "next_id": doc.next_id,
        "bookmarks": doc.bookmarks,
        "marks": marks,
        "highlights": doc.highlights,
    }))
}

/// The one letter of a key, when the key is one letter that `ok` accepts.
fn letter_of(key: &str, ok: fn(&char) -> bool) -> Option<char> {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if ok(&c) => Some(c),
        _ => None,
    }
}

fn finite_loc(loc: &Location) -> bool {
    loc.x.is_finite() && loc.y.is_finite()
}

/// Reads the items of `value` (an array) that parse and pass `keep`; a dropped one is noted.
fn items_of<T: serde::de::DeserializeOwned>(
    value: Option<&Value>,
    what: &str,
    keep: impl Fn(&T) -> bool,
    dropped: &mut Vec<String>,
) -> Vec<T> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        dropped.push(format!("{what}: expected an array"));
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match serde_json::from_value::<T>(item.clone()) {
            Ok(v) if keep(&v) => out.push(v),
            Ok(_) => dropped.push(format!("{what}[{i}]: invalid value")),
            Err(e) => dropped.push(format!("{what}[{i}]: {e}")),
        }
    }
    out
}

/// Reads a map of one-letter keys; bad keys and values are noted and left out.
fn letter_map<T: serde::de::DeserializeOwned>(
    value: Option<&Value>,
    what: &str,
    ok: fn(&char) -> bool,
    keep: impl Fn(&T) -> bool,
    dropped: &mut Vec<String>,
) -> BTreeMap<char, T> {
    let mut out = BTreeMap::new();
    let Some(value) = value else {
        return out;
    };
    let Some(obj) = value.as_object() else {
        dropped.push(format!("{what}: expected an object"));
        return out;
    };
    for (key, v) in obj {
        let Some(letter) = letter_of(key, ok) else {
            dropped.push(format!("{what}.{key}: not a mark letter"));
            continue;
        };
        match serde_json::from_value::<T>(v.clone()) {
            Ok(v) if keep(&v) => {
                out.insert(letter, v);
            }
            Ok(_) => dropped.push(format!("{what}.{key}: invalid value")),
            Err(e) => dropped.push(format!("{what}.{key}: {e}")),
        }
    }
    out
}

fn parse_doc(value: &Value, what: &str, dropped: &mut Vec<String>) -> Option<DocAnnotations> {
    let Some(obj) = value.as_object() else {
        dropped.push(format!("{what}: expected an object"));
        return None;
    };
    let Some(path) = obj.get("path").and_then(Value::as_str) else {
        dropped.push(format!("{what}: no path"));
        return None;
    };
    let mut doc = DocAnnotations::new(
        PathBuf::from(path),
        obj.get("fingerprint")
            .and_then(Value::as_str)
            .map(str::to_string),
        obj.get("last_used").and_then(Value::as_i64).unwrap_or(0),
    );
    doc.next_id = obj.get("next_id").and_then(Value::as_u64).unwrap_or(1);
    doc.bookmarks = items_of(
        obj.get("bookmarks"),
        &format!("{what}.bookmarks"),
        |b: &Bookmark| finite_loc(&b.loc),
        dropped,
    );
    doc.marks = letter_map(
        obj.get("marks"),
        &format!("{what}.marks"),
        char::is_ascii_lowercase,
        finite_loc,
        dropped,
    );
    doc.highlights = items_of(
        obj.get("highlights"),
        &format!("{what}.highlights"),
        |h: &Highlight| {
            h.chars.start <= h.chars.end
                && h.rects.iter().all(|r| {
                    r.x0.is_finite() && r.y0.is_finite() && r.x1.is_finite() && r.y1.is_finite()
                })
        },
        dropped,
    );
    doc.tidy();
    Some(doc)
}

fn parse_annotations(bytes: &[u8]) -> Result<Loaded<Annotations>, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let obj = value
        .as_object()
        .ok_or_else(|| "top level is not an object".to_string())?;
    warn_if_newer("annotations", json_version(obj));
    let mut dropped = Vec::new();
    let mut docs: Vec<DocAnnotations> = Vec::new();
    match obj.get("documents") {
        None => {}
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                let Some(doc) = parse_doc(item, &format!("documents[{i}]"), &mut dropped) else {
                    continue;
                };
                // A hand-edited file may hold a path twice; keep the most recently used.
                match docs.iter_mut().find(|d| d.path == doc.path) {
                    Some(existing) if existing.last_used < doc.last_used => *existing = doc,
                    Some(_) => {}
                    None => docs.push(doc),
                }
            }
        }
        Some(_) => dropped.push("documents: expected an array".into()),
    }
    let global_marks = letter_map(
        obj.get("global_marks"),
        "global_marks",
        char::is_ascii_uppercase,
        |m: &GlobalMark| finite_loc(&m.loc),
        &mut dropped,
    );
    Ok(Loaded {
        value: Annotations {
            version: FORMAT_VERSION,
            docs,
            global_marks,
        },
        dropped: (!dropped.is_empty()).then(|| dropped.join("; ")),
    })
}
