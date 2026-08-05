//! View model for the org sidebar — the cross-tab panel that shows
//! every tab, the panes inside each tab, and how busy the Claude
//! session in each pane is.
//!
//! The row list is rebuilt from `App` on demand rather than kept as
//! incrementally-maintained state. Tabs, panes, names and roles all
//! move under MCP control at any moment, so a cache would need
//! invalidation hooks in a dozen places; rebuilding a handful of rows
//! is far cheaper than getting that wrong. What *is* cached is the
//! expensive part — the per-pane Claude snapshots, refreshed on a timer
//! by [`App::tick_claude_snapshots`] rather than per frame.

use super::*;

use crate::claude_monitor::ClaudeSnapshot;

/// A click / selection target inside the sidebar.
///
/// Deliberately `(tab index, pane id)` rather than a row index: rows
/// shift whenever a tab is opened, closed or split, and a stale row
/// index would silently retarget a click at whatever moved into that
/// line. `pane_id` is `None` for a tab header row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrgSidebarTarget {
    pub tab: usize,
    pub pane_id: Option<usize>,
}

impl OrgSidebarTarget {
    pub(crate) fn tab(tab: usize) -> Self {
        Self { tab, pane_id: None }
    }
}

/// What kind of process a pane row is showing, used to pick the marker
/// glyph and colour. Mirrors the `claude` / `codex` / `shell` labels
/// `render_single_pane` puts in the pane border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrgPaneKind {
    Claude,
    Codex,
    Shell,
}

/// One rendered line of the sidebar.
pub(crate) struct OrgSidebarRow {
    pub target: OrgSidebarTarget,
    /// Tab header rows carry the tab's display name; pane rows carry
    /// the role, the registered pane name, or a `#id` fallback.
    pub label: String,
    pub is_active_tab: bool,
    /// Pane rows only.
    pub kind: Option<OrgPaneKind>,
    /// Pane rows only — `true` when this is its tab's focused pane.
    pub is_focused_pane: bool,
    /// Pane rows carry their own snapshot; tab header rows carry the
    /// aggregate `(working panes, total panes)` instead.
    pub snapshot: Option<ClaudeSnapshot>,
    pub working_panes: usize,
    pub total_panes: usize,
}

impl App {
    /// Is the sidebar eligible to be shown at all? `off` disables the
    /// feature outright, including its toggle key, so every call site
    /// can gate on this one predicate.
    pub(crate) fn org_sidebar_enabled(&self) -> bool {
        self.org_sidebar_mode != crate::config::OrgSidebarMode::Off
    }

    /// Is the sidebar being painted right now (mode allows it *and* the
    /// runtime toggle is on)? Does not account for the narrow-terminal
    /// degrade ladder — use `last_org_sidebar_rect` for that.
    pub(crate) fn org_sidebar_active(&self) -> bool {
        self.org_sidebar_enabled() && self.org_sidebar_visible
    }

    /// Build the full row list, top to bottom.
    ///
    /// Pane order within a tab comes from `LayoutNode::collect_pane_ids`
    /// — the layout tree's own first-then-second walk, which is the
    /// same order `calculate_rects` paints in — so the sidebar lists
    /// panes the way they appear on screen rather than in `HashMap`
    /// iteration order.
    pub(crate) fn org_sidebar_rows(&self) -> Vec<OrgSidebarRow> {
        let mut rows = Vec::new();
        for (tab, ws) in self.workspaces.iter().enumerate() {
            let pane_ids = ws.layout.collect_pane_ids();
            let is_active_tab = tab == self.active_tab;

            let working_panes = pane_ids
                .iter()
                .filter(|id| self.claude_snapshots.get(id).is_some_and(|s| s.is_working))
                .count();

            rows.push(OrgSidebarRow {
                target: OrgSidebarTarget::tab(tab),
                label: ws.display_name().to_string(),
                is_active_tab,
                kind: None,
                is_focused_pane: false,
                snapshot: None,
                working_panes,
                total_panes: pane_ids.len(),
            });

            for pane_id in pane_ids {
                let Some(pane) = ws.panes.get(&pane_id) else {
                    // `collect_pane_ids` walks the layout tree, which can
                    // briefly outlive the pane map during teardown.
                    continue;
                };
                let kind = if pane.claude_ever_seen() {
                    OrgPaneKind::Claude
                } else if pane.codex_ever_seen() {
                    OrgPaneKind::Codex
                } else {
                    OrgPaneKind::Shell
                };
                rows.push(OrgSidebarRow {
                    target: OrgSidebarTarget {
                        tab,
                        pane_id: Some(pane_id),
                    },
                    label: pane_row_label(ws, pane),
                    is_active_tab,
                    kind: Some(kind),
                    is_focused_pane: ws.focused_pane_id == pane_id,
                    snapshot: self.claude_snapshots.get(&pane_id).cloned(),
                    working_panes: 0,
                    total_panes: 0,
                });
            }
        }
        rows
    }

