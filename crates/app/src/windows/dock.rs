//! Everything that touches `egui_dock` (rule R-3). Other modules use [`DockLayout`] and
//! [`DockHost`] only.
//!
//! Terms: a *pane* is one group of tabs shown one at a time (an `egui_dock` leaf). A layout is a
//! tree of panes split left/right or top/bottom.
//!
//! Only the main surface of the `egui_dock` state is used. Its own floating windows are switched
//! off; windows are real OS windows, each with its own `DockLayout`.

use std::collections::HashSet;

use egui::{Id, Rect, Ui, WidgetText};
use egui_dock::tab_viewer::OnCloseResponse;
use egui_dock::{
    AccessibilityTranslations, DockArea, DockState, LeafTranslations, Node, NodeIndex, NodePath,
    Split, Style, SurfaceIndex, TabContextMenuTranslations, TabIndex, TabViewer, Translations,
    Tree,
};
use refdesk_core::store::LayoutNode;

use crate::i18n::tr;
use crate::state::TabId;

/// Smallest and largest share of a split that is saved (`LayoutNode::Split::fraction`).
const MIN_FRACTION: f32 = 0.05;
const MAX_FRACTION: f32 = 0.95;

/// Where a new pane goes relative to the pane it splits from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    /// To the left. Not offered by a command yet; dragging a tab does it.
    #[allow(dead_code)]
    Left,
    /// To the right.
    Right,
    /// Above. Not offered by a command yet; dragging a tab does it.
    #[allow(dead_code)]
    Up,
    /// Below.
    Down,
}

impl SplitDir {
    fn to_dock(self) -> Split {
        match self {
            SplitDir::Left => Split::Left,
            SplitDir::Right => Split::Right,
            SplitDir::Up => Split::Above,
            SplitDir::Down => Split::Below,
        }
    }
}

/// What the dock area needs from the application while it draws. Implemented by `windows`.
///
/// The host never changes the layout itself: closing and focusing come back as calls here, and
/// the host turns them into actions for `dispatch` (rule R-6).
pub trait DockHost {
    /// The text on the tab (italic for a preview tab).
    fn title(&mut self, tab: TabId) -> WidgetText;
    /// The text shown while the pointer rests on the tab, if any.
    fn tooltip(&mut self, tab: TabId) -> Option<String>;
    /// Draws the content of the tab.
    fn ui(&mut self, ui: &mut Ui, tab: TabId);
    /// Fills the menu shown on a right click on the tab.
    fn context_menu(&mut self, ui: &mut Ui, tab: TabId);
    /// Whether the tab shows a close button and closes on a middle click.
    fn is_closeable(&self, tab: TabId) -> bool;
    /// The user asked to close the tab (close button or middle click). The tab stays until the
    /// host's `dispatch` removes it.
    fn close_requested(&mut self, tab: TabId);
    /// The user made the tab the focused one (clicked its title or its pane).
    fn activated(&mut self, tab: TabId);
    /// The user double-clicked the title of the tab (which keeps a preview tab open, WS-3).
    fn title_double_clicked(&mut self, tab: TabId);
}

/// The tab layout of one window: panes, the splits between them, which tab each pane shows and
/// which pane has the focus.
#[derive(Clone, Debug)]
pub struct DockLayout {
    state: DockState<TabId>,
}

impl Default for DockLayout {
    fn default() -> Self {
        DockLayout::new()
    }
}

impl DockLayout {
    /// A layout without tabs.
    pub fn new() -> DockLayout {
        DockLayout::with_tree(Tree::default())
    }

    fn with_tree(tree: Tree<TabId>) -> DockLayout {
        let mut state = DockState::new(Vec::new()).with_translations(translations(tr));
        *state.main_surface_mut() = tree;
        DockLayout { state }
    }

    /// True when there is no tab.
    pub fn is_empty(&self) -> bool {
        self.tab_count() == 0
    }

    /// Number of tabs in all panes.
    pub fn tab_count(&self) -> usize {
        self.tree().num_tabs()
    }

    /// Number of panes.
    pub fn pane_count(&self) -> usize {
        self.leaves().len()
    }

    /// All tabs in document order: panes left to right and top to bottom (first area of a split
    /// before the second), tabs in tab-bar order.
    pub fn tabs(&self) -> Vec<TabId> {
        self.leaves()
            .into_iter()
            .filter_map(|n| self.tree().leaf(n).ok())
            .flat_map(|leaf| leaf.tabs.iter().copied())
            .collect()
    }

    /// Whether the tab is in this layout.
    pub fn contains(&self, tab: TabId) -> bool {
        self.find(tab).is_some()
    }

    /// The tabs of the pane that holds `tab`, in tab-bar order. Empty when `tab` is not here.
    pub fn pane_tabs(&self, tab: TabId) -> Vec<TabId> {
        self.find(tab)
            .and_then(|(node, _)| self.tree().leaf(node).ok())
            .map(|leaf| leaf.tabs.clone())
            .unwrap_or_default()
    }

