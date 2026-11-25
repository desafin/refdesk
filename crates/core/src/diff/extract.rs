//! Turning one document into a flow of paragraphs and words (`docs/SPEC.md` §8.5 steps 1–3).
//!
//! - A paragraph is a [`TextBlock`](crate::text::TextBlock) of the page text, minus the lines
//!   [`noise`] found. Blocks with no words left are dropped. Lines that belong to no block are
//!   not read (the engine puts every line in a block).
//! - A paragraph that runs over a page break is joined back: the last paragraph so far does not
//!   end with `.`, `!`, `?` or `:` (closing quotes and brackets after them are skipped), and the
//!   first paragraph of the next page that has text starts with a lowercase letter.
//! - Words are the runs of non-whitespace characters. Punctuation stays with its word.
//! - A word broken at a line end ("speci-" / "fication": a letter and `-`, U+2010 or U+00AD at
//!   the end of the line, a lowercase letter at the start of the next) becomes one word without
//!   the hyphen. Across a page break the second piece goes into [`Word::tail`].
//! - The comparison key of a word is [`normalize_key`]: NFKC, optionally lowercase, without
//!   soft hyphens and zero-width characters. Displayed text is always the original characters.

use std::ops::Range;
use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;
use xxhash_rust::xxh3::xxh3_64;

use crate::geom::Size;
use crate::text::{PageText, TextPos};

use super::noise;

/// How [`extract`] reads a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtractOptions {
    /// Drop header and footer lines that repeat over the pages ([`noise`]).
    pub skip_repeated_lines: bool,
    /// Compare words without regard to case (keys are lowercased).
    pub ignore_case: bool,
}

impl Default for ExtractOptions {
    /// Skip repeated lines, keep case.
    fn default() -> Self {
        Self {
            skip_repeated_lines: true,
            ignore_case: false,
        }
    }
}

/// The input for one page: its text and its size in pt.
#[derive(Clone, Debug)]
pub struct PageSource {
    /// The page text, as the engine produced it.
    pub text: Arc<PageText>,
    /// The page size in pt; the header and footer bands are measured from it.
    pub size: Size,
}

/// A document as a flow of words and paragraphs.
#[derive(Clone, Debug, Default)]
pub struct DocTokens {
    /// The text of every page, in page order. Word and paragraph positions index into these.
    pub pages: Vec<Arc<PageText>>,
    /// All words in document order. Words of noise lines are not here.
    pub words: Vec<Word>,
    /// All paragraphs in document order. Each covers a non-empty range of `words`.
    pub paragraphs: Vec<Paragraph>,
    /// The lines dropped as repeated headers or footers, in page and line order.
    pub removed_lines: Vec<RemovedLine>,
}

/// One word with its comparison key and where it is on the pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    /// The normalised text used for comparison ([`normalize_key`]). Never empty.
    pub key: String,
    /// Zero-based page of the word (of its first piece, if it is broken over a page).
    pub page: u32,
    /// The characters of the word on `page`, an index range into that page's
    /// [`PageText::chars`]. A word broken at a line end inside the page covers both pieces.
    pub chars: Range<u32>,
    /// The second piece of a word broken at the last line end of a page: the page and character
    /// range on it.
    pub tail: Option<(u32, Range<u32>)>,
}

impl Word {
    /// The position of the first character.
    pub fn start(&self) -> TextPos {
        TextPos {
            page: self.page,
            ch: self.chars.start,
        }
    }

    /// The boundary position after the last character (as in a [`Selection`](crate::Selection)).
    pub fn end(&self) -> TextPos {
        match &self.tail {
            Some((page, r)) => TextPos {
                page: *page,
                ch: r.end,
            },
            None => TextPos {
                page: self.page,
                ch: self.chars.end,
            },
        }
    }

    /// The pieces of the word as `(page, character range)`: one, or two for a word broken over
    /// a page.
    pub fn segments(&self) -> impl Iterator<Item = (u32, Range<u32>)> + '_ {
        std::iter::once((self.page, self.chars.clone())).chain(self.tail.clone())
    }
}

/// A paragraph: a run of words with a hash of their keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paragraph {
    /// Index range into [`DocTokens::words`]. Never empty.
    pub words: Range<u32>,
    /// xxh3 hash of the word keys joined by single spaces. Paragraphs with the same words have
    /// the same key, wherever their lines are broken; the value is stable between runs.
    pub key: u64,
    /// The position of the first character of the first word.
    pub start: TextPos,
    /// The boundary position after the last character of the last word.
    pub end: TextPos,
}

