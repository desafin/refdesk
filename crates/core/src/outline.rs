//! The table of contents. See `docs/ARCHITECTURE.md` §4.5.

use serde::{Deserialize, Serialize};

use crate::model::Location;

/// One entry of the outline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutlineItem {
    /// Title as written in the document.
    pub title: String,
    /// Nesting depth. Top level is 0.
    pub depth: u16,
    /// Index of the parent item, `None` at top level. Always smaller than the item's own index.
    pub parent: Option<u32>,
    /// Where the entry points to inside the document, if it does.
    pub target: Option<Location>,
    /// An external URI the entry points to, if it does.
    pub uri: Option<String>,
}

/// An entry in nested form, as an engine reads it from a document. See [`Outline::from_tree`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutlineNode {
    /// Title.
    pub title: String,
    /// Where the entry points to inside the document, if it does.
    pub target: Option<Location>,
    /// An external URI the entry points to, if it does.
    pub uri: Option<String>,
    /// Entries below this one, in order.
    pub children: Vec<OutlineNode>,
}

/// The outline, flattened in pre-order (document order).
///
/// `items` is public for reading. Build an outline with [`Outline::from_tree`] or
/// [`Outline::from_items`] and do not change `items` afterwards: the lookup index used by
/// [`Outline::current`] is built at construction.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "OutlineRepr")]
pub struct Outline {
    /// All items in pre-order.
    pub items: Vec<OutlineItem>,
    /// Indices of the items that have a target, sorted by (page, y, index).
    #[serde(skip)]
    by_target: Vec<u32>,
}

#[derive(Deserialize)]
struct OutlineRepr {
    items: Vec<OutlineItem>,
}

impl From<OutlineRepr> for Outline {
    fn from(repr: OutlineRepr) -> Outline {
        Outline::from_items(repr.items)
    }
}

/// Orders by page, then by y. Negative zero counts as zero.
fn target_cmp(a: &Location, b: &Location) -> std::cmp::Ordering {
    a.page
        .cmp(&b.page)
        .then((a.y + 0.0).total_cmp(&(b.y + 0.0)))
}

impl Outline {
    /// An outline without items.
    pub fn new() -> Outline {
        Outline::default()
    }

    /// Builds an outline from items already in pre-order with valid `depth` and `parent`.
    pub fn from_items(items: Vec<OutlineItem>) -> Outline {
        let mut by_target: Vec<u32> = (0..items.len() as u32)
            .filter(|&i| items[i as usize].target.is_some())
            .collect();
        by_target.sort_by(
            |&a, &b| match (&items[a as usize].target, &items[b as usize].target) {
                (Some(ta), Some(tb)) => target_cmp(ta, tb).then(a.cmp(&b)),
                _ => a.cmp(&b),
            },
        );
        Outline { items, by_target }
    }

    /// Flattens a nested outline in pre-order, filling in `depth` and `parent`.
    pub fn from_tree(roots: Vec<OutlineNode>) -> Outline {
        let mut items = Vec::new();
        // Explicit stack: a damaged document may nest very deeply.
        let mut stack = vec![(roots.into_iter(), None::<u32>, 0u16)];
        while let Some((iter, parent, depth)) = stack.last_mut() {
            let (parent, depth) = (*parent, *depth);
            match iter.next() {
                None => {
                    stack.pop();
                }
                Some(node) => {
                    let idx = items.len() as u32;
                    items.push(OutlineItem {
                        title: node.title,
                        depth,
                        parent,
                        target: node.target,
                        uri: node.uri,
                    });
                    stack.push((
                        node.children.into_iter(),
                        Some(idx),
                        depth.saturating_add(1),
                    ));
                }
            }
        }
        Outline::from_items(items)
    }

    /// The item the reader is in at `at`: among the items that have a target, the last one whose
    /// (page, y) is not after `at`. Items with an equal (page, y) are ordered by document order.
    /// `None` before the first target. The x coordinate is ignored.
    pub fn current(&self, at: Location) -> Option<u32> {
        let n = self.by_target.partition_point(|&i| {
            self.items[i as usize]
                .target
                .is_some_and(|t| target_cmp(&t, &at).is_le())
        });
        n.checked_sub(1).map(|k| self.by_target[k])
    }

    /// The ancestors of `idx`, nearest first. Empty for a top-level or unknown item.
    pub fn ancestors(&self, idx: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = idx;
        while let Some(p) = self.items.get(cur as usize).and_then(|it| it.parent) {
            // A parent always precedes its child; anything else is damaged data.
            if p >= cur {
                break;
            }
            out.push(p);
            cur = p;
        }
        out
    }