    /// Adds a tab and makes it the active tab of its pane and the focus. It goes right after
    /// `near` when `near` is here, else at the end of the focused pane, else into a new pane.
    /// Nothing happens when the tab is already here.
    pub fn add_tab(&mut self, tab: TabId, near: Option<TabId>) {
        if self.contains(tab) {
            return;
        }
        let near = near.and_then(|t| self.find(t));
        let node = match near.or_else(|| self.focused_leaf().map(|n| (n, usize::MAX))) {
            Some((node, index)) => {
                let Ok(leaf) = self.tree_mut().leaf_mut(node) else {
                    return;
                };
                if index == usize::MAX {
                    leaf.append_tab(tab);
                } else {
                    leaf.insert_tab(index + 1, tab);
                }
                node
            }
            None => {
                *self.tree_mut() = Tree::new(vec![tab]);
                NodeIndex::root()
            }
        };
        self.set_focus(node);
    }

    /// Removes a tab. When it was the active tab of its pane, the tab to its right becomes active
    /// (the one to its left when it was last). A pane left empty disappears; when it had the
    /// focus, a neighboring pane gets it. Returns false when the tab is not here.
    pub fn remove_tab(&mut self, tab: TabId) -> bool {
        let Some((node, index)) = self.find(tab) else {
            return false;
        };
        let now_empty = {
            let Ok(leaf) = self.tree_mut().leaf_mut(node) else {
                return false;
            };
            leaf.tabs.remove(index);
            let active = leaf.active.0;
            if index < active {
                leaf.active = TabIndex(active - 1);
            } else if index == active {
                leaf.active = TabIndex(index.min(leaf.tabs.len().saturating_sub(1)));
            }
            leaf.tabs.is_empty()
        };
        if now_empty {
            if node == NodeIndex::root() {
                // `Tree::remove_leaf` on the root clears the nodes but keeps a stale focus.
                *self.tree_mut() = Tree::default();
            } else {
                self.tree_mut().remove_leaf(node);
            }
        }
        true
    }

    /// Makes `tab` the active tab of its pane and gives that pane the focus. Returns false when
    /// the tab is not here.
    pub fn activate(&mut self, tab: TabId) -> bool {
        let Some((node, index)) = self.find(tab) else {
            return false;
        };
        if let Ok(leaf) = self.tree_mut().leaf_mut(node) {
            leaf.active = TabIndex(index);
        }
        self.set_focus(node);
        true
    }

    /// The active tab of the focused pane.
    pub fn focused_tab(&self) -> Option<TabId> {
        self.focused_leaf().and_then(|n| self.active_of(n))
    }

    /// The tab after (or before) the active one in the focused pane, wrapping around. Changes
    /// nothing; activate the result to switch.
    pub fn cycle(&self, forward: bool) -> Option<TabId> {
        let leaf = self.tree().leaf(self.focused_leaf()?).ok()?;
        let len = leaf.tabs.len();
        if len == 0 {
            return None;
        }
        let current = leaf.active.0.min(len - 1);
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        leaf.tabs.get(next).copied()
    }

    /// Moves the focus to the next pane in document order (wrapping around) and returns its
    /// active tab.
    pub fn focus_next_pane(&mut self) -> Option<TabId> {
        let leaves = self.leaves();
        let current = self.focused_leaf()?;
        let position = leaves.iter().position(|n| *n == current).unwrap_or(0);
        let next = *leaves.get((position + 1) % leaves.len().max(1))?;
        self.set_focus(next);
        self.active_of(next)
    }

    /// Splits the pane holding `tab` and puts `new_tab` alone in the new pane on side `dir`. The
    /// new pane gets the focus. Returns false (and changes nothing) when `tab` is not here or
    /// `new_tab` already is.
    pub fn split(&mut self, tab: TabId, new_tab: TabId, dir: SplitDir) -> bool {
        if self.contains(new_tab) {
            return false;
        }
        let Some((node, _)) = self.find(tab) else {
            return false;
        };
        let path = NodePath::new(SurfaceIndex::main(), node);
        let [_, new] = self
            .state
            .split(path, dir.to_dock(), 0.5, Node::leaf(new_tab));
        self.set_focus(new);
        true
    }

    /// Each pane's screen rectangle (tab bar and content) and its tabs, as of the last frame
    /// that drew this layout. Panes that were never drawn have `Rect::NOTHING`.
    #[allow(dead_code)] // read by window transfer (WP-21)
    pub fn pane_rects(&self) -> Vec<(Rect, Vec<TabId>)> {
        self.leaves()
            .into_iter()
            .filter_map(|n| self.tree().leaf(n).ok())
            .map(|leaf| (leaf.rect, leaf.tabs.clone()))
            .collect()
    }

    /// The layout in the session format. `key_of` names each tab.
    #[allow(dead_code)] // used by session saving (`session.rs`)
    pub fn to_layout(&self, key_of: impl Fn(TabId) -> String) -> LayoutNode {
        node_to_layout(self.tree(), NodeIndex::root(), &key_of).unwrap_or_default()
    }