/// A line dropped as a repeated header or footer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovedLine {
    /// Zero-based page.
    pub page: u32,
    /// Index into that page's [`PageText::lines`].
    pub line: u32,
    /// The line text with surrounding whitespace removed.
    pub text: String,
}

impl DocTokens {
    /// The words of `paragraph` (empty if its range is out of bounds).
    pub fn paragraph_words(&self, paragraph: &Paragraph) -> &[Word] {
        let r = &paragraph.words;
        self.words
            .get(r.start as usize..r.end as usize)
            .unwrap_or(&[])
    }

    /// The original characters of `word`, both pieces of a broken word included (with the
    /// hyphen). Characters that do not exist are skipped.
    pub fn word_text(&self, word: &Word) -> String {
        let mut out = String::new();
        for (page, r) in word.segments() {
            if let Some(pt) = self.pages.get(page as usize) {
                out.extend(chars_of(pt, &r).iter().map(|c| c.ch));
            }
        }
        out
    }
}

/// The comparison key of a word: soft hyphens (U+00AD) and zero-width characters removed, NFKC
/// (which also splits ligatures such as "ﬁ"), and lowercased when `ignore_case` is set.
pub fn normalize_key(raw: &str, ignore_case: bool) -> String {
    let nfkc: String = raw.chars().filter(|&c| !is_ignorable(c)).nfkc().collect();
    if ignore_case {
        nfkc.to_lowercase()
    } else {
        nfkc
    }
}

/// Characters that never take part in a comparison.
fn is_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
    )
}

/// Hyphen characters that may end a line in the middle of a word.
fn is_hyphen(c: char) -> bool {
    matches!(c, '-' | '\u{2010}' | '\u{00AD}')
}

/// Whether `before` ends in "letter + hyphen" and `next` starts with a lowercase letter.
fn hyphen_break(before: &str, next: &str) -> bool {
    let mut rev = before.chars().rev();
    let hyphen = rev.next().is_some_and(is_hyphen);
    let letter = rev.next().is_some_and(char::is_alphabetic);
    hyphen && letter && next.chars().next().is_some_and(char::is_lowercase)
}

/// Whether a paragraph whose last word is `last` ends a sentence.
fn ends_sentence(last: &str) -> bool {
    const CLOSERS: &[char] = &[
        ')', ']', '}', '"', '\'', '\u{201D}', '\u{2019}', '\u{00BB}', '\u{203A}', '\u{300D}',
        '\u{300F}',
    ];
    let trimmed = last.trim_end_matches(CLOSERS);
    trimmed.ends_with([
        '.', '!', '?', ':', '\u{3002}', '\u{FF01}', '\u{FF1F}', '\u{FF1A}',
    ])
}

/// The characters of `r` on `pt`, cut to what exists.
fn chars_of<'a>(pt: &'a PageText, r: &Range<u32>) -> &'a [crate::text::TextChar] {
    let len = pt.chars.len();
    let end = (r.end as usize).min(len);
    let start = (r.start as usize).min(end);
    &pt.chars[start..end]
}

/// A run of non-whitespace characters on one line.
struct Piece {
    chars: Range<u32>,
    raw: String,
}

/// The pieces of `line` on `pt`, in order.
fn line_pieces(pt: &PageText, line: &Range<u32>) -> Vec<Piece> {
    let len = pt.chars.len();
    let end = (line.end as usize).min(len);
    let start = (line.start as usize).min(end);
    let mut pieces = Vec::new();
    let mut cur: Option<Piece> = None;
    for i in start..end {
        let ch = pt.chars[i].ch;
        // Indices fit: they come from a `Range<u32>`.
        let idx = i as u32;
        if ch.is_whitespace() {
            pieces.extend(cur.take());
        } else if let Some(p) = cur.as_mut() {
            p.chars.end = idx + 1;
            p.raw.push(ch);
        } else {
            cur = Some(Piece {
                chars: idx..idx + 1,
                raw: ch.to_string(),
            });
        }
    }
    pieces.extend(cur);
    pieces
}

/// A word whose key is not computed yet, because the next line may still continue it.
struct OpenWord {
    page: u32,
    chars: Range<u32>,
    tail: Option<(u32, Range<u32>)>,
    /// Original text, with the hyphen of a joined line break already removed.
    raw: String,
}

