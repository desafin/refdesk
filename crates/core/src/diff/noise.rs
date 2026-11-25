//! Finding repeated header and footer lines (`docs/SPEC.md` §8.5 step 2).
//!
//! Headers, footers and page numbers repeat on every page and differ between versions (version
//! string, date, page number). Left in, they would fill the list of changes with noise.
//!
//! A line is a candidate when its vertical centre lies in the top or bottom [`BAND`] of its page.
//! Its *shape* is its text with every run of digits replaced by `#` ([`line_shape`]). Candidates
//! are grouped by band, shape and font size (rounded to half a point, so a large chapter heading
//! is not grouped with a small running header of the same words). A group is noise when it
//! appears on at least [`MIN_PAGES`] pages and
//!
//! - on at least half of the pages, or
//! - on at least half of the odd or of the even pages (headers that alternate between left and
//!   right pages, with some pages such as the title page lacking them), or
//! - in a run of at least [`MIN_PAGES`] pages, each at most [`RUN_GAP`] pages after the previous
//!   one (a running header of one chapter or section, on every page or every other page).
//!
//! Lines outside the bands are never noise, however often they repeat: a sentence repeated in
//! the body is content.

use std::collections::{HashMap, HashSet};

use crate::text::{PageText, TextLine};

use super::extract::PageSource;

/// Height of the top and of the bottom band, as a fraction of the page height.
pub const BAND: f32 = 0.15;

/// The fewest pages a shape must appear on to count as noise. Documents with fewer pages have
/// no noise at all.
pub const MIN_PAGES: usize = 3;

/// The largest distance in pages between two occurrences of a group that still continues a run.
pub const RUN_GAP: usize = 2;

/// The top or bottom band of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Band {
    /// The top [`BAND`] of the page.
    Top,
    /// The bottom [`BAND`] of the page.
    Bottom,
}

/// The band the vertical centre of `line` lies in, on a page `page_height` pt high. `None` for
/// the middle of the page and for a page without a usable height.
pub fn line_band(line: &TextLine, page_height: f32) -> Option<Band> {
    if !(page_height.is_finite() && page_height > 0.0) {
        return None;
    }
    let centre = (line.bounds.y0 + line.bounds.y1) / 2.0;
    if !centre.is_finite() {
        return None;
    }
    if centre < page_height * BAND {
        Some(Band::Top)
    } else if centre > page_height * (1.0 - BAND) {
        Some(Band::Bottom)
    } else {
        None
    }
}

/// The shape of a line: every run of digits becomes one `#`, runs of whitespace become one
/// space, and leading and trailing whitespace is removed.
///
/// `"Spec v1.2 — Page 12"` becomes `"Spec v#.# — Page #"`.
pub fn line_shape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_digits = false;
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            in_digits = false;
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        if c.is_numeric() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

/// The median font size of the characters of `line`, rounded to half a point (as a count of
/// half points). 0 for a line without characters or sizes that are not numbers.
fn size_bucket(page: &PageText, line: &TextLine) -> i32 {
    let len = page.chars.len();
    let end = (line.chars.end as usize).min(len);
    let start = (line.chars.start as usize).min(end);
    let mut sizes: Vec<f32> = page.chars[start..end]
        .iter()
        .map(|c| c.size)
        .filter(|s| s.is_finite())
        .collect();
    if sizes.is_empty() {
        return 0;
    }
    sizes.sort_by(f32::total_cmp);
    let median = sizes[sizes.len() / 2];
    // Font sizes are far below `i32::MAX`; the cast saturates anyway.
    (median * 2.0).round() as i32
}

/// The characters of `line` as a string. Indices past the end of the page are cut off.
pub fn line_text(page: &PageText, line: &TextLine) -> String {
    let len = page.chars.len();
    let end = (line.chars.end as usize).min(len);
    let start = (line.chars.start as usize).min(end);
    page.chars[start..end].iter().map(|c| c.ch).collect()
}

/// What groups candidate lines: band, shape and font size in half points.
type Key = (Band, String, i32);

/// Whether a group that appears on the pages `on` (increasing indices) of an `n`-page document
/// is noise.
fn is_noise(on: &[usize], n: usize) -> bool {
    if on.len() < MIN_PAGES {
        return false;
    }
    let pages_by_parity = [n.div_ceil(2), n / 2];
    let mut by_parity = [0usize; 2];
    for &p in on {
        by_parity[p % 2] += 1;
    }
    let mut run = 1;
    let mut longest_run = 1;
    for w in on.windows(2) {
        run = if w[1] - w[0] <= RUN_GAP { run + 1 } else { 1 };
        longest_run = longest_run.max(run);
    }
    2 * on.len() >= n
        || (0..2).any(|k| pages_by_parity[k] > 0 && 2 * by_parity[k] >= pages_by_parity[k])
        || longest_run >= MIN_PAGES
}

