use super::*;

impl App {
    pub(crate) fn new_tab(&mut self) -> Result<usize> {
        self.new_tab_with_cwd(None)
    }

    pub(crate) fn new_tab_with_cwd(&mut self, cwd_override: Option<PathBuf>) -> Result<usize> {
        let cwd = cwd_override
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let name = dir_name(&cwd);
        let pane_id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);

        let ws = Workspace::new(name, cwd, pane_id, 10, 40, self.event_tx.clone())?;
        self.workspaces.push(ws);
        self.active_tab = self.workspaces.len() - 1;
        self.suspend_overlay();
        // Sidebar rows are keyed by tab index, so the whole cache is
        // stale the moment the tab set changes.
        self.reset_org_sidebar_caches();
        Ok(pane_id)
    }

    pub(crate) fn close_tab(&mut self, index: usize) {
        if self.workspaces.len() <= 1 {
            return;
        }

        let pane_ids_in_tab: Vec<usize> = self.workspaces[index].panes.keys().copied().collect();

        let mut to_emit: Vec<(usize, Option<String>, Option<String>)> = Vec::new();
        {
            let ws = &mut self.workspaces[index];
            for pid in &pane_ids_in_tab {
                let name = ws
                    .pane_names
                    .iter()
                    .find(|(_, id)| **id == *pid)
                    .map(|(n, _)| n.clone());
                if let Some(pane) = ws.panes.get_mut(pid) {
                    if !pane.exit_event_emitted {
                        pane.exit_event_emitted = true;
                        to_emit.push((*pid, name, pane.role.clone()));
                    }
                }
            }
            for pid in &pane_ids_in_tab {
                self.saved_overlay_drafts.remove(pid);
                self.claude_monitor.remove(*pid);
                self.peer_client_kinds.remove(pid);
                self.pending_codex_peer_messages.remove(pid);
            }
        }
        if self
            .codex_peer_notification
            .as_ref()
            .is_some_and(|n| pane_ids_in_tab.contains(&n.target_pane))
        {
            self.codex_peer_notification = None;
        }

        let overlay_in_tab = self
            .overlay
            .as_ref()
            .is_some_and(|o| self.workspaces[index].panes.contains_key(&o.target_pane));
        if overlay_in_tab {
            self.overlay = None;
        }

        self.workspaces[index].shutdown();
        self.workspaces.remove(index);
        // `remove` shifts every later tab down one slot. Clamping only
        // when `active_tab` runs off the end (the pre-#291 behaviour)
        // silently retargets the active tab whenever an *earlier* tab
        // closes: with [A,B,C,D] and the user on C (index 2), closing B
        // leaves [A,C,D] and index 2 now points at D. Because the
        // numeric value never changed, the `prev_active != active_tab`
        // guard below also skipped `suspend_overlay`, stranding the IME
        // overlay on a pane in a different tab. Follow the shift
        // instead. Reachable from MCP `close_pane` / `close_tab` on a
        // background tab, and now from the org sidebar's cross-tab view.
        let prev_active = self.active_tab;
        if index < self.active_tab {
            self.active_tab -= 1;
        } else if self.active_tab >= self.workspaces.len() {
            self.active_tab = self.workspaces.len() - 1;
        }
        // Suspend only when the workspace the user is *looking at*
        // changed, which is exactly "we closed the active tab".
        // `prev_active != active_tab` is not that test any more: now
        // that an earlier close decrements the index, it also fires for
        // `index < prev_active`, where the visible workspace is
        // unchanged — and tearing down a half-composed IME overlay
        // because some other tab closed in the background would be a
        // regression of its own.
        if index == prev_active {
            self.suspend_overlay();
        }
        // Tab indices shift after removal; any pending outer-edge or
        // boundary double-click is keyed by the pre-removal index and
        // would now point at a different workspace.
        self.last_edge_click = None;
        self.last_boundary_click = None;
        self.last_tab_click = None;
        self.reset_org_sidebar_caches();
        self.mark_layout_change();
        // A tab that vanished under a pending prompt (MCP close of its
        // last pane, or the tab itself) can no longer be confirmed.
        self.revalidate_close_confirm();
        for (pid, name, role) in to_emit {
            self.emit_pane_exited(pid, name, role);
        }
    }

    const MAX_PANES: usize = 16;

    pub(crate) fn split_focused_pane(
        &mut self,
        direction: SplitDirection,
        cwd_override: Option<PathBuf>,
    ) -> Result<Option<usize>> {
        self.split_focused_pane_with_position(direction, false, cwd_override)
    }

    /// Like [`Self::split_focused_pane`] but lets the caller place the
    /// new pane on the first (top / left) side instead of the default
    /// second (bottom / right). Used by the outer-edge double-click
    /// path so a click on the top or left edge spawns the new pane on
    /// the clicked side. All min-size / MAX_PANES guards match
    /// `split_focused_pane`.
    pub(crate) fn split_focused_pane_with_position(
        &mut self,
        direction: SplitDirection,
        new_pane_first: bool,
        cwd_override: Option<PathBuf>,
    ) -> Result<Option<usize>> {
        let ws_index = self.active_tab;
        let target = self.ws().focused_pane_id;
        self.split_pane_in_workspace(ws_index, target, direction, new_pane_first, cwd_override)
    }

    /// Split `target_pane_id` inside workspace `ws_index`, regardless of
    /// which tab is currently on screen.
    ///
    /// Every piece of state this touches — the pane cap, the geometry
    /// used for the min-size guard and the new PTY's first-frame size,
    /// the layout tree, the inherited cwd, the post-split focus — is
    /// per-workspace, so all of it is indexed rather than read through
    /// `self.ws()`. Splitting a hidden tab must not disturb the visible
    /// one, which is why the relayout at the end is targeted too:
    /// `mark_layout_change` (with its repaint cooldown) is only right
    /// for the tab the user is actually watching.
    pub(crate) fn split_pane_in_workspace(
        &mut self,
        ws_index: usize,
        target_pane_id: usize,
        direction: SplitDirection,
        new_pane_first: bool,
        cwd_override: Option<PathBuf>,
    ) -> Result<Option<usize>> {
        if self.workspaces.get(ws_index).is_none() {
            return Ok(None);
        }
        if self.workspaces[ws_index].layout.pane_count() >= Self::MAX_PANES {
            return Ok(None);
        }

        // A hidden workspace's `last_pane_rects` are frozen at whatever
        // they were when it was last on screen: only the active tab is
        // relaid out on a terminal resize or a sidebar toggle. Reading
        // them here without refreshing would run the min-size guard —
        // and seed the new PTY — against a terminal width that no
        // longer exists, in both directions: a split that should be
        // refused gets through after the user shrinks the terminal, and
        // a legal one is refused after they enlarge it. The active tab
        // is already accurate (`ui::render_panes` rewrites its rects
        // every frame), so leave it alone rather than pay a redundant
        // resize pass on the TUI's own split path.
        if ws_index != self.active_tab {
            self.relayout_workspace(ws_index);
        }

        let focused_rect = self.workspaces[ws_index]
            .last_pane_rects
            .iter()
            .find(|(id, _)| *id == target_pane_id)
            .map(|&(_, rect)| rect);

        if let Some(rect) = focused_rect {
            match direction {
                SplitDirection::Vertical => {
                    if rect.width / 2 < self.min_pane_width {
                        return Ok(None);
                    }
                }
                SplitDirection::Horizontal => {
                    if rect.height / 2 < self.min_pane_height {
                        return Ok(None);
                    }
                }
            }
        }

        let new_id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);

        let cwd = cwd_override.or_else(|| {
            self.workspaces[ws_index]
                .panes
                .get(&target_pane_id)
                .map(|p| p.cwd.clone())
        });

        // Seed the new PTY with the geometry it will actually get after the
        // split, minus the 1-cell border on each side. Falling back to a
        // fixed 10x40 (the old behavior) forced every fresh pane through a
        // startup resize/reflow once `render_panes` corrected the size, a
        // contributing factor to caret desync on plain PTY panes. The next
        // render still calls `pane.resize(...)` with the exact rect, so this
        // is purely a better first-frame estimate.
        let (init_rows, init_cols) = match focused_rect {
            Some(rect) => {
                let (w, h) = match direction {
                    SplitDirection::Vertical => (rect.width / 2, rect.height),
                    SplitDirection::Horizontal => (rect.width, rect.height / 2),
                };
                (h.saturating_sub(2).max(1), w.saturating_sub(2).max(1))
            }
            None => (10, 40),
        };
        let pane = Pane::new_with_cwd(new_id, init_rows, init_cols, self.event_tx.clone(), cwd)?;
        let ws = &mut self.workspaces[ws_index];
        ws.panes.insert(new_id, pane);
        ws.layout
            .split_pane_with_position(target_pane_id, new_id, direction, new_pane_first);
        // Focus follows the new pane *within its own tab*. For the
        // visible tab that is the long-standing behavior; for a hidden
        // tab it is what the user will find when they switch to it.
        ws.focused_pane_id = new_id;

        if ws_index == self.active_tab {
            self.mark_layout_change();
        } else {
            // A hidden workspace never passes through `ui::render_panes`,
            // so nothing else would refresh its `last_pane_rects`. Skip
            // this and the very next `list_panes` reports the new pane at
            // 0x0 with zero width/height — geometry the caller would take
            // at face value. Recompute for that workspace alone: no
            // repaint cooldown, no effect on the tab on screen.
            self.relayout_workspace(ws_index);
            self.selection = None;
            self.dirty = true;
        }
        // A split during a single-pane tab-close prompt would widen the
        // blast radius past what the user agreed to.
        self.revalidate_close_confirm();
        Ok(Some(new_id))
    }

    pub(crate) fn spawn_claude_in_selected_dir(&mut self, direction: SplitDirection) -> Result<()> {
        let raw_cwd = self.ws().file_tree.selected_launch_cwd();
        let canon = raw_cwd.canonicalize().unwrap_or_else(|_| raw_cwd.clone());
        let cwd = if canon.is_dir() {
            strip_verbatim_prefix(canon)
        } else {
            self.ws().file_tree.root_path.clone()
        };

        let Some(new_id) = self.split_focused_pane(direction, Some(cwd))? else {
            return Ok(());
        };

        if let Some(pane) = self.ws_mut().panes.get_mut(&new_id) {
            pane.queue_startup_text(&format!("{CLAUDE_PEER_LAUNCH_CMD} "));
        }
        self.emit_pane_started(new_id);
        Ok(())
    }

    pub fn apply_layout(&mut self, config: &LayoutConfig) -> Result<()> {
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        validate_layout_cwds(&config.root, &base)?;
        let initial_pane_id = self.ws().focused_pane_id;
        self.apply_layout_node(&config.root, initial_pane_id)?;
        Ok(())
    }

    fn apply_layout_node(&mut self, node: &LayoutNodeSpec, target_pane_id: usize) -> Result<()> {
        match node {
            LayoutNodeSpec::Pane {
                id,
                command,
                role,
                cwd: _,
            } => {
                if !id.is_empty() {
                    self.ws_mut().pane_names.insert(id.clone(), target_pane_id);
                }
                if let Some(pane) = self.ws_mut().panes.get_mut(&target_pane_id) {
                    if let Some(r) = role {
                        pane.role = Some(r.clone());
                    }
                    if let Some(cmd) = command {
                        let upgraded = crate::mcp_peer::upgrade_claude_command(cmd);
                        pane.queue_startup_command(&upgraded);
                    }
                }
                Ok(())
            }
            LayoutNodeSpec::Split {
                direction,
                ratio: _,
                first,
                second,
            } => {
                self.ws_mut().focused_pane_id = target_pane_id;
                let split_dir = match direction {
                    DirectionSpec::Vertical => SplitDirection::Vertical,
                    DirectionSpec::Horizontal => SplitDirection::Horizontal,
                };
                let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let new_pane_cwd = subtree_root_cwd(second, &base);
                let new_pane_id = self
                    .split_focused_pane(split_dir, new_pane_cwd)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "layout split refused (too small or MAX_PANES) while applying layout"
                        )
                    })?;
                self.apply_layout_node(first, target_pane_id)?;
                self.apply_layout_node(second, new_pane_id)?;
                self.emit_pane_started(new_pane_id);
                Ok(())
            }
        }
    }

    // ─── Ctrl+W close confirmation (Issue #285) ───────────
    //
    // Two clearly separated routes into the same destructive
    // primitives:
    //
    //   TUI  : `request_close_focused_*` → pending [`CloseConfirm`]
    //          → (user presses `y`) → `close_*_now`
    //   MCP  : `handle_close` → immediate close, no pending state
    //
    // The MCP contract is "close_pane closes the pane", so automation
    // never observes — let alone waits on — a human confirmation.

    /// Ctrl+W on a tab with more than one pane: remember the *focused
    /// pane id* and put up the confirmation. Nothing is destroyed here.
    pub(crate) fn request_close_focused_pane(&mut self) {
        let pane_id = self.ws().focused_pane_id;
        if self.ws().layout.pane_count() <= 1 {
            return;
        }
        self.close_confirm = Some(CloseConfirm::Pane { pane_id });
        self.dirty = true;
    }

    /// Ctrl+W on a single-pane tab: remember the tab by an anchor pane
    /// plus the full pane-id snapshot, so a concurrent split can be
    /// detected on confirm.
    pub(crate) fn request_close_focused_tab(&mut self) {
        if self.workspaces.len() <= 1 {
            return;
        }
        let mut expected_pane_ids = self.ws().layout.collect_pane_ids();
        expected_pane_ids.sort_unstable();
        let Some(&anchor_pane_id) = expected_pane_ids.first() else {
            return;
        };
        self.close_confirm = Some(CloseConfirm::Tab {
            anchor_pane_id,
            expected_pane_ids,
        });
        self.dirty = true;
    }

    /// `n` / `Esc` (and any other invalidation): drop the prompt.
    pub(crate) fn cancel_close_confirm(&mut self) {
        if self.close_confirm.take().is_some() {
            self.dirty = true;
        }
    }

    /// `y`: execute exactly what was pinned at request time, or
    /// nothing at all if the world moved underneath us.
    pub(crate) fn confirm_close_now(&mut self) {
        // Take first: the close primitives call
        // `revalidate_close_confirm`, which must not see the entry
        // it is in the middle of executing.
        let Some(pending) = self.close_confirm.take() else {
            return;
        };
        self.dirty = true;
        match pending {
            CloseConfirm::Pane { pane_id } => self.close_pane_now(pane_id),
            CloseConfirm::Tab {
                anchor_pane_id,
                expected_pane_ids,
            } => self.close_tab_now(anchor_pane_id, &expected_pane_ids),
        }
    }

    /// Close one confirmed pane. Re-resolves the workspace from the
    /// pane id (never `active_tab`) and refuses to escalate: if the
    /// pane became its tab's only pane while the prompt was up, the
    /// user's "close this pane" must not silently become "close this
    /// whole tab".
    ///
    /// A pane that exited naturally (`exited = true`) is still in the
    /// layout, so `y` simply removes it;
    /// [`Self::remove_pane_from_layout`]'s `exit_event_emitted` guard
    /// keeps `PaneExited` exactly-once.
    fn close_pane_now(&mut self, pane_id: usize) {
        let Some(ws_index) = self.workspace_index_of_pane(pane_id) else {
            return;
        };
        if self.workspaces[ws_index].layout.pane_count() <= 1 {
            return;
        }
        let _ = self.remove_pane_from_layout(ws_index, pane_id);
    }

    /// Close one confirmed tab, but only if it is still the same tab
    /// holding exactly the same panes as when the prompt went up.
    fn close_tab_now(&mut self, anchor_pane_id: usize, expected_pane_ids: &[usize]) {
        if self.workspaces.len() <= 1 {
            return;
        }
        let Some(ws_index) = self.workspace_index_of_pane(anchor_pane_id) else {
            return;
        };
        let mut current = self.workspaces[ws_index].layout.collect_pane_ids();
        current.sort_unstable();
        if current != expected_pane_ids {
            return;
        }
        self.close_tab(ws_index);
    }

    pub(crate) fn workspace_index_of_pane(&self, pane_id: usize) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|ws| ws.panes.contains_key(&pane_id))
    }

    /// Drop a pending confirmation whose target no longer matches
    /// reality. Called from every layout mutation (MCP close, MCP
    /// split, tab close) so the modal disappears the moment it stops
    /// describing something the user can still agree to.
    pub(crate) fn revalidate_close_confirm(&mut self) {
        let still_valid = match self.close_confirm.as_ref() {
            None => return,
            Some(CloseConfirm::Pane { pane_id }) => self
                .workspace_index_of_pane(*pane_id)
                // Not just "does the pane exist": once it is the tab's
                // only pane the confirmed action is no longer possible.
                .is_some_and(|i| self.workspaces[i].layout.pane_count() > 1),
            Some(CloseConfirm::Tab {
                anchor_pane_id,
                expected_pane_ids,
            }) => {
                self.workspaces.len() > 1
                    && self
                        .workspace_index_of_pane(*anchor_pane_id)
                        .is_some_and(|i| {
                            let mut current = self.workspaces[i].layout.collect_pane_ids();
                            current.sort_unstable();
                            current == *expected_pane_ids
                        })
            }
        };
        if !still_valid {
            self.close_confirm = None;
            self.dirty = true;
        }
    }

    pub(crate) fn remove_pane_from_layout(
        &mut self,
        ws_index: usize,
        pane_id: usize,
    ) -> std::result::Result<(), ipc::CodedError> {
        let ws = &mut self.workspaces[ws_index];
        if !ws.panes.contains_key(&pane_id) {
            return Err(ipc::CodedError::new(
                ipc::err_code::PANE_VANISHED,
                "pane vanished",
            ));
        }
        let pane_ids = ws.layout.collect_pane_ids();
        let current_idx = pane_ids.iter().position(|&id| id == pane_id);

        let exited_meta: Option<(Option<String>, Option<String>)> = {
            let name = ws
                .pane_names
                .iter()
                .find(|(_, id)| **id == pane_id)
                .map(|(n, _)| n.clone());
            match ws.panes.get_mut(&pane_id) {
                Some(pane) if !pane.exit_event_emitted => {
                    pane.exit_event_emitted = true;
                    Some((name, pane.role.clone()))
                }
                _ => None,
            }
        };

        ws.layout.remove_pane(pane_id);

        if let Some(mut pane) = ws.panes.remove(&pane_id) {
            pane.kill();
        }

        ws.pane_names.retain(|_, id| *id != pane_id);
        self.drop_overlay_for_pane(pane_id);
        self.claude_monitor.remove(pane_id);
        self.peer_client_kinds.remove(&pane_id);
        self.pending_codex_peer_messages.remove(&pane_id);
        if self
            .codex_peer_notification
            .as_ref()
            .is_some_and(|n| n.target_pane == pane_id)
        {
            self.codex_peer_notification = None;
        }

        let ws = &mut self.workspaces[ws_index];
        let remaining_ids = ws.layout.collect_pane_ids();
        if ws.focused_pane_id == pane_id {
            if let Some(idx) = current_idx {
                let new_idx = if idx >= remaining_ids.len() {
                    remaining_ids.len().saturating_sub(1)
                } else {
                    idx
                };
                if let Some(&next) = remaining_ids.get(new_idx) {
                    ws.focused_pane_id = next;
                }
            } else if let Some(&first) = remaining_ids.first() {
                ws.focused_pane_id = first;
            }
        }

        if ws_index == self.active_tab {
            self.mark_layout_change();
        } else {
            self.dirty = true;
        }
        // An MCP close that removed the confirmation's target — or that
        // left a pane-close target as its tab's last pane — expires the
        // prompt rather than silently retargeting it.
        self.revalidate_close_confirm();
        if let Some((name, role)) = exited_meta {
            self.emit_pane_exited(pane_id, name, role);
        }
        Ok(())
    }

    pub(crate) fn handle_close(
        &mut self,
        target: &PaneRef,
    ) -> std::result::Result<usize, ipc::CodedError> {
        let (ws_index, pane_id) = self.resolve_pane_across_workspaces(target).ok_or_else(|| {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        })?;

        let is_only_pane = self.workspaces[ws_index].layout.pane_count() <= 1;
        if is_only_pane {
            if self.workspaces.len() <= 1 {
                return Err(ipc::CodedError::new(
                    ipc::err_code::LAST_PANE,
                    "cannot close the last pane of the only tab",
                ));
            }
            self.close_tab(ws_index);
            return Ok(pane_id);
        }

        self.remove_pane_from_layout(ws_index, pane_id)?;
        Ok(pane_id)
    }

    /// Which workspace an IPC request should be resolved against.
    ///
    /// `None` — no `from_pane` on the wire — is the legacy contract the
    /// `renga` CLI still relies on: "the tab the user is looking at".
    /// `Some(id)` is the caller's own pane, so the answer is the tab
    /// that *owns* that pane, which may well not be the visible one
    /// (Issue #288: a background agent's `send_keys` must not land in
    /// whatever tab the human happens to have switched to).
    ///
    /// A `from_pane` that no longer exists is an error, never a
    /// fallback: the caller's pane vanishing mid-call is precisely when
    /// guessing a workspace does the most damage.
    pub(crate) fn resolve_caller_workspace(
        &self,
        from_pane: Option<usize>,
    ) -> std::result::Result<usize, ipc::CodedError> {
        match from_pane {
            None => Ok(self.active_tab),
            Some(id) => self.workspace_index_of_pane(id).ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("caller pane {id} not found in any workspace"),
                )
            }),
        }
    }

    /// Resolve `target` for a caller sitting in workspace `ws_idx`.
    ///
    /// - `Focused` / `Name` stay inside `ws_idx`. Both are *relative*
    ///   addresses — "the focused pane", "the pane called worker" — and
    ///   a relative address that silently crosses into another tab is
    ///   the #288 bug in miniature. `Name` in particular is only unique
    ///   per tab, so a global search would be ambiguous by design.
    /// - `Id` searches every workspace. Numeric ids are globally unique
    ///   and unambiguous, so naming one is an explicit cross-tab
    ///   request — the same escape hatch [`Self::handle_close`] has
    ///   offered since it moved to `resolve_pane_across_workspaces`.
    pub(crate) fn resolve_target_from(
        &self,
        ws_idx: usize,
        target: &PaneRef,
    ) -> Option<(usize, usize)> {
        match target {
            PaneRef::Id(id) => self
                .workspaces
                .iter()
                .enumerate()
                .find(|(_, ws)| ws.panes.contains_key(id))
                .map(|(i, _)| (i, *id)),
            PaneRef::Focused | PaneRef::Name(_) => {
                let ws = self.workspaces.get(ws_idx)?;
                ws.resolve_pane_ref(target).map(|id| (ws_idx, id))
            }
        }
    }

    /// The one entry point every pane-targeting IPC handler uses, so
    /// the caller-scope rules cannot drift apart between `send_keys`,
    /// `inspect_pane`, `focus_pane` and the three `spawn_*` tools.
    ///
    /// Note the ordering: the caller is resolved *first* and its
    /// failure is fatal, even for an explicit `Id` target that would
    /// have resolved on its own. A request whose `from_pane` is bogus
    /// is a request we cannot attribute, and attributing it to the
    /// visible tab is the failure mode being fixed here.
    pub(crate) fn resolve_request_target(
        &self,
        from_pane: Option<usize>,
        target: &PaneRef,
    ) -> std::result::Result<(usize, usize), ipc::CodedError> {
        let ws_idx = self.resolve_caller_workspace(from_pane)?;
        let not_found = || {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        };
        match from_pane {
            // Legacy: everything — including `Id` — resolves inside the
            // active tab, exactly as before #288. Widening `Id` here
            // would be a silent semantic change for `renga send --id`.
            None => self.workspaces[ws_idx]
                .resolve_pane_ref(target)
                .map(|id| (ws_idx, id))
                .ok_or_else(not_found),
            Some(_) => self
                .resolve_target_from(ws_idx, target)
                .ok_or_else(not_found),
        }
    }

    pub(crate) fn resolve_pane_across_workspaces(
        &self,
        target: &PaneRef,
    ) -> Option<(usize, usize)> {
        match target {
            PaneRef::Focused => {
                let ws = self.ws();
                if ws.panes.contains_key(&ws.focused_pane_id) {
                    Some((self.active_tab, ws.focused_pane_id))
                } else {
                    None
                }
            }
            PaneRef::Id(id) => self
                .workspaces
                .iter()
                .enumerate()
                .find(|(_, ws)| ws.panes.contains_key(id))
                .map(|(i, _)| (i, *id)),
            PaneRef::Name(name) => {
                let ordered: Vec<usize> = std::iter::once(self.active_tab)
                    .chain((0..self.workspaces.len()).filter(|i| *i != self.active_tab))
                    .collect();
                for i in ordered {
                    let ws = &self.workspaces[i];
                    if let Some(&id) = ws.pane_names.get(name) {
                        if ws.panes.contains_key(&id) {
                            return Some((i, id));
                        }
                    }
                }
                None
            }
        }
    }

    /// The non-pane focus stops `Ctrl+Right` visits after it runs out
    /// of panes, in cycle order, filtered to the ones currently on
    /// screen.
    ///
    /// Extracted when the org sidebar added a fourth [`FocusTarget`]:
    /// the previous hand-written if-chains needed one more branch in
    /// each of six places, and the two directions had already drifted
    /// into subtly different shapes. Order is
    /// `panes → file tree → preview → org sidebar → panes`; the sidebar
    /// goes last so the pre-existing `Ctrl+Right` muscle memory
    /// (pane → tree → preview) is untouched.
    fn focus_cycle_targets(&self) -> Vec<FocusTarget> {
        // Membership is decided by the *resolved layout*, not by the
        // logical visibility flags. A panel can be flagged on and still
        // be nowhere on screen — `replace` mode takes the tree's slot,
        // and the degrade ladder drops the preview, then the tree, on a
        // narrow terminal. Cycling onto one of those swallows every
        // later keystroke, and for the file tree a bare `c` / `v` would
        // split the workspace into a Claude pane nobody asked for. The
        // sidebar ships on by default, so it eats columns the other two
        // used to have and makes that degrade case routine rather than
        // an edge case.
        let layout = self.main_area_layout();
        let mut targets = Vec::new();
        if layout.file_tree.is_some() {
            targets.push(FocusTarget::FileTree);
        }
        if layout.preview.is_some() {
            targets.push(FocusTarget::Preview);
        }
        if layout.org_sidebar.is_some() {
            targets.push(FocusTarget::OrgSidebar);
        }
        targets
    }

    pub(crate) fn focus_next_pane(&mut self) {
        let cycle = self.focus_cycle_targets();
        let ws = self.ws_mut();
        let ids = ws.layout.collect_pane_ids();

        if ws.focus_target == FocusTarget::Pane {
            let Some(idx) = ids.iter().position(|&id| id == ws.focused_pane_id) else {
                return;
            };
            if idx + 1 < ids.len() {
                ws.focused_pane_id = ids[idx + 1];
            } else if let Some(&first) = cycle.first() {
                ws.focus_target = first;
            } else {
                ws.focused_pane_id = ids[0];
            }
            return;
        }

        // A panel can vanish while it holds focus (toggled off, or
        // squeezed out by a narrow terminal), in which case it is no
        // longer in the cycle and focus falls back to the panes.
        match cycle.iter().position(|&t| t == ws.focus_target) {
            Some(pos) if pos + 1 < cycle.len() => ws.focus_target = cycle[pos + 1],
            _ => ws.focus_target = FocusTarget::Pane,
        }
    }

    pub(crate) fn focus_prev_pane(&mut self) {
        let cycle = self.focus_cycle_targets();
        let ws = self.ws_mut();
        let ids = ws.layout.collect_pane_ids();

        if ws.focus_target == FocusTarget::Pane {
            let Some(idx) = ids.iter().position(|&id| id == ws.focused_pane_id) else {
                return;
            };
            if idx > 0 {
                ws.focused_pane_id = ids[idx - 1];
            } else if let Some(&last) = cycle.last() {
                ws.focus_target = last;
            } else {
                ws.focused_pane_id = ids[ids.len() - 1];
            }
            return;
        }

        match cycle.iter().position(|&t| t == ws.focus_target) {
            Some(pos) if pos > 0 => ws.focus_target = cycle[pos - 1],
            _ => {
                ws.focus_target = FocusTarget::Pane;
                if let Some(&last) = ids.last() {
                    ws.focused_pane_id = last;
                }
            }
        }
    }

    /// The one supported way to change the active tab.
    ///
    /// Tab switching used to be `self.active_tab = n` copy-pasted
    /// across four call sites, each remembering a different subset of
    /// the bookkeeping: the keyboard paths suspended the IME overlay
    /// unconditionally (even when switching to the tab already active),
    /// the mouse path was the only one clearing the double-click
    /// caches, and none of them dropped the stale text selection. The
    /// org sidebar adds a fifth caller and a piece of state that
    /// *must* survive the switch, so the whole sequence lives here.
    ///
    /// Returns `true` when the active tab actually changed.
    pub(crate) fn switch_tab(&mut self, index: usize) -> bool {
        // `ws()` indexes `workspaces` directly and would panic. The
        // sidebar renders from a snapshot that an MCP `close_tab` can
        // invalidate between paint and click, so this is a live guard,
        // not a formality.
        if index >= self.workspaces.len() || index == self.active_tab {
            return false;
        }
        self.suspend_overlay();
        // Focus is per-workspace, so the incoming tab would otherwise
        // restore whatever it was focused on last — knocking the user
        // out of the sidebar they are currently driving.
        let keep_sidebar_focus = self.ws().focus_target == FocusTarget::OrgSidebar;
        self.active_tab = index;
        if keep_sidebar_focus {
            self.ws_mut().focus_target = FocusTarget::OrgSidebar;
        }
        // Every one of these is keyed by tab index or by a pane that
        // belongs to the tab we just left.
        self.last_tab_click = None;
        self.last_edge_click = None;
        self.last_boundary_click = None;
        self.selection = None;
        self.dirty = true;
        true
    }
}

