//! Narrowing a search to a part of the document (SPEC FIND-16, SEL-5).
//!
//! A [`SearchScope`] is a pair of places: where the part starts (included) and where it ends
//! (not included). The parts people pick are the sections of the table of contents, so
//! [`section_scope`] turns an outline item into a scope and [`scope_choices`] lists the scopes
//! that contain a given place: the whole document, then each step of the section path from the
//! top-level chapter down to the deepest section.
//!
//! A match belongs to the scope when it *starts* in it. Where a match starts is judged by the
//! bottom edge of the first rectangle of the match: the heading of a section is then inside the
//! section whether the table of contents points at the top of the heading or at its baseline,
//! and the heading of the next section is outside.

use std::ops::Range;

use super::SearchMatch;
use crate::model::Location;
use crate::outline::Outline;

/// A part of a document: from `start` (included) to `end` (not included), in page space.
///
/// `end` may lie past the last page (`page == page_count`, `y == 0`): the rest of the document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchScope {
    /// Where the part starts. The x coordinate is not used.
    pub start: Location,
    /// Where the part ends. The x coordinate is not used.
    pub end: Location,
}

/// Orders places by page, then by height. Negative zero counts as zero.
fn cmp_place(page: u32, y: f32, to: &Location) -> std::cmp::Ordering {
    page.cmp(&to.page).then((y + 0.0).total_cmp(&(to.y + 0.0)))
}

impl SearchScope {
    /// The pages that can hold something of the scope, clamped to a document of `page_count`
    /// pages. A scope that ends at the very top of a page does not include that page. Empty when
    /// the scope starts past the end of the document or ends before it starts.
    pub fn pages(&self, page_count: u32) -> Range<u32> {
        let first = self.start.page.min(page_count);
        let past_last = self.end.page.saturating_add(u32::from(self.end.y > 0.0));
        let past_last = past_last.min(page_count);
        if first >= past_last {
            return 0..0;
        }
        first..past_last
    }

    /// Whether a thing on `page` whose bottom edge is at height `y` starts inside the scope.
    pub fn contains(&self, page: u32, y: f32) -> bool {
        cmp_place(page, y, &self.start).is_ge() && cmp_place(page, y, &self.end).is_lt()
    }

    /// Whether the match starts inside the scope.
    pub fn contains_match(&self, m: &SearchMatch) -> bool {
        let y = m.rects.first().map_or(0.0, |r| r.y1);
        self.contains(m.page, y)
    }
}

/// One thing the user can pick as the scope of a search.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeChoice {
    /// The index of the outline item the scope is the section of; `None` for the whole document.
    pub item: Option<u32>,
    /// The title of the section (trimmed). Empty for the whole document.
    pub title: String,
    /// The titles from the top level down to this section. Empty for the whole document.
    pub path: Vec<String>,
    /// The scope; `None` for the whole document.
    pub scope: Option<SearchScope>,
}

impl ScopeChoice {
    /// The whole document.
    pub fn whole_document() -> ScopeChoice {
        ScopeChoice {
            item: None,
            title: String::new(),
            path: Vec::new(),
            scope: None,
        }
    }

    /// The section `idx` of `outline` in a document of `page_count` pages. `None` when the item
    /// does not lead anywhere in the document.
    pub fn section(outline: &Outline, idx: u32, page_count: u32) -> Option<ScopeChoice> {
        let scope = section_scope(outline, idx, page_count)?;
        let item = outline.items.get(idx as usize)?;
        Some(ScopeChoice {
            item: Some(idx),
            title: item.title.trim().to_owned(),
            path: outline
                .path_titles(idx)
                .into_iter()
                .map(|t| t.trim().to_owned())
                .collect(),
            scope: Some(scope),
        })
    }
}

/// The place where the document ends, as the end of a [`SearchScope`].
pub fn document_end(page_count: u32) -> Location {
    Location::new(page_count, 0.0, 0.0)
}

/// The scope of the section `idx`: from the place its entry points to up to the start of the next
/// entry that is as deep or less deep (the end of the document when there is none). Entries that
/// lead nowhere in the document, or that lie before the start (a table of contents that is out of
/// order), are passed over when looking for the end. `None` when the item has no place.
pub fn section_scope(outline: &Outline, idx: u32, page_count: u32) -> Option<SearchScope> {
    let item = outline.items.get(idx as usize)?;
    let start = item.target?;
    let end = outline
        .items
        .iter()
        .skip(idx as usize + 1)
        .filter(|next| next.depth <= item.depth)
        .filter_map(|next| next.target)
        .find(|t| cmp_place(t.page, t.y, &start).is_gt())
        .unwrap_or_else(|| document_end(page_count));
    Some(SearchScope { start, end })
}