/// Which lines are repeated headers or footers: `result[page][line]` is `true` for a noise line.
/// The result has one entry per page and, for each page, one entry per line of its text.
pub fn repeated_lines(pages: &[PageSource]) -> Vec<Vec<bool>> {
    let mut noise: Vec<Vec<bool>> = pages
        .iter()
        .map(|p| vec![false; p.text.lines.len()])
        .collect();
    let n = pages.len();
    if n < MIN_PAGES {
        return noise;
    }

    // The candidate key of every line (`None` outside the bands or without visible text).
    let keys: Vec<Vec<Option<Key>>> = pages
        .iter()
        .map(|p| {
            p.text
                .lines
                .iter()
                .map(|line| {
                    let band = line_band(line, p.size.h)?;
                    let shape = line_shape(&line_text(&p.text, line));
                    (!shape.is_empty()).then(|| (band, shape, size_bucket(&p.text, line)))
                })
                .collect()
        })
        .collect();

    // The pages each key appears on, in increasing order.
    let mut occurrences: HashMap<&Key, Vec<usize>> = HashMap::new();
    for (index, page_keys) in keys.iter().enumerate() {
        let distinct: HashSet<&Key> = page_keys.iter().flatten().collect();
        for key in distinct {
            occurrences.entry(key).or_default().push(index);
        }
    }

    let noisy: HashSet<&Key> = occurrences
        .iter()
        .filter(|(_, on)| is_noise(on, n))
        .map(|(k, _)| *k)
        .collect();

    for (page_keys, page_noise) in keys.iter().zip(noise.iter_mut()) {
        for (key, flag) in page_keys.iter().zip(page_noise.iter_mut()) {
            *flag = key.as_ref().is_some_and(|k| noisy.contains(k));
        }
    }
    noise
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::diff::test_pages::{PAGE_H, line_page, source};

    fn noise_count(noise: &[Vec<bool>]) -> usize {
        noise.iter().flatten().filter(|&&b| b).count()
    }

    #[test]
    fn shape_replaces_digit_runs_and_tidies_spaces() {
        assert_eq!(line_shape("Spec v1.2 — Page 12"), "Spec v#.# — Page #");
        assert_eq!(line_shape("  Page   7  "), "Page #");
        assert_eq!(line_shape("123"), "#");
        assert_eq!(line_shape("   "), "");
        assert_eq!(line_shape("2026-11-25"), "#-#-#");
    }

    #[test]
    fn band_uses_the_vertical_centre() {
        let pt = line_page(&[(100.0, "top"), (400.0, "middle"), (700.0, "bottom")]);
        let bands: Vec<_> = pt.lines.iter().map(|l| line_band(l, PAGE_H)).collect();
        assert_eq!(bands, vec![Some(Band::Top), None, Some(Band::Bottom)]);
        assert_eq!(line_band(&pt.lines[0], 0.0), None);
        assert_eq!(line_band(&pt.lines[0], f32::NAN), None);
    }

    #[test]
    fn header_with_changing_page_number_is_noise() {
        let pages: Vec<_> = (1..=10)
            .map(|n| {
                source(line_page(&[
                    (30.0, &format!("Document v1.2 — Page {n}")),
                    (300.0, &format!("Body text of page {n} differs.")),
                ]))
            })
            .collect();
        let noise = repeated_lines(&pages);
        for page in &noise {
            assert_eq!(page, &vec![true, false]);
        }
    }

    #[test]
    fn page_number_alone_in_the_bottom_band_is_noise() {
        let pages: Vec<_> = (1..=5)
            .map(|n| source(line_page(&[(300.0, "Body"), (740.0, &n.to_string())])))
            .collect();
        let noise = repeated_lines(&pages);
        assert_eq!(noise_count(&noise), 5);
        assert!(noise.iter().all(|p| p == &vec![false, true]));
    }

    #[test]
    fn same_shape_in_other_band_is_counted_separately() {
        // "Page N" at the top on two pages and at the bottom on two pages: neither band reaches
        // three pages.
        let pages: Vec<_> = (0..4)
            .map(|i| {
                let y = if i < 2 { 30.0 } else { 740.0 };
                source(line_page(&[(y, &format!("Page {i}"))]))
            })
            .collect();
        assert_eq!(noise_count(&repeated_lines(&pages)), 0);
    }

    #[test]
    fn repeated_sentence_in_the_body_is_kept() {
        let pages: Vec<_> = (0..10)
            .map(|_| source(line_page(&[(400.0, "Note: this field is reserved.")])))
            .collect();
        assert_eq!(noise_count(&repeated_lines(&pages)), 0);
    }

    #[test]
    fn top_line_on_two_pages_only_is_kept() {
        let pages: Vec<_> = (0..10)
            .map(|i| {
                let head = if i == 2 || i == 6 {
                    "Chapter 3 Configuration"
                } else {
                    ""
                };
                source(line_page(&[(30.0, head), (300.0, "Body")]))
            })
            .collect();
        assert_eq!(noise_count(&repeated_lines(&pages)), 0);
    }

    #[test]
    fn two_page_document_has_no_noise() {
        let pages: Vec<_> = (1..=2)
            .map(|n| source(line_page(&[(30.0, &format!("Document v1.2 — Page {n}"))])))
            .collect();
        assert_eq!(noise_count(&repeated_lines(&pages)), 0);
    }

    #[test]
    fn alternating_headers_are_both_noise() {
        for n in [10usize, 11, 12, 13] {
            let pages: Vec<_> = (0..n)
                .map(|i| {
                    let head = if i % 2 == 0 {
                        "Refdesk Manual"
                    } else {
                        "Chapter 2 — Usage"
                    };
                    source(line_page(&[(30.0, head), (300.0, &format!("Body {i}"))]))
                })
                .collect();
            let noise = repeated_lines(&pages);
            assert_eq!(noise_count(&noise), n, "{n} pages");
        }
    }

    #[test]
    fn header_missing_on_title_and_blank_pages_is_still_noise() {
        // 12 pages; the alternating headers are missing on the title page, the page after it,
        // and the last two pages, so each appears on 4 pages: fewer than half of all pages, but
        // half or more of its own parity.
        let pages: Vec<_> = (0..12)
            .map(|i| {
                let head = match i {
                    0 | 1 | 10 | 11 => "",
                    _ if i % 2 == 0 => "Left header",
                    _ => "Right header",
                };
                source(line_page(&[(30.0, head), (300.0, "Body")]))
            })
            .collect();
        assert_eq!(noise_count(&repeated_lines(&pages)), 8);
    }

    /// A 20-page document with "3 Configuration" in the top band of the pages in `with`.
    fn header_on(with: &[usize]) -> Vec<PageSource> {
        (0..20)
            .map(|i| {
                let head = if with.contains(&i) {
                    "3 Configuration"
                } else {
                    ""
                };
                source(line_page(&[(30.0, head), (300.0, "Body")]))
            })
            .collect()
    }

    #[test]
    fn running_header_of_one_chapter_is_noise() {
        // Every page of a chapter, and every other page of another.
        assert_eq!(
            noise_count(&repeated_lines(&header_on(&[3, 4, 5, 6, 7]))),
            5
        );
        assert_eq!(noise_count(&repeated_lines(&header_on(&[9, 11, 13]))), 3);
    }

    #[test]
    fn scattered_top_line_is_kept() {
        assert_eq!(noise_count(&repeated_lines(&header_on(&[0, 7, 14]))), 0);
        assert_eq!(noise_count(&repeated_lines(&header_on(&[2, 4, 8, 10]))), 0);
    }

    #[test]
    fn heading_in_a_larger_font_is_not_grouped_with_the_running_header() {
        let mut pages = header_on(&[3, 4, 5, 6, 7]);
        // Page 3 carries the chapter heading itself, in 20 pt.
        let mut heading = (*pages[3].text).clone();
        for c in &mut heading.chars {
            c.size = 20.0;
        }
        pages[3].text = std::sync::Arc::new(heading);
        let noise = repeated_lines(&pages);
        assert!(!noise[3][0]);
        assert!((4..8).all(|i| noise[i][0]));
    }

    #[test]
    fn empty_pages_and_lines_out_of_range_do_not_panic() {
        let mut broken = line_page(&[(30.0, "Header")]);
        broken.lines[0].chars = 5..999;
        let pages = vec![
            source(PageText::default()),
            source(broken),
            source(PageText::default()),
        ];
        let noise = repeated_lines(&pages);
        assert_eq!(noise.len(), 3);
        assert_eq!(noise_count(&noise), 0);
        assert!(repeated_lines(&[]).is_empty());
    }
}