    /// Whether the item has at least one child.
    pub fn has_children(&self, idx: u32) -> bool {
        // Pre-order: the first child, if any, directly follows its parent.
        self.items
            .get(idx as usize + 1)
            .is_some_and(|next| next.parent == Some(idx))
    }

    /// The direct children of `idx`, in order.
    pub fn children(&self, idx: u32) -> Vec<u32> {
        let Some(item) = self.items.get(idx as usize) else {
            return Vec::new();
        };
        let start = idx as usize + 1;
        self.items[start.min(self.items.len())..]
            .iter()
            .enumerate()
            // The subtree ends at the first following item that is not deeper.
            .take_while(|(_, it)| it.depth > item.depth)
            .filter(|(_, it)| it.parent == Some(idx))
            .map(|(k, _)| (start + k) as u32)
            .collect()
    }

    /// Titles from the top level down to `idx`, for showing the path to a section (TOC-6).
    /// Empty for an unknown item.
    pub fn path_titles(&self, idx: u32) -> Vec<&str> {
        let Some(item) = self.items.get(idx as usize) else {
            return Vec::new();
        };
        let mut out: Vec<&str> = self
            .ancestors(idx)
            .into_iter()
            .rev()
            .filter_map(|a| self.items.get(a as usize))
            .map(|it| it.title.as_str())
            .collect();
        out.push(item.title.as_str());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(title: &str, target: Option<(u32, f32)>, children: Vec<OutlineNode>) -> OutlineNode {
        OutlineNode {
            title: title.to_string(),
            target: target.map(|(p, y)| Location::new(p, 0.0, y)),
            uri: None,
            children,
        }
    }

    fn at(page: u32, y: f32) -> Location {
        Location::new(page, 0.0, y)
    }

    fn titles(o: &Outline) -> Vec<&str> {
        o.items.iter().map(|i| i.title.as_str()).collect()
    }

    /// 0 Intro            p0 y100
    ///   1 Scope          p0 y300
    ///   2 Goals          p0 y500
    /// 3 Design           p2 y50
    ///   4 Overview       (no target)
    ///     5 Parts        p2 y400
    /// 6 Appendix         p5 y0
    fn sample() -> Outline {
        Outline::from_tree(vec![
            node(
                "Intro",
                Some((0, 100.0)),
                vec![
                    node("Scope", Some((0, 300.0)), vec![]),
                    node("Goals", Some((0, 500.0)), vec![]),
                ],
            ),
            node(
                "Design",
                Some((2, 50.0)),
                vec![node(
                    "Overview",
                    None,
                    vec![node("Parts", Some((2, 400.0)), vec![])],
                )],
            ),
            node("Appendix", Some((5, 0.0)), vec![]),
        ])
    }

    #[test]
    fn from_tree_flattens_in_preorder_with_depth_and_parent() {
        let o = sample();
        assert_eq!(
            titles(&o),
            [
                "Intro", "Scope", "Goals", "Design", "Overview", "Parts", "Appendix"
            ]
        );
        let depths: Vec<u16> = o.items.iter().map(|i| i.depth).collect();
        assert_eq!(depths, [0, 1, 1, 0, 1, 2, 0]);
        let parents: Vec<Option<u32>> = o.items.iter().map(|i| i.parent).collect();
        assert_eq!(
            parents,
            [None, Some(0), Some(0), None, Some(3), Some(4), None]
        );
    }

    #[test]
    fn current_with_several_items_on_one_page() {
        let o = sample();
        assert_eq!(o.current(at(0, 100.0)), Some(0));
        assert_eq!(o.current(at(0, 299.9)), Some(0));
        assert_eq!(o.current(at(0, 300.0)), Some(1));
        assert_eq!(o.current(at(0, 450.0)), Some(1));
        assert_eq!(o.current(at(0, 700.0)), Some(2));
        // Later pages stay in the last section until the next target starts.
        assert_eq!(o.current(at(1, 10.0)), Some(2));
        assert_eq!(o.current(at(2, 49.0)), Some(2));
        assert_eq!(o.current(at(2, 50.0)), Some(3));
        assert_eq!(o.current(at(5, 0.0)), Some(6));
        assert_eq!(o.current(at(99, 0.0)), Some(6));
    }

    #[test]
    fn current_before_first_item_is_none() {
        let o = sample();
        assert_eq!(o.current(at(0, 99.0)), None);
        assert_eq!(Outline::new().current(at(0, 0.0)), None);
    }

    #[test]
    fn current_ignores_items_without_target() {
        let o = sample();
        // "Overview" (index 4) has no target and is never current.
        for y in [50.0, 100.0, 399.0] {
            assert_ne!(o.current(at(2, y)), Some(4));
        }
        assert_eq!(o.current(at(2, 399.0)), Some(3));
        assert_eq!(o.current(at(2, 400.0)), Some(5));
        // An outline where nothing has a target.
        let none = Outline::from_tree(vec![node("a", None, vec![]), node("b", None, vec![])]);
        assert_eq!(none.current(at(3, 3.0)), None);
    }

    #[test]
    fn current_with_targets_out_of_document_order() {
        // Document order: A(p4) B(p1) C(p3) D(p1, lower on the page than B) E(p0)
        let o = Outline::from_tree(vec![
            node("A", Some((4, 0.0)), vec![]),
            node("B", Some((1, 100.0)), vec![]),
            node("C", Some((3, 0.0)), vec![]),
            node("D", Some((1, 200.0)), vec![]),
            node("E", Some((0, 0.0)), vec![]),
        ]);
        let name = |l: Location| o.current(l).map(|i| o.items[i as usize].title.as_str());
        assert_eq!(name(at(0, 0.0)), Some("E"));
        assert_eq!(name(at(1, 150.0)), Some("B"));
        assert_eq!(name(at(1, 200.0)), Some("D"));
        assert_eq!(name(at(2, 0.0)), Some("D"));
        assert_eq!(name(at(3, 0.0)), Some("C"));
        assert_eq!(name(at(4, 0.0)), Some("A"));
        assert_eq!(name(at(9, 0.0)), Some("A"));
    }

    #[test]
    fn current_prefers_later_item_at_equal_position() {
        let o = Outline::from_tree(vec![
            node("first", Some((1, 10.0)), vec![]),
            node("second", Some((1, 10.0)), vec![]),
        ]);
        assert_eq!(o.current(at(1, 10.0)), Some(1));
        assert_eq!(o.current(at(1, 9.0)), None);
    }

    #[test]
    fn ancestors_nearest_first() {
        let o = sample();
        assert_eq!(o.ancestors(5), vec![4, 3]);
        assert_eq!(o.ancestors(1), vec![0]);
        assert_eq!(o.ancestors(0), Vec::<u32>::new());
        assert_eq!(o.ancestors(99), Vec::<u32>::new());
    }

    #[test]
    fn children_and_has_children() {
        let o = sample();
        assert_eq!(o.children(0), vec![1, 2]);
        assert_eq!(o.children(3), vec![4]);
        assert_eq!(o.children(4), vec![5]);
        assert_eq!(o.children(5), Vec::<u32>::new());
        assert_eq!(o.children(6), Vec::<u32>::new());
        assert_eq!(o.children(99), Vec::<u32>::new());
        assert!(o.has_children(0));
        assert!(o.has_children(3));
        assert!(o.has_children(4));
        assert!(!o.has_children(1));
        assert!(!o.has_children(2));
        assert!(!o.has_children(5));
        assert!(!o.has_children(6));
        assert!(!o.has_children(99));
    }

    #[test]
    fn path_titles_from_top_level_down() {
        let o = sample();
        assert_eq!(o.path_titles(5), vec!["Design", "Overview", "Parts"]);
        assert_eq!(o.path_titles(1), vec!["Intro", "Scope"]);
        assert_eq!(o.path_titles(6), vec!["Appendix"]);
        assert!(o.path_titles(99).is_empty());
    }

    #[test]
    fn from_tree_survives_very_deep_nesting() {
        let mut n = node("leaf", None, vec![]);
        for _ in 0..20_000 {
            n = node("x", None, vec![n]);
        }
        let o = Outline::from_tree(vec![n]);
        assert_eq!(o.items.len(), 20_001);
        assert_eq!(o.items[20_000].depth, 20_000);
        assert_eq!(o.items[20_000].parent, Some(19_999));
    }

    #[test]
    fn serde_roundtrip_rebuilds_lookup_index() {
        let o = sample();
        let s = serde_json::to_string(&o).unwrap();
        let back: Outline = serde_json::from_str(&s).unwrap();
        assert_eq!(back, o);
        assert_eq!(back.current(at(0, 450.0)), Some(1));
    }
}