    /// Builds a layout from the session format. `tab_of` finds the tab for a key; unknown keys
    /// (and keys seen before) are skipped, and panes left without tabs are folded away. The
    /// first pane gets the focus.
    #[allow(dead_code)] // used by session saving (`session.rs`)
    pub fn from_layout(node: &LayoutNode, tab_of: impl Fn(&str) -> Option<TabId>) -> DockLayout {
        let mut seen = HashSet::new();
        let Some(pruned) = prune(node, &tab_of, &mut seen) else {
            return DockLayout::new();
        };
        let mut tree = Tree::new(pruned.first_tabs().to_vec());
        build(&mut tree, NodeIndex::root(), pruned);
        let mut layout = DockLayout::with_tree(tree);
        if let Some(first) = layout.leaves().first().copied() {
            layout.set_focus(first);
        }
        layout
    }

    /// Draws the panes with their tab bars. `id` must differ between windows.
    ///
    /// Dragging a tab to reorder it, or onto the edge of a pane to split, is left to
    /// `egui_dock`, which changes the layout right here. That is an exception to rule R-6: it
    /// rearranges tabs inside this layout but never adds or removes one. Closing a tab only
    /// reports to the host.
    pub fn show(&mut self, ui: &mut Ui, id: Id, host: &mut impl DockHost) {
        let before = self.focused_tab();
        let mut viewer = Viewer { host };
        DockArea::new(&mut self.state)
            .id(id)
            .style(Style::from_egui(ui.style().as_ref()))
            // The tab menu is drawn by the host (see `Viewer::on_tab_button`).
            .tab_context_menus(false)
            .show_leaf_close_all_buttons(false)
            .show_leaf_collapse_buttons(false)
            .show_inside(ui, &mut viewer);
        let after = self.focused_tab();
        if after != before
            && let Some(tab) = after
        {
            viewer.host.activated(tab);
        }
    }

    // --- internals -------------------------------------------------------------------------

    fn tree(&self) -> &Tree<TabId> {
        self.state.main_surface()
    }

    fn tree_mut(&mut self) -> &mut Tree<TabId> {
        self.state.main_surface_mut()
    }

    /// The pane and position of a tab.
    fn find(&self, tab: TabId) -> Option<(NodeIndex, usize)> {
        self.tree()
            .find_tab(&tab)
            .map(|(node, index)| (node, index.0))
    }

    /// Panes with at least one tab, in document order.
    fn leaves(&self) -> Vec<NodeIndex> {
        let mut out = Vec::new();
        collect_leaves(self.tree(), NodeIndex::root(), &mut out);
        out
    }

    /// The focused pane; the first pane when the dock state has no valid focus.
    fn focused_leaf(&self) -> Option<NodeIndex> {
        self.state
            .focused_leaf()
            .filter(|path| path.surface.is_main())
            .map(|path| path.node)
            .filter(|node| {
                self.tree()
                    .leaf(*node)
                    .is_ok_and(|leaf| !leaf.tabs.is_empty())
            })
            .or_else(|| self.leaves().first().copied())
    }

    fn set_focus(&mut self, node: NodeIndex) {
        self.state
            .set_focused_node_and_surface(NodePath::new(SurfaceIndex::main(), node));
    }

    fn active_of(&self, node: NodeIndex) -> Option<TabId> {
        let leaf = self.tree().leaf(node).ok()?;
        let len = leaf.tabs.len();
        leaf.tabs
            .get(leaf.active.0.min(len.saturating_sub(1)))
            .copied()
    }
}

/// The node at `index`, if the tree is that large.
fn node_at<T>(tree: &Tree<T>, index: NodeIndex) -> Option<&Node<T>> {
    (index.0 < tree.len()).then(|| &tree[index])
}

fn collect_leaves(tree: &Tree<TabId>, index: NodeIndex, out: &mut Vec<NodeIndex>) {
    match node_at(tree, index) {
        Some(Node::Leaf(leaf)) if !leaf.tabs.is_empty() => out.push(index),
        Some(Node::Horizontal(_) | Node::Vertical(_)) => {
            collect_leaves(tree, index.left(), out);
            collect_leaves(tree, index.right(), out);
        }
        _ => {}
    }
}

fn clamp_fraction(fraction: f32) -> f32 {
    if fraction.is_finite() {
        fraction.clamp(MIN_FRACTION, MAX_FRACTION)
    } else {
        0.5
    }
}

