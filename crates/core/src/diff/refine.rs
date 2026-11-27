//! Second pass of a comparison: the words of a misaligned region (`docs/SPEC.md` §8.5 step 5).
//!
//! A region is a run of paragraphs on each side that the paragraph alignment could not pair.
//! Paragraph boundaries depend on how the PDF engine groups lines into blocks, so the same text
//! may be one paragraph on one side and two on the other. The region is therefore compared as
//! one flow of words, and the paragraphs are classified from that:
//!
//! 1. The words of both sides are compared with the histogram algorithm, hyphens left out
//!    ([`compare_key`]). Short runs of equal
//!    words between larger edits ("the", "of" shared by two unrelated paragraphs) are folded
//!    into the edits ([`absorb_short_equalities`]).
//! 2. A left and a right paragraph are *linked* when the words they share are at least
//!    [`MIN_SHARED_PERCENT`] per cent of the shorter of the two.
//! 3. Linked paragraphs form groups (widened to whole paragraph intervals on both sides). A
//!    group whose words all have a partner inside the group is not a change: only the
//!    paragraph boundaries differ. Any other group is a modification, and the words without a
//!    partner inside it are the changed words.
//! 4. Paragraphs outside every group were removed (left) or added (right). Consecutive ones
//!    form one item.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::time::Instant;

use similar::DiffOp;

use super::align::diff_keys;
use super::extract::{DocTokens, Word};

/// Two paragraphs are linked when the words they share are at least this share of the shorter
/// paragraph's words. See the module documentation and the WP-61 report for the reasoning.
pub(crate) const MIN_SHARED_PERCENT: usize = 50;

/// One outcome of a region, in document order. Paragraph ranges index the paragraph lists of
/// the two sides; word indices index their word lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    /// The same words on both sides; at most the paragraph boundaries differ.
    Same {
        left: Range<usize>,
        right: Range<usize>,
    },
    /// Left paragraphs whose words are gone.
    Removed { left: Range<usize> },
    /// Right paragraphs whose words are new.
    Added { right: Range<usize> },
    /// Paragraphs that share enough words to be one changed passage.
    Modified {
        left: Range<usize>,
        right: Range<usize>,
        /// Left words without a partner in the passage, ascending.
        left_words: Vec<usize>,
        /// Right words without a partner in the passage, ascending.
        right_words: Vec<usize>,
    },
}

/// A run of the word script: equal words (`old == new`) or an edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seg {
    pub eq: bool,
    pub old: usize,
    pub new: usize,
}

/// Turns `similar` operations into runs, joining neighbouring edits.
pub(crate) fn segs_from_ops(ops: &[DiffOp]) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    for op in ops {
        let seg = match *op {
            DiffOp::Equal { len, .. } => Seg {
                eq: true,
                old: len,
                new: len,
            },
            DiffOp::Delete { old_len, .. } => Seg {
                eq: false,
                old: old_len,
                new: 0,
            },
            DiffOp::Insert { new_len, .. } => Seg {
                eq: false,
                old: 0,
                new: new_len,
            },
            DiffOp::Replace {
                old_len, new_len, ..
            } => Seg {
                eq: false,
                old: old_len,
                new: new_len,
            },
        };
        push_seg(&mut out, seg);
    }
    out
}

/// Appends `seg`, merging it into the last run when both are equal runs or both are edits.
fn push_seg(out: &mut Vec<Seg>, seg: Seg) {
    if seg.old == 0 && seg.new == 0 {
        return;
    }
    match out.last_mut() {
        Some(last) if last.eq == seg.eq => {
            last.old += seg.old;
            last.new += seg.new;
        }
        _ => out.push(seg),
    }
}

/// Folds an equal run into the edits around it when it is no longer than the larger side of the
/// edit before it and no longer than the larger side of the edit after it (the semantic cleanup
/// of diff-match-patch, counted in words). Repeats until nothing changes. Equal runs at either
/// end, and long equal runs, stay.
pub(crate) fn absorb_short_equalities(segs: Vec<Seg>) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::with_capacity(segs.len());
    for seg in segs {
        push_seg(&mut out, seg);
        // Look at [.., edit, equal, edit] at the top as long as it folds.
        while out.len() >= 3 {
            let n = out.len();
            let (before, mid, after) = (out[n - 3], out[n - 2], out[n - 1]);
            let folds = !before.eq
                && mid.eq
                && !after.eq
                && mid.old <= before.old.max(before.new)
                && mid.old <= after.old.max(after.new);
            if !folds {
                break;
            }
            out.truncate(n - 2);
            if let Some(last) = out.last_mut() {
                last.old += mid.old + after.old;
                last.new += mid.new + after.new;
            }
        }
    }
    out
}