impl OpenWord {
    /// Appends `piece` (on `page`) as the continuation of a word broken at a line end.
    /// Returns `false`, changing nothing, if the word cannot hold another piece there.
    fn join(&mut self, page: u32, piece: &Piece) -> bool {
        match &mut self.tail {
            None if page == self.page => {
                self.chars =
                    self.chars.start.min(piece.chars.start)..self.chars.end.max(piece.chars.end);
            }
            None => self.tail = Some((page, piece.chars.clone())),
            Some((tp, tr)) if *tp == page => {
                *tr = tr.start.min(piece.chars.start)..tr.end.max(piece.chars.end);
            }
            Some(_) => return false,
        }
        self.raw.pop();
        self.raw.push_str(&piece.raw);
        true
    }
}

/// Builds the word and paragraph lists.
struct Builder {
    ignore_case: bool,
    words: Vec<Word>,
    paragraphs: Vec<Paragraph>,
    /// Index of the first word of the open paragraph.
    para_start: usize,
    /// The last word of the open paragraph; `None` when no paragraph is open.
    open: Option<OpenWord>,
}

impl Builder {
    /// Whether the open paragraph continues into a block whose first piece is `first`.
    fn continues_into(&self, first: &Piece) -> bool {
        self.open.as_ref().is_some_and(|w| !ends_sentence(&w.raw))
            && first.raw.chars().next().is_some_and(char::is_lowercase)
    }

    /// Adds the words of one line. `line_start` is false only for the very first line of a new
    /// paragraph, where no hyphen join is possible.
    fn add_line(&mut self, page: u32, pieces: Vec<Piece>, line_start: bool) {
        for (i, piece) in pieces.into_iter().enumerate() {
            if i == 0
                && line_start
                && let Some(open) = self.open.as_mut()
                && hyphen_break(&open.raw, &piece.raw)
                && open.join(page, &piece)
            {
                continue;
            }
            self.flush_word();
            self.open = Some(OpenWord {
                page,
                chars: piece.chars,
                tail: None,
                raw: piece.raw,
            });
        }
    }

    /// Turns the open word into a [`Word`]. A word whose key is empty is dropped.
    fn flush_word(&mut self) {
        if let Some(w) = self.open.take() {
            let key = normalize_key(&w.raw, self.ignore_case);
            if !key.is_empty() {
                self.words.push(Word {
                    key,
                    page: w.page,
                    chars: w.chars,
                    tail: w.tail,
                });
            }
        }
    }