    /// Resolve the stored selection to an index into [`Self::org_sidebar_rows`].
    ///
    /// Falls back to the active tab's header row when the selection
    /// points at a tab or pane that has since gone away, so the sidebar
    /// never renders with nothing selected.
    pub(crate) fn org_sidebar_selected_index(&self, rows: &[OrgSidebarRow]) -> usize {
        if let Some(sel) = self.org_sidebar_selection {
            if let Some(i) = rows.iter().position(|r| r.target == sel) {
                return i;
            }
        }
        rows.iter()
            .position(|r| r.target == OrgSidebarTarget::tab(self.active_tab))
            .unwrap_or(0)
    }

    /// Move the selection by `delta` rows, clamped to the ends.
    pub(crate) fn org_sidebar_move_selection(&mut self, delta: isize) {
        let rows = self.org_sidebar_rows();
        if rows.is_empty() {
            return;
        }
        let current = self.org_sidebar_selected_index(&rows) as isize;
        let next = (current + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.org_sidebar_selection = Some(rows[next].target);
        self.dirty = true;
    }

    /// Act on the selected row: switch to its tab, and for a pane row
    /// also focus that pane.
    ///
    /// Activating a tab header keeps focus in the sidebar so the user
    /// can keep browsing; activating a pane row is an explicit "take me
    /// there", so focus follows into the pane.
    pub(crate) fn org_sidebar_activate(&mut self) {
        let rows = self.org_sidebar_rows();
        if rows.is_empty() {
            return;
        }
        let target = rows[self.org_sidebar_selected_index(&rows)].target;
        self.org_sidebar_activate_target(target);
    }

    pub(crate) fn org_sidebar_activate_target(&mut self, target: OrgSidebarTarget) {
        if target.tab >= self.workspaces.len() {
            return;
        }
        self.org_sidebar_selection = Some(target);
        self.switch_tab(target.tab);
        if let Some(pane_id) = target.pane_id {
            if self.workspaces[target.tab].panes.contains_key(&pane_id) {
                let ws = &mut self.workspaces[target.tab];
                ws.focused_pane_id = pane_id;
                ws.focus_target = FocusTarget::Pane;
            }
        }
        self.dirty = true;
    }

    /// Clamp `org_sidebar_scroll` so the selected row stays on screen.
    /// Called by the renderer once it knows how many rows fit.
    pub(crate) fn org_sidebar_ensure_visible(&mut self, selected: usize, visible_height: usize) {
        if visible_height == 0 {
            return;
        }
        if selected < self.org_sidebar_scroll {
            self.org_sidebar_scroll = selected;
        } else if selected >= self.org_sidebar_scroll + visible_height {
            self.org_sidebar_scroll = selected + 1 - visible_height;
        }
    }

    /// Drop every cached click target and scroll position. Called when
    /// the tab set changes, because row indices and tab indices both
    /// shift underneath the cache.
    pub(crate) fn reset_org_sidebar_caches(&mut self) {
        self.org_sidebar_row_targets.clear();
        self.org_sidebar_scroll = 0;
        self.org_sidebar_selection = None;
    }
}

/// Label for a pane row: the role if one was set (roles are the whole
/// point of the org view), else the registered IPC name, else `#id`.
fn pane_row_label(ws: &Workspace, pane: &Pane) -> String {
    if let Some(role) = pane.role.as_deref() {
        return role.to_string();
    }
    if let Some(name) = ws
        .pane_names
        .iter()
        .find(|(_, id)| **id == pane.id)
        .map(|(n, _)| n.as_str())
    {
        return name.to_string();
    }
    format!("#{}", pane.id)
}