/// Walk a layout subtree and return the cwd that should be used for
/// the pane hosting that subtree's root position. For a bare `Pane`
/// leaf that's simply its own cwd; for a `Split` we inherit from the
/// `first` child so the parent pane (already present before the split)
/// and the subtree agree. Relative paths are joined onto `base`. A
/// `None` result means "use default" (inherit from parent pane or
/// process cwd).
pub(crate) fn subtree_root_cwd(node: &LayoutNodeSpec, base: &std::path::Path) -> Option<PathBuf> {
    match node {
        LayoutNodeSpec::Pane { cwd, .. } => cwd.as_deref().and_then(|s| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                let p = PathBuf::from(t);
                Some(if p.is_absolute() { p } else { base.join(p) })
            }
        }),
        LayoutNodeSpec::Split { first, .. } => subtree_root_cwd(first, base),
    }
}

/// Pre-flight check: walk the layout tree and fail fast if any `cwd`
/// field doesn't point at an existing directory. Keeps the mutation
/// semantics consistent with `Request::Split` / `Request::NewTab` —
/// bad cwd = no partial layout.
pub(crate) fn validate_layout_cwds(node: &LayoutNodeSpec, base: &std::path::Path) -> Result<()> {
    match node {
        LayoutNodeSpec::Pane { cwd, id, .. } => {
            if let Some(raw) = cwd {
                let t = raw.trim();
                if !t.is_empty() {
                    let p = PathBuf::from(t);
                    let joined = if p.is_absolute() { p } else { base.join(p) };
                    let meta = std::fs::metadata(&joined).map_err(|e| {
                        anyhow::anyhow!(
                            "layout pane '{id}' cwd {} is not accessible: {e}",
                            joined.display()
                        )
                    })?;
                    if !meta.is_dir() {
                        return Err(anyhow::anyhow!(
                            "layout pane '{id}' cwd {} is not a directory",
                            joined.display()
                        ));
                    }
                }
            }
            Ok(())
        }
        LayoutNodeSpec::Split { first, second, .. } => {
            validate_layout_cwds(first, base)?;
            validate_layout_cwds(second, base)?;
            Ok(())
        }
    }
}