/// The word index range covered by the paragraphs `paras` of `doc`.
pub(crate) fn word_span(doc: &DocTokens, paras: &Range<usize>) -> Range<usize> {
    let first = doc
        .paragraphs
        .get(paras.start)
        .map(|p| p.words.start as usize);
    let last = paras
        .end
        .checked_sub(1)
        .and_then(|i| doc.paragraphs.get(i))
        .map(|p| p.words.end as usize);
    match (first, last) {
        (Some(a), Some(b)) if paras.start < paras.end => {
            let b = b.min(doc.words.len());
            a.min(b)..b
        }
        _ => 0..0,
    }
}

/// For each word of `span`, the index of its paragraph relative to `paras.start`, and the word
/// count of each paragraph.
fn paragraph_of_words(
    doc: &DocTokens,
    paras: &Range<usize>,
    span: &Range<usize>,
) -> (Vec<usize>, Vec<usize>) {
    let mut of = vec![0; span.len()];
    let mut lens = vec![0; paras.len()];
    for (k, p) in doc.paragraphs[paras.clone()].iter().enumerate() {
        let a = (p.words.start as usize).clamp(span.start, span.end);
        let b = (p.words.end as usize).clamp(a, span.end);
        of[a - span.start..b - span.start].fill(k);
        lens[k] = b - a;
    }
    (of, lens)
}

/// Numbers the keys of `words`: equal keys get equal numbers, on both sides of a region.
///
/// Hyphens are left out of the comparison ([`compare_key`]): a compound broken at a line end
/// ("well-" / "known") is joined by the extraction without its hyphen in one version and kept
/// whole ("well-known") in the other.
fn intern<'a>(ids: &mut HashMap<Cow<'a, str>, u32>, words: &'a [Word]) -> Vec<u32> {
    words
        .iter()
        .map(|w| {
            let next = u32::try_from(ids.len()).unwrap_or(u32::MAX);
            *ids.entry(compare_key(&w.key)).or_insert(next)
        })
        .collect()
}

/// `key` without hyphen characters, unless nothing would be left.
fn compare_key(key: &str) -> Cow<'_, str> {
    let is_hyphen = |c: char| matches!(c, '-' | '\u{2010}' | '\u{2011}');
    if !key.contains(is_hyphen) {
        return Cow::Borrowed(key);
    }
    let stripped: String = key.chars().filter(|&c| !is_hyphen(c)).collect();
    if stripped.is_empty() {
        Cow::Borrowed(key)
    } else {
        Cow::Owned(stripped)
    }
}

/// Union-find over `n` nodes.
struct Groups {
    parent: Vec<usize>,
}

impl Groups {
    fn new(n: usize) -> Groups {
        Groups {
            parent: (0..n).collect(),
        }
    }

    fn root(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn join(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.root(a), self.root(b));
        if ra != rb {
            self.parent[ra.max(rb)] = ra.min(rb);
        }
    }
}

/// A group of linked paragraphs, as relative inclusive intervals on both sides.
#[derive(Clone, Copy, Debug)]
struct Group {
    l: (usize, usize),
    r: (usize, usize),
}