fn node_to_layout(
    tree: &Tree<TabId>,
    index: NodeIndex,
    key_of: &impl Fn(TabId) -> String,
) -> Option<LayoutNode> {
    let (vertical, split) = match node_at(tree, index)? {
        Node::Leaf(leaf) if !leaf.tabs.is_empty() => {
            return Some(LayoutNode::Tabs {
                tabs: leaf.tabs.iter().map(|t| key_of(*t)).collect(),
                active: leaf.active.0.min(leaf.tabs.len() - 1),
            });
        }
        // `egui_dock`: Horizontal places its children left and right, Vertical top and bottom.
        Node::Horizontal(split) => (false, split),
        Node::Vertical(split) => (true, split),
        _ => return None,
    };
    let first = node_to_layout(tree, index.left(), key_of);
    let second = node_to_layout(tree, index.right(), key_of);
    match (first, second) {
        (Some(first), Some(second)) => Some(LayoutNode::Split {
            vertical,
            fraction: clamp_fraction(split.fraction),
            first: Box::new(first),
            second: Box::new(second),
        }),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// A `LayoutNode` with unknown tabs removed and empty panes folded away.
enum Pruned {
    Tabs(Vec<TabId>, usize),
    Split {
        vertical: bool,
        fraction: f32,
        first: Box<Pruned>,
        second: Box<Pruned>,
    },
}

impl Pruned {
    /// The tabs of the first (leftmost, topmost) pane.
    fn first_tabs(&self) -> &[TabId] {
        match self {
            Pruned::Tabs(tabs, _) => tabs,
            Pruned::Split { first, .. } => first.first_tabs(),
        }
    }
}

fn prune(
    node: &LayoutNode,
    tab_of: &impl Fn(&str) -> Option<TabId>,
    seen: &mut HashSet<TabId>,
) -> Option<Pruned> {
    match node {
        LayoutNode::Tabs { tabs, active } => {
            let mut kept = Vec::new();
            // The active tab, or the first known tab after it when it is unknown.
            let mut new_active = 0;
            for (i, key) in tabs.iter().enumerate() {
                if i == *active {
                    new_active = kept.len();
                }
                if let Some(tab) = tab_of(key)
                    && seen.insert(tab)
                {
                    kept.push(tab);
                }
            }
            if kept.is_empty() {
                return None;
            }
            let active = new_active.min(kept.len() - 1);
            Some(Pruned::Tabs(kept, active))
        }
        LayoutNode::Split {
            vertical,
            fraction,
            first,
            second,
        } => {
            let first = prune(first, tab_of, seen);
            let second = prune(second, tab_of, seen);
            match (first, second) {
                (Some(first), Some(second)) => Some(Pruned::Split {
                    vertical: *vertical,
                    fraction: clamp_fraction(*fraction),
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

/// Fills the subtree at `index`, which on entry is a pane holding `node.first_tabs()`.
fn build(tree: &mut Tree<TabId>, index: NodeIndex, node: Pruned) {
    match node {
        Pruned::Tabs(_, active) => {
            if let Ok(leaf) = tree.leaf_mut(index) {
                leaf.active = TabIndex(active);
            }
        }
        Pruned::Split {
            vertical,
            fraction,
            first,
            second,
        } => {
            // Splitting right/below keeps the old pane as the first child; the fraction is the
            // first child's share.
            let dir = if vertical { Split::Below } else { Split::Right };
            let new = Node::leaf_with(second.first_tabs().to_vec());
            let [old, new] = tree.split(index, dir, fraction, new);
            build(tree, old, *first);
            build(tree, new, *second);
        }
    }
}

/// The texts `egui_dock` may show, from the translation table instead of its English defaults.
fn translations(text: impl Fn(&str) -> &'static str) -> Translations {
    let close_all_hint = text("dock.close_all_hint");
    let minimize_hint = text("dock.minimize_hint");
    Translations {
        tab_context_menu: TabContextMenuTranslations {
            close_button: text("cmd.tab.close").to_owned(),
            eject_button: text("dock.eject").to_owned(),
            hide_tab_bar_button: text("dock.hide_tab_bar").to_owned(),
            show_tab_bar_button: text("dock.show_tab_bar").to_owned(),
        },
        leaf: LeafTranslations {
            close_button_disabled_tooltip: text("dock.not_closable").to_owned(),
            close_all_button: text("dock.close_all").to_owned(),
            close_all_button_menu_hint: close_all_hint.to_owned(),
            close_all_button_modifier_hint: close_all_hint.to_owned(),
            close_all_button_modifier_menu_hint: close_all_hint.to_owned(),
            close_all_button_disabled_tooltip: text("dock.not_closable").to_owned(),
            minimize_button: text("dock.minimize").to_owned(),
            minimize_button_menu_hint: minimize_hint.to_owned(),
            minimize_button_modifier_hint: minimize_hint.to_owned(),
            minimize_button_modifier_menu_hint: minimize_hint.to_owned(),
        },
        accessibility: AccessibilityTranslations {
            close_button: text("cmd.tab.close").to_owned(),
            add_button: text("dock.add_tab").to_owned(),
            close_all_button: text("dock.close_all").to_owned(),
            collapse_button: text("dock.minimize").to_owned(),
            expand_button: text("dock.expand").to_owned(),
            expand_window_button: text("dock.expand").to_owned(),
        },
    }
}

/// Adapts a [`DockHost`] to `egui_dock`'s `TabViewer`.
struct Viewer<'a, H: DockHost> {
    host: &'a mut H,
}

impl<H: DockHost> TabViewer for Viewer<'_, H> {
    type Tab = TabId;

    fn id(&mut self, tab: &mut TabId) -> Id {
        Id::unique(("refdesk-tab", *tab))
    }

    fn title(&mut self, tab: &mut TabId) -> WidgetText {
        self.host.title(*tab)
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut TabId) {
        self.host.ui(ui, *tab);
    }

    fn on_tab_button(&mut self, tab: &mut TabId, response: &egui::Response) {
        if let Some(tip) = self.host.tooltip(*tab) {
            let _ = response.clone().on_hover_text(tip);
        }
        if response.clicked() {
            self.host.activated(*tab);
        }
        if response.double_clicked() {
            self.host.title_double_clicked(*tab);
        }
        // `egui_dock`'s own tab menu is off so the host decides every entry and their order.
        response.context_menu(|ui| self.host.context_menu(ui, *tab));
    }

    fn on_close(&mut self, tab: &mut TabId) -> OnCloseResponse {
        // Removing tabs is `dispatch`'s job (rule R-6).
        self.host.close_requested(*tab);
        OnCloseResponse::Ignore
    }

    fn is_closeable(&self, tab: &TabId) -> bool {
        self.host.is_closeable(*tab)
    }

    fn allowed_in_windows(&self, _tab: &mut TabId) -> bool {
        // No `egui_dock` floating windows: tabs leave a window as real OS windows (WP-21).
        false
    }

    fn scroll_bars(&self, _tab: &TabId) -> [bool; 2] {
        // The views scroll themselves.
        [false, false]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, lookup};

    fn t(n: u64) -> TabId {
        TabId::for_test(n)
    }

    fn layout_of(tabs: &[u64]) -> DockLayout {
        let mut d = DockLayout::new();
        for n in tabs {
            d.add_tab(t(*n), None);
        }
        d
    }

    fn key(tab: TabId) -> String {
        format!("{tab:?}")
    }

    fn panes(d: &DockLayout) -> Vec<Vec<TabId>> {
        d.pane_rects().into_iter().map(|(_, tabs)| tabs).collect()
    }

    #[test]
    fn a_new_layout_is_empty() {
        let d = DockLayout::new();
        assert!(d.is_empty());
        assert_eq!(d.tab_count(), 0);
        assert_eq!(d.pane_count(), 0);
        assert_eq!(d.focused_tab(), None);
        assert_eq!(d.cycle(true), None);
        assert!(d.tabs().is_empty());
    }

    #[test]
    fn adding_to_an_empty_layout_makes_a_pane() {
        let mut d = DockLayout::new();
        d.add_tab(t(1), None);
        assert_eq!(d.pane_count(), 1);
        assert_eq!(d.tabs(), vec![t(1)]);
        assert_eq!(d.focused_tab(), Some(t(1)));
        assert!(d.contains(t(1)));
        assert!(!d.contains(t(2)));
    }

    #[test]
    fn adding_without_near_appends_to_the_focused_pane() {
        let mut d = layout_of(&[1, 2]);
        assert_eq!(d.tabs(), vec![t(1), t(2)]);
        assert_eq!(d.focused_tab(), Some(t(2)));
        d.split(t(1), t(3), SplitDir::Right);
        // The new pane is focused: the next tab goes there.
        d.add_tab(t(4), None);
        assert_eq!(panes(&d), vec![vec![t(1), t(2)], vec![t(3), t(4)]]);
        assert_eq!(d.focused_tab(), Some(t(4)));
    }

    #[test]
    fn adding_near_a_tab_puts_it_right_after_in_that_pane() {
        let mut d = layout_of(&[1, 2]);
        d.split(t(2), t(3), SplitDir::Down);
        d.add_tab(t(4), Some(t(1)));
        assert_eq!(panes(&d), vec![vec![t(1), t(4), t(2)], vec![t(3)]]);
        assert_eq!(d.focused_tab(), Some(t(4)));
        // A `near` that is not here falls back to the focused pane.
        d.add_tab(t(5), Some(t(99)));
        assert_eq!(panes(&d), vec![vec![t(1), t(4), t(2), t(5)], vec![t(3)]]);
        // A tab that is already here is not added twice.
        d.add_tab(t(5), None);
        assert_eq!(d.tab_count(), 5);
    }

    #[test]
    fn removing_the_active_tab_activates_its_right_neighbor_then_the_left() {
        let mut d = layout_of(&[1, 2, 3]);
        d.activate(t(2));
        assert!(d.remove_tab(t(2)));
        assert_eq!(d.focused_tab(), Some(t(3)));
        assert!(d.remove_tab(t(3)));
        assert_eq!(d.focused_tab(), Some(t(1)));
        assert!(!d.remove_tab(t(3)));
        assert!(d.remove_tab(t(1)));
        assert!(d.is_empty());
        assert_eq!(d.pane_count(), 0);
        assert_eq!(d.focused_tab(), None);
        // The layout is usable again after becoming empty.
        d.add_tab(t(7), None);
        assert_eq!(d.focused_tab(), Some(t(7)));
    }

    #[test]
    fn removing_another_tab_keeps_the_active_one() {
        let mut d = layout_of(&[1, 2, 3]);
        assert_eq!(d.focused_tab(), Some(t(3)));
        assert!(d.remove_tab(t(1)));
        assert_eq!(d.focused_tab(), Some(t(3)));
        assert_eq!(d.tabs(), vec![t(2), t(3)]);
    }

    #[test]
    fn removing_the_last_tab_of_a_pane_removes_the_pane_and_moves_the_focus() {
        let mut d = layout_of(&[1, 2]);
        d.split(t(2), t(3), SplitDir::Right);
        assert_eq!(d.pane_count(), 2);
        assert_eq!(d.focused_tab(), Some(t(3)));
        assert!(d.remove_tab(t(3)));
        assert_eq!(d.pane_count(), 1);
        assert_eq!(d.focused_tab(), Some(t(2)));

        // An unfocused pane that empties leaves the focus where it was.
        d.split(t(1), t(4), SplitDir::Down);
        d.activate(t(2));
        assert!(d.remove_tab(t(4)));
        assert_eq!(d.focused_tab(), Some(t(2)));
        assert_eq!(panes(&d), vec![vec![t(1), t(2)]]);
    }

    #[test]
    fn activate_focuses_the_pane_of_the_tab() {
        let mut d = layout_of(&[1, 2]);
        d.split(t(1), t(3), SplitDir::Right);
        assert_eq!(d.focused_tab(), Some(t(3)));
        assert!(d.activate(t(1)));
        assert_eq!(d.focused_tab(), Some(t(1)));
        assert!(d.activate(t(3)));
        assert_eq!(d.focused_tab(), Some(t(3)));
        // Activating an inactive tab of the other pane switches both.
        assert!(d.activate(t(2)));
        assert_eq!(d.focused_tab(), Some(t(2)));
        assert!(!d.activate(t(99)));
        assert_eq!(d.focused_tab(), Some(t(2)));
    }

    #[test]
    fn cycle_wraps_inside_the_focused_pane() {
        let mut d = layout_of(&[1, 2, 3]);
        d.split(t(3), t(4), SplitDir::Right);
        // The focused pane has one tab: cycling stays on it.
        assert_eq!(d.cycle(true), Some(t(4)));
        d.activate(t(3));
        assert_eq!(d.cycle(true), Some(t(1)));
        assert_eq!(d.cycle(false), Some(t(2)));
        d.activate(t(1));
        assert_eq!(d.cycle(false), Some(t(3)));
        // Cycling does not change anything by itself.
        assert_eq!(d.focused_tab(), Some(t(1)));
    }

    #[test]
    fn split_in_all_four_directions() {
        for (dir, vertical, new_first) in [
            (SplitDir::Left, false, true),
            (SplitDir::Right, false, false),
            (SplitDir::Up, true, true),
            (SplitDir::Down, true, false),
        ] {
            let mut d = layout_of(&[1, 2]);
            assert!(d.split(t(1), t(3), dir), "{dir:?}");
            assert_eq!(d.pane_count(), 2);
            assert_eq!(d.focused_tab(), Some(t(3)), "{dir:?}");
            let expected = if new_first {
                vec![vec![t(3)], vec![t(1), t(2)]]
            } else {
                vec![vec![t(1), t(2)], vec![t(3)]]
            };
            assert_eq!(panes(&d), expected, "{dir:?}");
            match d.to_layout(key) {
                LayoutNode::Split { vertical: v, .. } => assert_eq!(v, vertical, "{dir:?}"),
                other => panic!("{dir:?}: not a split: {other:?}"),
            }
        }
    }

    #[test]
    fn split_refuses_unknown_and_duplicate_tabs() {
        let mut d = layout_of(&[1]);
        assert!(!d.split(t(9), t(2), SplitDir::Right));
        assert!(!d.split(t(1), t(1), SplitDir::Right));
        assert_eq!(d.pane_count(), 1);
    }

    #[test]
    fn splitting_a_pane_inside_a_split_nests() {
        let mut d = layout_of(&[1]);
        d.split(t(1), t(2), SplitDir::Right);
        d.split(t(2), t(3), SplitDir::Down);
        d.split(t(1), t(4), SplitDir::Down);
        assert_eq!(
            panes(&d),
            vec![vec![t(1)], vec![t(4)], vec![t(2)], vec![t(3)]]
        );
        assert_eq!(d.tabs(), vec![t(1), t(4), t(2), t(3)]);
    }

    #[test]
    fn focus_next_pane_walks_the_panes_in_order_and_wraps() {
        let mut d = layout_of(&[1]);
        assert_eq!(d.focus_next_pane(), Some(t(1)), "one pane: stays");
        d.split(t(1), t(2), SplitDir::Right);
        d.split(t(2), t(3), SplitDir::Down);
        d.activate(t(1));
        assert_eq!(d.focus_next_pane(), Some(t(2)));
        assert_eq!(d.focused_tab(), Some(t(2)));
        assert_eq!(d.focus_next_pane(), Some(t(3)));
        assert_eq!(d.focus_next_pane(), Some(t(1)));
        assert_eq!(DockLayout::new().focus_next_pane(), None);
    }

    #[test]
    fn pane_tabs_lists_the_pane_of_a_tab() {
        let mut d = layout_of(&[1, 2]);
        d.split(t(2), t(3), SplitDir::Right);
        assert_eq!(d.pane_tabs(t(1)), vec![t(1), t(2)]);
        assert_eq!(d.pane_tabs(t(3)), vec![t(3)]);
        assert!(d.pane_tabs(t(9)).is_empty());
    }

    // --- session format ----------------------------------------------------------------------

    fn tab_of(name: &str) -> Option<TabId> {
        name.strip_prefix("TabId(")
            .and_then(|s| s.strip_suffix(')'))
            .and_then(|s| s.parse().ok())
            .map(TabId::for_test)
    }

    fn round_trip(d: &DockLayout) {
        let first = d.to_layout(key);
        let back = DockLayout::from_layout(&first, tab_of);
        assert_eq!(back.to_layout(key), first);
        assert_eq!(back.tabs(), d.tabs());
    }

    fn tabs_node(keys: &[&str], active: usize) -> LayoutNode {
        LayoutNode::Tabs {
            tabs: keys.iter().map(|k| (*k).to_owned()).collect(),
            active,
        }
    }

    #[test]
    fn round_trip_of_a_single_pane() {
        let mut d = layout_of(&[1, 2, 3]);
        d.activate(t(2));
        round_trip(&d);
        assert_eq!(
            d.to_layout(key),
            tabs_node(&["TabId(1)", "TabId(2)", "TabId(3)"], 1)
        );
    }

    #[test]
    fn round_trip_of_a_left_right_split() {
        let mut d = layout_of(&[1, 2]);
        d.split(t(2), t(3), SplitDir::Right);
        d.add_tab(t(4), None);
        d.activate(t(3));
        d.activate(t(1));
        round_trip(&d);
        let layout = d.to_layout(key);
        assert_eq!(
            layout,
            LayoutNode::Split {
                vertical: false,
                fraction: 0.5,
                first: Box::new(tabs_node(&["TabId(1)", "TabId(2)"], 0)),
                second: Box::new(tabs_node(&["TabId(3)", "TabId(4)"], 0)),
            }
        );
    }

    #[test]
    fn round_trip_of_three_nested_splits_keeps_directions_fractions_and_active_tabs() {
        let layout = LayoutNode::Split {
            vertical: false,
            fraction: 0.3,
            first: Box::new(tabs_node(&["TabId(1)", "TabId(2)"], 1)),
            second: Box::new(LayoutNode::Split {
                vertical: true,
                fraction: 0.7,
                first: Box::new(LayoutNode::Split {
                    vertical: false,
                    fraction: 0.25,
                    first: Box::new(tabs_node(&["TabId(3)"], 0)),
                    second: Box::new(tabs_node(&["TabId(4)", "TabId(5)", "TabId(6)"], 2)),
                }),
                second: Box::new(tabs_node(&["TabId(7)"], 0)),
            }),
        };
        let d = DockLayout::from_layout(&layout, tab_of);
        assert_eq!(d.to_layout(key), layout);
        assert_eq!(d.pane_count(), 4);
        assert_eq!(d.tabs(), (1..=7).map(t).collect::<Vec<_>>());
        round_trip(&d);
    }

    #[test]
    fn unknown_tab_keys_are_skipped_and_empty_panes_fold_away() {
        let layout = LayoutNode::Split {
            vertical: false,
            fraction: 0.4,
            first: Box::new(tabs_node(&["gone", "TabId(1)", "TabId(2)"], 0)),
            second: Box::new(LayoutNode::Split {
                vertical: true,
                fraction: 0.6,
                first: Box::new(tabs_node(&["gone", "also gone"], 1)),
                second: Box::new(tabs_node(&["TabId(3)", "missing"], 1)),
            }),
        };
        let d = DockLayout::from_layout(&layout, tab_of);
        assert_eq!(
            d.to_layout(key),
            LayoutNode::Split {
                vertical: false,
                fraction: 0.4,
                // The unknown active tab gave way to the next known one.
                first: Box::new(tabs_node(&["TabId(1)", "TabId(2)"], 0)),
                // The inner split lost a side and became its other side.
                second: Box::new(tabs_node(&["TabId(3)"], 0)),
            }
        );
        assert_eq!(d.focused_tab(), Some(t(1)));
    }

    #[test]
    fn a_layout_with_only_unknown_keys_is_empty() {
        let layout = LayoutNode::Split {
            vertical: true,
            fraction: 0.5,
            first: Box::new(tabs_node(&["a"], 0)),
            second: Box::new(tabs_node(&["b", "c"], 1)),
        };
        let d = DockLayout::from_layout(&layout, tab_of);
        assert!(d.is_empty());
        assert_eq!(d.to_layout(key), LayoutNode::default());
        assert!(DockLayout::from_layout(&LayoutNode::default(), tab_of).is_empty());
    }

    #[test]
    fn repeated_keys_and_bad_values_from_a_file_are_tamed() {
        let layout = LayoutNode::Split {
            vertical: false,
            fraction: f32::NAN,
            first: Box::new(tabs_node(&["TabId(1)", "TabId(1)"], 9)),
            second: Box::new(LayoutNode::Split {
                vertical: false,
                fraction: 3.0,
                first: Box::new(tabs_node(&["TabId(1)"], 0)),
                second: Box::new(tabs_node(&["TabId(2)"], 0)),
            }),
        };
        let d = DockLayout::from_layout(&layout, tab_of);
        assert_eq!(d.tabs(), vec![t(1), t(2)]);
        assert_eq!(
            d.to_layout(key),
            LayoutNode::Split {
                vertical: false,
                fraction: 0.5,
                first: Box::new(tabs_node(&["TabId(1)"], 0)),
                second: Box::new(tabs_node(&["TabId(2)"], 0)),
            }
        );
    }

    #[test]
    fn translations_replace_every_english_default() {
        let ko = translations(|k| lookup(Lang::Ko, k).unwrap_or("???"));
        let en = Translations::english();
        let texts = |t: &Translations| {
            let m = &t.tab_context_menu;
            let l = &t.leaf;
            let a = &t.accessibility;
            vec![
                m.close_button.clone(),
                m.eject_button.clone(),
                m.hide_tab_bar_button.clone(),
                m.show_tab_bar_button.clone(),
                l.close_button_disabled_tooltip.clone(),
                l.close_all_button.clone(),
                l.close_all_button_menu_hint.clone(),
                l.close_all_button_modifier_hint.clone(),
                l.close_all_button_modifier_menu_hint.clone(),
                l.close_all_button_disabled_tooltip.clone(),
                l.minimize_button.clone(),
                l.minimize_button_menu_hint.clone(),
                l.minimize_button_modifier_hint.clone(),
                l.minimize_button_modifier_menu_hint.clone(),
                a.close_button.clone(),
                a.add_button.clone(),
                a.close_all_button.clone(),
                a.collapse_button.clone(),
                a.expand_button.clone(),
                a.expand_window_button.clone(),
            ]
        };
        for (ours, default) in texts(&ko).iter().zip(texts(&en)) {
            assert_ne!(ours, "???", "missing key");
            assert_ne!(*ours, default);
        }
    }

    // --- drawing -----------------------------------------------------------------------------

    #[derive(Default)]
    struct TestHost {
        drawn: Vec<TabId>,
        closed: Vec<TabId>,
        activated: Vec<TabId>,
    }

    impl DockHost for TestHost {
        fn title(&mut self, tab: TabId) -> WidgetText {
            format!("{tab:?}").into()
        }
        fn tooltip(&mut self, _tab: TabId) -> Option<String> {
            Some("tip".to_owned())
        }
        fn ui(&mut self, ui: &mut Ui, tab: TabId) {
            self.drawn.push(tab);
            ui.label("content");
        }
        fn context_menu(&mut self, ui: &mut Ui, _tab: TabId) {
            ui.label("menu");
        }
        fn is_closeable(&self, _tab: TabId) -> bool {
            true
        }
        fn close_requested(&mut self, tab: TabId) {
            self.closed.push(tab);
        }
        fn activated(&mut self, tab: TabId) {
            self.activated.push(tab);
        }
        fn title_double_clicked(&mut self, _tab: TabId) {}
    }

    fn frame(ctx: &egui::Context, d: &mut DockLayout, host: &mut TestHost) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                d.show(ui, Id::unique("test-dock"), host);
            });
        })
        .drop_without_applying_deltas();
    }

    #[test]
    fn drawing_shows_the_active_tab_of_every_pane_and_records_rects() {
        let ctx = egui::Context::default();
        let mut d = layout_of(&[1, 2]);
        let mut host = TestHost::default();
        frame(&ctx, &mut d, &mut host);
        assert_eq!(host.drawn, vec![t(2)]);

        d.split(t(2), t(3), SplitDir::Right);
        host.drawn.clear();
        frame(&ctx, &mut d, &mut host);
        frame(&ctx, &mut d, &mut host);
        let rects = d.pane_rects();
        assert_eq!(rects.len(), 2);
        let (left, right) = (rects[0].0, rects[1].0);
        assert!(left.is_positive() && right.is_positive());
        assert!(left.right() <= right.left() + 1.0, "{left:?} {right:?}");
        assert!(host.drawn.contains(&t(2)) && host.drawn.contains(&t(3)));
        // Nothing was clicked: no close request, no focus change.
        assert!(host.closed.is_empty());
        assert!(host.activated.is_empty());
        assert_eq!(d.tab_count(), 3);
    }

    #[test]
    fn drawing_an_empty_layout_does_not_panic() {
        let ctx = egui::Context::default();
        let mut d = DockLayout::new();
        let mut host = TestHost::default();
        frame(&ctx, &mut d, &mut host);
        assert!(host.drawn.is_empty());
    }
}