    /// Closes the open paragraph, if it has any words.
    fn close_paragraph(&mut self) {
        self.flush_word();
        let words = &self.words[self.para_start.min(self.words.len())..];
        if let (Some(first), Some(last)) = (words.first(), words.last()) {
            let joined = words
                .iter()
                .map(|w| w.key.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            self.paragraphs.push(Paragraph {
                words: to_u32(self.para_start)..to_u32(self.words.len()),
                key: xxh3_64(joined.as_bytes()),
                start: first.start(),
                end: last.end(),
            });
        }
        self.para_start = self.words.len();
    }
}

/// `usize` → `u32`, saturating (word counts never come near `u32::MAX`).
fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Turns the pages of one document into words and paragraphs, dropping repeated header and
/// footer lines when `opts.skip_repeated_lines` is set. Pure: no IO, no engine access.
pub fn extract(pages: &[PageSource], opts: &ExtractOptions) -> DocTokens {
    let noise = if opts.skip_repeated_lines {
        noise::repeated_lines(pages)
    } else {
        Vec::new()
    };
    let is_noise = |page: usize, line: usize| {
        noise
            .get(page)
            .and_then(|p| p.get(line))
            .copied()
            .unwrap_or(false)
    };

    let mut removed_lines = Vec::new();
    for (p, src) in pages.iter().enumerate() {
        for (l, line) in src.text.lines.iter().enumerate() {
            if is_noise(p, l) {
                removed_lines.push(RemovedLine {
                    page: to_u32(p),
                    line: to_u32(l),
                    text: noise::line_text(&src.text, line).trim().to_owned(),
                });
            }
        }
    }

    let mut b = Builder {
        ignore_case: opts.ignore_case,
        words: Vec::new(),
        paragraphs: Vec::new(),
        para_start: 0,
        open: None,
    };
    for (p, src) in pages.iter().enumerate() {
        let pt = &src.text;
        let page = to_u32(p);
        let mut first_on_page = true;
        for block in &pt.blocks {
            let end = (block.lines.end as usize).min(pt.lines.len());
            let start = (block.lines.start as usize).min(end);
            let lines: Vec<Vec<Piece>> = (start..end)
                .filter(|&l| !is_noise(p, l))
                .map(|l| line_pieces(pt, &pt.lines[l].chars))
                .filter(|pieces| !pieces.is_empty())
                .collect();
            let Some(first) = lines.first().and_then(|l| l.first()) else {
                continue;
            };
            let continues = first_on_page && b.continues_into(first);
            first_on_page = false;
            if !continues {
                b.close_paragraph();
            }
            for (i, pieces) in lines.into_iter().enumerate() {
                b.add_line(page, pieces, continues || i > 0);
            }
        }
    }
    b.close_paragraph();

    DocTokens {
        pages: pages.iter().map(|s| Arc::clone(&s.text)).collect(),
        words: b.words,
        paragraphs: b.paragraphs,
        removed_lines,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::diff::test_pages::{block_page, line_page, source};

    const KEEP_NOISE: ExtractOptions = ExtractOptions {
        skip_repeated_lines: false,
        ignore_case: false,
    };

    fn run(pages: Vec<PageText>, opts: ExtractOptions) -> DocTokens {
        let sources: Vec<PageSource> = pages.into_iter().map(source).collect();
        extract(&sources, &opts)
    }

    fn keys(doc: &DocTokens) -> Vec<&str> {
        doc.words.iter().map(|w| w.key.as_str()).collect()
    }

    fn para_keys(doc: &DocTokens, p: usize) -> Vec<&str> {
        doc.paragraph_words(&doc.paragraphs[p])
            .iter()
            .map(|w| w.key.as_str())
            .collect()
    }

    /// Every word's characters, read back from the page, give its key (for words without
    /// line-break joins and normalisation).
    fn assert_ranges_spell_keys(doc: &DocTokens) {
        for w in &doc.words {
            assert_eq!(doc.word_text(w), w.key, "{w:?}");
        }
    }

    #[test]
    fn words_split_on_whitespace_and_keep_punctuation() {
        let doc = run(
            vec![block_page(&[(300.0, &["Hello, world!  (see", "foo) foo"])])],
            KEEP_NOISE,
        );
        assert_eq!(keys(&doc), ["Hello,", "world!", "(see", "foo)", "foo"]);
        assert_ranges_spell_keys(&doc);
        assert_eq!(doc.words[0].chars, 0..6);
        assert_eq!(doc.words[1].chars, 7..13);
        assert!(doc.words.iter().all(|w| w.page == 0 && w.tail.is_none()));
        assert_ne!(doc.words[3].key, doc.words[4].key);
    }

    #[test]
    fn ligatures_are_split_in_the_key_only() {
        let doc = run(
            vec![line_page(&[(300.0, "the \u{FB01}le \u{FB02}ow")])],
            KEEP_NOISE,
        );
        assert_eq!(keys(&doc), ["the", "file", "flow"]);
        // The ligature is one character on the page.
        assert_eq!(doc.words[1].chars, 4..7);
        assert_eq!(doc.word_text(&doc.words[1]), "\u{FB01}le");
    }

    #[test]
    fn ignore_case_lowercases_keys() {
        let page = || vec![line_page(&[(300.0, "Hello WORLD Straße")])];
        let opts = ExtractOptions {
            skip_repeated_lines: false,
            ignore_case: true,
        };
        assert_eq!(keys(&run(page(), opts)), ["hello", "world", "straße"]);
        assert_eq!(keys(&run(page(), KEEP_NOISE)), ["Hello", "WORLD", "Straße"]);
    }

    #[test]
    fn soft_hyphens_and_zero_width_characters_leave_the_key() {
        let doc = run(
            vec![line_page(&[(
                300.0,
                "con\u{00AD}fig zero\u{200B}width \u{200B}",
            )])],
            KEEP_NOISE,
        );
        assert_eq!(keys(&doc), ["config", "zerowidth"]);
    }

    #[test]
    fn hyphenated_line_break_joins_the_word() {
        let doc = run(
            vec![block_page(&[(300.0, &["the speci-", "fication says"])])],
            KEEP_NOISE,
        );
        assert_eq!(keys(&doc), ["the", "specification", "says"]);
        let w = &doc.words[1];
        assert_eq!(w.chars, 4..18);
        assert_eq!(w.tail, None);
        assert_eq!(doc.word_text(w), "speci-fication");
        assert_eq!(doc.paragraphs.len(), 1);
    }

    #[test]
    fn other_hyphen_characters_join_too() {
        for hyphen in ['\u{2010}', '\u{00AD}'] {
            let first = format!("the speci{hyphen}");
            let doc = run(
                vec![block_page(&[(300.0, &[&first, "fication"])])],
                KEEP_NOISE,
            );
            assert_eq!(keys(&doc), ["the", "specification"], "{hyphen:?}");
        }
    }

    #[test]
    fn hyphen_before_a_capital_or_after_a_non_letter_stays() {
        let doc = run(
            vec![block_page(&[(
                300.0,
                &["uses Wi-", "Fi and 3-", "way and -", "x"],
            )])],
            KEEP_NOISE,
        );
        assert_eq!(
            keys(&doc),
            ["uses", "Wi-", "Fi", "and", "3-", "way", "and", "-", "x"]
        );
        assert_ranges_spell_keys(&doc);
    }

    #[test]
    fn hyphenated_page_break_puts_the_second_piece_in_the_tail() {
        let doc = run(
            vec![
                block_page(&[(300.0, &["Intro."]), (400.0, &["the speci-"])]),
                block_page(&[(100.0, &["fication is here."])]),
            ],
            KEEP_NOISE,
        );
        assert_eq!(
            keys(&doc),
            ["Intro.", "the", "specification", "is", "here."]
        );
        let w = &doc.words[2];
        assert_eq!((w.page, w.chars.clone()), (0, 10..16));
        assert_eq!(w.tail, Some((1, 0..8)));
        assert_eq!(w.end(), TextPos { page: 1, ch: 8 });
        assert_eq!(doc.word_text(w), "speci-fication");
        assert_eq!(doc.paragraphs.len(), 2);
        assert_eq!(para_keys(&doc, 1), ["the", "specification", "is", "here."]);
    }

    #[test]
    fn paragraph_broken_by_a_page_is_joined() {
        let doc = run(
            vec![
                block_page(&[(300.0, &["First one."]), (600.0, &["The parser reads the"])]),
                block_page(&[(100.0, &["configuration file first."]), (200.0, &["Next."])]),
            ],
            KEEP_NOISE,
        );
        assert_eq!(doc.paragraphs.len(), 3);
        assert_eq!(
            para_keys(&doc, 1),
            [
                "The",
                "parser",
                "reads",
                "the",
                "configuration",
                "file",
                "first."
            ]
        );
        let p = &doc.paragraphs[1];
        assert_eq!(p.start, TextPos { page: 0, ch: 10 });
        assert_eq!(p.end, TextPos { page: 1, ch: 25 });
        assert_eq!(p.words, 2..9);
    }

    #[test]
    fn sentence_end_stops_the_join() {
        for last in [
            "done.",
            "done!",
            "done?",
            "follows:",
            "(done.)",
            "\u{201C}done.\u{201D}",
        ] {
            let doc = run(
                vec![
                    line_page(&[(600.0, &format!("It is {last}"))]),
                    line_page(&[(100.0, "and then more")]),
                ],
                KEEP_NOISE,
            );
            assert_eq!(doc.paragraphs.len(), 2, "{last}");
        }
    }

    #[test]
    fn capital_start_on_the_next_page_stops_the_join() {
        let doc = run(
            vec![
                line_page(&[(600.0, "A heading without a stop")]),
                line_page(&[(100.0, "The next paragraph")]),
            ],
            KEEP_NOISE,
        );
        assert_eq!(doc.paragraphs.len(), 2);
    }

    #[test]
    fn join_skips_header_lines_and_pages_without_text() {
        // The engine may put the header after the body in reading order.
        let pages: Vec<PageText> = (1..=4)
            .map(|n| {
                let body = match n {
                    1 => "the text runs on",
                    2 => "over the page",
                    3 => "",
                    _ => "and past a blank page.",
                };
                line_page(&[(300.0, body), (30.0, &format!("Manual v2 — Page {n}"))])
            })
            .collect();
        let doc = run(pages, ExtractOptions::default());
        assert_eq!(doc.removed_lines.len(), 4);
        assert_eq!(doc.paragraphs.len(), 1);
        assert_eq!(doc.words.len(), 12);
        assert!(doc.words.iter().all(|w| !w.key.contains("Page")));
    }

    #[test]
    fn removed_lines_are_reported_and_switchable() {
        let pages = || -> Vec<PageText> {
            (1..=3)
                .map(|n| {
                    line_page(&[
                        (30.0, &format!("  Spec v1.{n} — Page {n} ")),
                        (300.0, "Body."),
                    ])
                })
                .collect()
        };
        let doc = run(pages(), ExtractOptions::default());
        assert_eq!(
            doc.removed_lines,
            (1..=3)
                .map(|n| RemovedLine {
                    page: n - 1,
                    line: 0,
                    text: format!("Spec v1.{n} — Page {n}"),
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(keys(&doc), ["Body.", "Body.", "Body."]);

        let kept = run(pages(), KEEP_NOISE);
        assert!(kept.removed_lines.is_empty());
        // "Spec v1.n — Page n" is five words, "Body." one.
        assert_eq!(kept.words.len(), 3 * 6);
    }

    #[test]
    fn paragraph_key_follows_the_words_only() {
        let doc = run(
            vec![block_page(&[
                (100.0, &["The worker renders the", "visible pages first."]),
                (200.0, &["The worker renders", "the visible pages first."]),
                (300.0, &["The worker renders the visible", "pages first."]),
                (400.0, &["The worker renders the", "visible pages last."]),
                (500.0, &["The worker renders the", "visible pages first"]),
            ])],
            KEEP_NOISE,
        );
        let k: Vec<u64> = doc.paragraphs.iter().map(|p| p.key).collect();
        assert_eq!(k.len(), 5);
        assert_eq!(k[0], k[1]);
        assert_eq!(k[0], k[2]);
        assert_ne!(k[0], k[3]);
        assert_ne!(k[0], k[4]);
        assert_eq!(
            k[0],
            xxh3_64(b"The worker renders the visible pages first."),
            "stable between runs"
        );
    }

    #[test]
    fn paragraph_key_with_ignore_case() {
        let page = || {
            vec![block_page(&[
                (100.0, &["Read THE file"]),
                (200.0, &["read the File"]),
            ])]
        };
        let opts = ExtractOptions {
            skip_repeated_lines: false,
            ignore_case: true,
        };
        let doc = run(page(), opts);
        assert_eq!(doc.paragraphs[0].key, doc.paragraphs[1].key);
        let doc = run(page(), KEEP_NOISE);
        assert_ne!(doc.paragraphs[0].key, doc.paragraphs[1].key);
    }

    #[test]
    fn empty_documents_pages_and_blocks() {
        let doc = extract(&[], &ExtractOptions::default());
        assert!(doc.pages.is_empty() && doc.words.is_empty() && doc.paragraphs.is_empty());

        // An empty page, a page with only blank blocks, a block without lines, a block whose
        // line range is out of bounds and a line whose character range is out of bounds.
        let mut odd = block_page(&[(100.0, &[""]), (200.0, &["   "]), (300.0, &["word"])]);
        odd.blocks.push(crate::text::TextBlock {
            lines: 2..2,
            bounds: Default::default(),
        });
        odd.blocks.push(crate::text::TextBlock {
            lines: 1..99,
            bounds: Default::default(),
        });
        odd.lines[2].chars = 5..99;
        let doc = run(
            vec![PageText::default(), odd, PageText::default()],
            ExtractOptions::default(),
        );
        assert_eq!(doc.pages.len(), 3);
        // "word" (characters 3..7) cut to "rd"; the out-of-range block reads lines 1 and 2 again.
        assert_eq!(keys(&doc), ["rd", "rd"]);
        assert_eq!(doc.paragraphs.len(), 2);
        for p in &doc.paragraphs {
            assert!(!p.words.is_empty());
            assert!(!doc.paragraph_words(p).is_empty());
        }
    }

    #[test]
    fn segments_and_positions_of_a_word() {
        let w = Word {
            key: "x".into(),
            page: 2,
            chars: 5..9,
            tail: Some((3, 0..4)),
        };
        assert_eq!(w.start(), TextPos { page: 2, ch: 5 });
        assert_eq!(w.segments().collect::<Vec<_>>(), [(2, 5..9), (3, 0..4)]);
        let doc = DocTokens::default();
        assert_eq!(doc.word_text(&w), "");
        let bad = Paragraph {
            words: 3..7,
            key: 0,
            start: w.start(),
            end: w.end(),
        };
        assert!(doc.paragraph_words(&bad).is_empty());
    }
}