/// What can be picked as the scope at `at`: the whole document first, then the section `at` is in
/// and each section above it, the top-level chapter first and the deepest section last. Sections
/// that lead nowhere are left out. Without a table of contents, or before its first entry, only
/// the whole document is there.
pub fn scope_choices(outline: &Outline, at: Location, page_count: u32) -> Vec<ScopeChoice> {
    let mut out = vec![ScopeChoice::whole_document()];
    let Some(current) = outline.current(at) else {
        return out;
    };
    let mut chain = outline.ancestors(current);
    chain.reverse();
    chain.push(current);
    out.extend(
        chain
            .into_iter()
            .filter_map(|idx| ScopeChoice::section(outline, idx, page_count)),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use crate::outline::OutlineNode;

    fn node(title: &str, page: u32, y: f32, children: Vec<OutlineNode>) -> OutlineNode {
        OutlineNode {
            title: title.to_string(),
            target: Some(Location::new(page, 0.0, y)),
            uri: None,
            children,
        }
    }

    fn at(page: u32, y: f32) -> Location {
        Location::new(page, 0.0, y)
    }

    /// Preface, then 7 (7.3 (7.3.2), 7.4), then 8.
    fn outline() -> Outline {
        Outline::from_tree(vec![
            node("Preface", 0, 10.0, vec![]),
            node(
                "7 Functional specification",
                1,
                40.0,
                vec![
                    node(
                        "7.3 State management",
                        1,
                        200.0,
                        vec![node("7.3.2 Transitions", 2, 100.0, vec![])],
                    ),
                    node("7.4 Errors", 3, 20.0, vec![]),
                ],
            ),
            node("8 Annex", 4, 50.0, vec![]),
        ])
    }

    fn m(page: u32, y0: f32, y1: f32) -> SearchMatch {
        SearchMatch {
            page,
            chars: 0..1,
            rects: vec![Rect {
                x0: 0.0,
                y0,
                x1: 10.0,
                y1,
            }],
            cont: None,
            before: String::new(),
            matched: String::new(),
            after: String::new(),
        }
    }

    #[test]
    fn a_scope_holds_what_starts_between_its_places() {
        let scope = SearchScope {
            start: at(2, 100.0),
            end: at(4, 50.0),
        };
        assert!(!scope.contains(2, 99.0));
        assert!(scope.contains(2, 100.0), "the start is included");
        assert!(scope.contains(3, 0.0));
        assert!(scope.contains(4, 49.9));
        assert!(!scope.contains(4, 50.0), "the end is not included");
        assert!(!scope.contains(5, 0.0));
        assert!(!scope.contains(1, 800.0));
    }

    #[test]
    fn a_match_is_placed_by_the_bottom_of_its_first_line() {
        // A heading whose entry points at its baseline: the line reaches above and below it.
        let scope = SearchScope {
            start: at(1, 100.0),
            end: at(1, 300.0),
        };
        assert!(
            scope.contains_match(&m(1, 91.0, 103.0)),
            "the heading itself"
        );
        assert!(
            !scope.contains_match(&m(1, 291.0, 303.0)),
            "the next heading"
        );
        assert!(!scope.contains_match(&m(1, 70.0, 82.0)), "a line above");
        let none = SearchMatch {
            rects: Vec::new(),
            ..m(1, 0.0, 0.0)
        };
        assert!(
            !scope.contains_match(&none),
            "no rectangle counts as the page top"
        );
    }

    #[test]
    fn the_pages_of_a_scope_are_clamped_and_stop_before_a_page_it_ends_at_the_top_of() {
        let s = |a: Location, b: Location| SearchScope { start: a, end: b };
        assert_eq!(s(at(2, 5.0), at(4, 50.0)).pages(10), 2..5);
        assert_eq!(s(at(2, 5.0), at(4, 0.0)).pages(10), 2..4);
        assert_eq!(s(at(2, 5.0), document_end(10)).pages(10), 2..10);
        assert_eq!(s(at(2, 5.0), at(40, 5.0)).pages(10), 2..10, "clamped");
        assert_eq!(
            s(at(12, 0.0), document_end(10)).pages(10),
            0..0,
            "past the end"
        );
        assert_eq!(
            s(at(3, 5.0), at(3, 0.0)).pages(10),
            0..0,
            "ends before it starts"
        );
        assert_eq!(
            s(at(3, 5.0), at(3, 200.0)).pages(10),
            3..4,
            "inside one page"
        );
    }

    #[test]
    fn a_section_ends_where_the_next_one_of_its_depth_or_above_starts() {
        let o = outline();
        let end_of = |title: &str| {
            let idx = o.items.iter().position(|i| i.title == title).expect("item") as u32;
            section_scope(&o, idx, 9).expect("scope").end
        };
        assert_eq!(
            end_of("7.3.2 Transitions"),
            at(3, 20.0),
            "the next section above it"
        );
        assert_eq!(
            end_of("7.3 State management"),
            at(3, 20.0),
            "its next sibling"
        );
        assert_eq!(
            end_of("7.4 Errors"),
            at(4, 50.0),
            "the next sibling of its parent"
        );
        assert_eq!(end_of("7 Functional specification"), at(4, 50.0));
        assert_eq!(end_of("Preface"), at(1, 40.0));
        assert_eq!(
            end_of("8 Annex"),
            document_end(9),
            "the last chapter ends with the document"
        );
        let start = section_scope(&o, 1, 9).expect("scope").start;
        assert_eq!((start.page, start.y), (1, 40.0));
    }

    #[test]
    fn items_without_a_place_or_out_of_order_are_passed_over_for_the_end() {
        let mut tree = vec![
            node("A", 1, 100.0, vec![]),
            node("B (no place)", 0, 0.0, vec![]),
            node("C (before A)", 0, 50.0, vec![]),
            node("D", 5, 10.0, vec![]),
        ];
        tree[1].target = None;
        let o = Outline::from_tree(tree);
        assert_eq!(section_scope(&o, 0, 9).expect("scope").end, at(5, 10.0));
        assert_eq!(section_scope(&o, 1, 9), None, "B leads nowhere");
        assert_eq!(section_scope(&o, 99, 9), None, "no such item");
    }

    #[test]
    fn the_choices_are_the_document_and_each_step_of_the_path() {
        let o = outline();
        let choices = scope_choices(&o, at(2, 300.0), 9);
        let titles: Vec<&str> = choices.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "",
                "7 Functional specification",
                "7.3 State management",
                "7.3.2 Transitions"
            ]
        );
        assert_eq!(choices[0], ScopeChoice::whole_document());
        assert_eq!(choices[1].scope.expect("scope").end, at(4, 50.0));
        assert_eq!(choices[2].scope.expect("scope").end, at(3, 20.0));
        assert_eq!(choices[3].scope.expect("scope").end, at(3, 20.0));
        assert_eq!(
            choices[3].path,
            [
                "7 Functional specification",
                "7.3 State management",
                "7.3.2 Transitions"
            ]
        );
        assert_eq!(choices[3].item, Some(3));

        // Every choice contains the place it was made at.
        for c in &choices[1..] {
            let s = c.scope.expect("scope");
            assert!(s.contains(2, 300.0), "{}", c.title);
        }
    }

    #[test]
    fn the_last_chapter_goes_to_the_end_and_a_top_level_place_has_one_step() {
        let o = outline();
        let choices = scope_choices(&o, at(7, 0.0), 9);
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[1].title, "8 Annex");
        assert_eq!(choices[1].scope.expect("scope").end, document_end(9));
    }

    #[test]
    fn without_an_outline_or_before_its_first_entry_only_the_whole_document_is_offered() {
        assert_eq!(
            scope_choices(&Outline::new(), at(3, 0.0), 9),
            vec![ScopeChoice::whole_document()]
        );
        // The first entry is on page 0 at 10 pt.
        assert_eq!(
            scope_choices(&outline(), at(0, 5.0), 9),
            vec![ScopeChoice::whole_document()]
        );
        assert_eq!(scope_choices(&outline(), at(0, 10.0), 9).len(), 2);
    }

    #[test]
    fn a_section_without_a_place_is_left_out_of_the_choices() {
        let mut tree = vec![node(
            "Chapter",
            1,
            10.0,
            vec![node(
                "Section",
                1,
                100.0,
                vec![node("Sub", 2, 10.0, vec![])],
            )],
        )];
        tree[0].children[0].target = None;
        let o = Outline::from_tree(tree);
        let titles: Vec<String> = scope_choices(&o, at(2, 50.0), 9)
            .into_iter()
            .map(|c| c.title)
            .collect();
        assert_eq!(titles, ["", "Chapter", "Sub"]);
    }
}