/// Compares the region `left_paras` × `right_paras` word by word and classifies its paragraphs.
/// The second value is `true` when the word comparison ran out of time; the region is then
/// reported as removed on the left and added on the right.
pub(crate) fn refine_region(
    left: &DocTokens,
    right: &DocTokens,
    left_paras: Range<usize>,
    right_paras: Range<usize>,
    deadline: Option<Instant>,
) -> (Vec<Item>, bool) {
    let left_paras =
        left_paras.start.min(left.paragraphs.len())..left_paras.end.min(left.paragraphs.len());
    let right_paras =
        right_paras.start.min(right.paragraphs.len())..right_paras.end.min(right.paragraphs.len());
    let whole = |late: bool| {
        let mut items = Vec::new();
        if !left_paras.is_empty() {
            items.push(Item::Removed {
                left: left_paras.clone(),
            });
        }
        if !right_paras.is_empty() {
            items.push(Item::Added {
                right: right_paras.clone(),
            });
        }
        (items, late)
    };
    if left_paras.is_empty() || right_paras.is_empty() {
        return whole(false);
    }

    let lspan = word_span(left, &left_paras);
    let rspan = word_span(right, &right_paras);
    let mut ids: HashMap<Cow<'_, str>, u32> = HashMap::new();
    let lids = intern(&mut ids, &left.words[lspan.clone()]);
    let rids = intern(&mut ids, &right.words[rspan.clone()]);
    let (ops, late) = diff_keys(&lids, &rids, deadline);
    if late {
        return whole(true);
    }
    let segs = absorb_short_equalities(segs_from_ops(&ops));

    // Partners of the words (relative indices).
    let mut lpartner: Vec<Option<usize>> = vec![None; lids.len()];
    let mut rpartner: Vec<Option<usize>> = vec![None; rids.len()];
    let (mut i, mut j) = (0, 0);
    for s in &segs {
        if s.eq {
            for k in 0..s.old {
                if let (Some(a), Some(b)) = (lpartner.get_mut(i + k), rpartner.get_mut(j + k)) {
                    *a = Some(j + k);
                    *b = Some(i + k);
                }
            }
        }
        i += s.old;
        j += s.new;
    }

    let (lpara, llen) = paragraph_of_words(left, &left_paras, &lspan);
    let (rpara, rlen) = paragraph_of_words(right, &right_paras, &rspan);
    let (nl, nr) = (llen.len(), rlen.len());

    // Shared word counts per paragraph pair. The script is monotone, so equal pairs are
    // consecutive.
    let mut shared: Vec<((usize, usize), usize)> = Vec::new();
    for (wi, p) in lpartner.iter().enumerate() {
        let Some(wj) = *p else { continue };
        let key = (lpara[wi], rpara[wj]);
        match shared.last_mut() {
            Some((k, n)) if *k == key => *n += 1,
            _ => shared.push((key, 1)),
        }
    }
    let mut groups = Groups::new(nl + nr);
    let mut linked_l = vec![false; nl];
    let mut linked_r = vec![false; nr];
    for &((a, b), n) in &shared {
        let shorter = llen[a].min(rlen[b]);
        if n * 100 >= shorter * MIN_SHARED_PERCENT {
            groups.join(a, nl + b);
            linked_l[a] = true;
            linked_r[b] = true;
        }
    }

    // Groups as intervals, widened and merged so that they do not overlap on either side.
    let mut by_root: HashMap<usize, Group> = HashMap::new();
    for (a, _) in linked_l.iter().enumerate().filter(|(_, l)| **l) {
        let g = by_root.entry(groups.root(a)).or_insert(Group {
            l: (a, a),
            r: (usize::MAX, 0),
        });
        g.l = (g.l.0.min(a), g.l.1.max(a));
    }
    for (b, _) in linked_r.iter().enumerate().filter(|(_, l)| **l) {
        let root = groups.root(nl + b);
        if let Some(g) = by_root.get_mut(&root) {
            g.r = (g.r.0.min(b), g.r.1.max(b));
        }
    }
    let mut list: Vec<Group> = by_root.into_values().filter(|g| g.r.0 <= g.r.1).collect();
    list.sort_by_key(|g| (g.l.0, g.r.0));
    let mut merged: Vec<Group> = Vec::new();
    for g in list {
        match merged.last_mut() {
            Some(m) if g.l.0 <= m.l.1 || g.r.0 <= m.r.1 => {
                m.l = (m.l.0.min(g.l.0), m.l.1.max(g.l.1));
                m.r = (m.r.0.min(g.r.0), m.r.1.max(g.r.1));
            }
            _ => merged.push(g),
        }
    }

    // Items in document order.
    let lbase = left_paras.start;
    let rbase = right_paras.start;
    let mut items = Vec::new();
    let (mut li, mut ri) = (0, 0);
    let flush_unlinked = |items: &mut Vec<Item>, l: Range<usize>, r: Range<usize>| {
        if !l.is_empty() {
            items.push(Item::Removed {
                left: lbase + l.start..lbase + l.end,
            });
        }
        if !r.is_empty() {
            items.push(Item::Added {
                right: rbase + r.start..rbase + r.end,
            });
        }
    };
    for g in &merged {
        flush_unlinked(&mut items, li..g.l.0.max(li), ri..g.r.0.max(ri));
        let lw: Vec<usize> = (0..lids.len())
            .filter(|&w| (g.l.0..=g.l.1).contains(&lpara[w]))
            .filter(|&w| !lpartner[w].is_some_and(|p| (g.r.0..=g.r.1).contains(&rpara[p])))
            .map(|w| lspan.start + w)
            .collect();
        let rw: Vec<usize> = (0..rids.len())
            .filter(|&w| (g.r.0..=g.r.1).contains(&rpara[w]))
            .filter(|&w| !rpartner[w].is_some_and(|p| (g.l.0..=g.l.1).contains(&lpara[p])))
            .map(|w| rspan.start + w)
            .collect();
        let left = lbase + g.l.0..lbase + g.l.1 + 1;
        let right = rbase + g.r.0..rbase + g.r.1 + 1;
        if lw.is_empty() && rw.is_empty() {
            items.push(Item::Same { left, right });
        } else {
            items.push(Item::Modified {
                left,
                right,
                left_words: lw,
                right_words: rw,
            });
        }
        li = li.max(g.l.1 + 1);
        ri = ri.max(g.r.1 + 1);
    }
    flush_unlinked(&mut items, li..nl, ri..nr);
    (items, false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::Duration;

    use super::*;
    use crate::diff::extract::{ExtractOptions, PageSource, extract};
    use crate::diff::test_pages::{block_page, source};

    fn seg(eq: bool, old: usize, new: usize) -> Seg {
        Seg { eq, old, new }
    }

    /// A one-page document with one block per paragraph (each block one line per string).
    fn doc(paras: &[&[&str]]) -> DocTokens {
        let blocks: Vec<(f32, &[&str])> = paras
            .iter()
            .enumerate()
            .map(|(i, p)| (60.0 + i as f32 * 90.0, *p))
            .collect();
        let pages: Vec<PageSource> = vec![source(block_page(&blocks))];
        extract(
            &pages,
            &ExtractOptions {
                skip_repeated_lines: false,
                ..ExtractOptions::default()
            },
        )
    }

    fn all(d: &DocTokens) -> Range<usize> {
        0..d.paragraphs.len()
    }

    fn keys_of(d: &DocTokens, words: &[usize]) -> Vec<String> {
        words.iter().map(|&w| d.words[w].key.clone()).collect()
    }

    #[test]
    fn short_equalities_between_edits_are_folded() {
        // del 3, eq 1, ins 2 → one edit.
        let s = absorb_short_equalities(vec![seg(false, 3, 0), seg(true, 1, 1), seg(false, 0, 2)]);
        assert_eq!(s, [seg(false, 4, 3)]);
        // A long equality stays; so do the equalities at the ends.
        let s = vec![
            seg(true, 1, 1),
            seg(false, 1, 1),
            seg(true, 5, 5),
            seg(false, 2, 0),
            seg(true, 1, 1),
        ];
        assert_eq!(absorb_short_equalities(s.clone()), s);
        // Folding cascades backwards once an edit has grown.
        let s = absorb_short_equalities(vec![
            seg(false, 3, 3),
            seg(true, 2, 2),
            seg(false, 1, 1),
            seg(true, 1, 1),
            seg(false, 2, 2),
        ]);
        assert_eq!(s, [seg(false, 9, 9)]);
        // An equality longer than the edit on one side stays.
        let s = vec![seg(false, 2, 2), seg(true, 3, 3), seg(false, 1, 1)];
        assert_eq!(absorb_short_equalities(s.clone()), s);
    }

    #[test]
    fn same_words_with_other_paragraph_boundaries_are_not_a_change() {
        let l = doc(&[&["alpha beta gamma delta", "epsilon zeta eta theta."]]);
        let r = doc(&[&["alpha beta gamma delta"], &["epsilon zeta eta theta."]]);
        let (items, late) = refine_region(&l, &r, all(&l), all(&r), None);
        assert!(!late);
        assert_eq!(
            items,
            [Item::Same {
                left: 0..1,
                right: 0..2
            }]
        );
    }

    #[test]
    fn one_word_changed_gives_one_modified_with_that_word() {
        let l = doc(&[&["the parser reads the configuration file first."]]);
        let r = doc(&[&["the parser reads the settings file first."]]);
        let (items, _) = refine_region(&l, &r, all(&l), all(&r), None);
        let [
            Item::Modified {
                left,
                right,
                left_words,
                right_words,
            },
        ] = items.as_slice()
        else {
            panic!("{items:?}");
        };
        assert_eq!((left.clone(), right.clone()), (0..1, 0..1));
        assert_eq!(keys_of(&l, left_words), ["configuration"]);
        assert_eq!(keys_of(&r, right_words), ["settings"]);
    }

    #[test]
    fn unrelated_paragraphs_are_removed_and_added() {
        let l = doc(&[&["the colour of the sky depends on the scattering of light in air."]]);
        let r = doc(&[&["a worker thread owns each document and answers requests of the viewer."]]);
        let (items, _) = refine_region(&l, &r, all(&l), all(&r), None);
        assert_eq!(
            items,
            [Item::Removed { left: 0..1 }, Item::Added { right: 0..1 }]
        );
    }

    #[test]
    fn inserted_paragraph_between_modified_ones_is_added() {
        let l = doc(&[
            &["one two three four five six seven."],
            &["eight nine ten eleven twelve thirteen."],
        ]);
        let r = doc(&[
            &["one two three four FIVE six seven."],
            &["completely new words appear right here now."],
            &["eight nine ten eleven twelve fourteen."],
        ]);
        let (items, _) = refine_region(&l, &r, all(&l), all(&r), None);
        assert_eq!(items.len(), 3, "{items:?}");
        assert!(
            matches!(&items[0], Item::Modified { left, right, .. } if *left == (0..1) && *right == (0..1))
        );
        assert_eq!(items[1], Item::Added { right: 1..2 });
        assert!(
            matches!(&items[2], Item::Modified { left, right, .. } if *left == (1..2) && *right == (2..3))
        );
    }

    #[test]
    fn a_removed_paragraph_inside_a_merged_one_becomes_part_of_the_modification() {
        // Right joins the first and the third paragraph and drops the second.
        let l = doc(&[
            &["alpha beta gamma delta"],
            &["removed words in between here"],
            &["epsilon zeta eta theta."],
        ]);
        let r = doc(&[&["alpha beta gamma delta epsilon zeta eta theta."]]);
        let (items, _) = refine_region(&l, &r, all(&l), all(&r), None);
        let [
            Item::Modified {
                left,
                right,
                left_words,
                right_words,
            },
        ] = items.as_slice()
        else {
            panic!("{items:?}");
        };
        assert_eq!((left.clone(), right.clone()), (0..3, 0..1));
        assert_eq!(
            keys_of(&l, left_words),
            ["removed", "words", "in", "between", "here"]
        );
        assert!(right_words.is_empty());
    }

    #[test]
    fn a_compound_joined_at_a_line_end_matches_the_whole_one() {
        let l = doc(&[&["a well-", "known rule."]]);
        let r = doc(&[&["a well-known rule."]]);
        assert_eq!(l.words[1].key, "wellknown");
        let (items, _) = refine_region(&l, &r, all(&l), all(&r), None);
        assert_eq!(
            items,
            [Item::Same {
                left: 0..1,
                right: 0..1
            }]
        );
        assert_eq!(compare_key("-"), "-");
        assert_eq!(compare_key("a\u{2010}b-c"), "abc");
    }

    #[test]
    fn one_sided_regions() {
        let l = doc(&[&["a b c."], &["d e f."]]);
        let r = doc(&[&["x"]]);
        assert_eq!(
            refine_region(&l, &r, 0..2, 0..0, None).0,
            [Item::Removed { left: 0..2 }]
        );
        assert_eq!(
            refine_region(&l, &r, 1..1, 0..1, None).0,
            [Item::Added { right: 0..1 }]
        );
        assert!(refine_region(&l, &r, 0..0, 0..0, None).0.is_empty());
    }

    #[test]
    fn a_passed_deadline_reports_the_whole_region() {
        let l = doc(&[&["a b c d."]]);
        let r = doc(&[&["a b x d."]]);
        let past = Instant::now().checked_sub(Duration::from_secs(1));
        let (items, late) = refine_region(&l, &r, all(&l), all(&r), past);
        assert!(late);
        assert_eq!(
            items,
            [Item::Removed { left: 0..1 }, Item::Added { right: 0..1 }]
        );
    }
}