/// Resolve an optional `cwd` string from an IPC Split / NewTab request
/// into an absolute `PathBuf`. Relative paths are joined onto `base`
/// (the target pane's cwd for Split, the server's process cwd for
/// NewTab). Missing / non-directory paths surface as
/// [`ipc::err_code::CWD_INVALID`] so the caller can distinguish them
/// from other failure codes. Returns `Ok(None)` when the caller did
/// not supply a cwd (including empty / whitespace) — preserving the
/// pre-cwd default-inheritance behavior.
pub(crate) fn resolve_optional_cwd(
    cwd: Option<&str>,
    base: &std::path::Path,
) -> std::result::Result<Option<PathBuf>, ipc::CodedError> {
    let raw = match cwd {
        Some(s) => s.trim(),
        None => return Ok(None),
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let candidate = PathBuf::from(raw);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        base.join(candidate)
    };
    // Canonicalize so the stored cwd is stable and follows `..` /
    // symlinks deterministically. Canonicalize also implicitly
    // verifies existence — a missing path errors out here.
    let canon = std::fs::canonicalize(&joined).map_err(|e| {
        ipc::CodedError::new(
            ipc::err_code::CWD_INVALID,
            format!("cwd {} is not accessible: {e}", joined.display()),
        )
    })?;
    // Directory check goes against the canonical (verbatim-prefixed on
    // Windows) path, not the stripped one. Long paths / UNC shares can
    // fail `is_dir()` once the `\\?\` prefix is removed, so we verify
    // first and then strip purely for display/storage.
    if !canon.is_dir() {
        return Err(ipc::CodedError::new(
            ipc::err_code::CWD_INVALID,
            format!("cwd {} is not a directory", canon.display()),
        ));
    }
    // Windows' canonicalize returns a `\\?\C:\...` verbatim path,
    // which leaks into `PaneInfo.cwd` / MCP list output and looks
    // wrong in user-facing tooling. Strip the prefix for storage so
    // the PTY cwd string matches what a shell would show.
    Ok(Some(strip_verbatim_prefix(canon)))
}

/// Strip Windows `\\?\` (verbatim) and `\\?\UNC\` prefixes from a
/// canonicalized path. On non-Windows this is an identity function.
/// Kept as a free function so both the IPC cwd resolver and any
/// future code paths that serialize canonicalized paths can share one
/// definition. Prefers string manipulation over `dunce` so we don't
/// add a dependency for a few lines of path-prefix cleanup.
pub(crate) fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy().into_owned();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            // `\\?\UNC\server\share\...` → `\\server\share\...`
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
        PathBuf::from(s)
    }
    #[cfg(not(windows))]
    {
        p
    }
}

pub(crate) fn default_command_for_role(role: Option<&str>) -> Option<String> {
    match role {
        Some("claude") => Some(CLAUDE_PEER_LAUNCH_CMD.to_string()),
        _ => None,
    }
}

/// Extract directory name from a path for tab title.
pub(crate) fn dir_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}
