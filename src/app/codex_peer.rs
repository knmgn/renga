use super::user_turn::{turn_readiness_on_screen, TurnAgent, TurnReadiness};
use super::*;

pub(crate) const CODEX_APPEND_ENTER_DELAY: Duration = Duration::from_millis(75);
pub(crate) const CODEX_PEER_NUDGE_SUBMIT_DELAY: Duration = Duration::from_millis(1000);
pub(crate) const CODEX_APPEND_ENTER_SNAPSHOT_LINES: usize = 8;

/// Window during which a `(target, from, body)` triple is treated as
/// a re-send and dropped before reaching `Event::PeerInbox`. Set to a
/// small handful of seconds so legitimate retries after the
/// receiver's reply still get through, but a dispatcher / worker
/// that fires the exact same payload twice in quick succession
/// can't double-paper the transcript with phantom user turns. See
/// renga#221 acceptance criterion #2.
pub(crate) const PEER_SEND_DEDUPE_TTL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCodexPeerMessage {
    pub(crate) from_pane: usize,
    pub(crate) from_name: Option<String>,
    pub(crate) from_kind: Option<PeerClientKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPeerNotificationState {
    pub(crate) target_pane: usize,
    pub(crate) message: PendingCodexPeerMessage,
    pub(crate) pending_count: usize,
}

impl CodexPeerNotificationState {
    fn register_message(&mut self, message: PendingCodexPeerMessage) {
        self.message = message;
        self.pending_count = self.pending_count.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingCodexPeerDelivery {
    Draft(PendingCodexPeerMessage),
    SubmitAt(Instant),
}

pub(crate) fn screen_tail_lines(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let (cursor_row, _) = screen.cursor_position();
    let mut last_content_row = None;
    for row in 0..rows {
        let mut has_text = false;
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    has_text = true;
                    break;
                }
            }
        }
        if has_text {
            last_content_row = Some(row);
        }
    }
    let end_row = last_content_row.unwrap_or(cursor_row).max(cursor_row);
    let start_row = end_row
        .saturating_add(1)
        .saturating_sub(CODEX_APPEND_ENTER_SNAPSHOT_LINES as u16);
    let mut lines =
        Vec::with_capacity(end_row.saturating_sub(start_row).saturating_add(1) as usize);
    for row in start_row..=end_row {
        let mut line = String::with_capacity(cols as usize);
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

fn pane_screen_tail_lines(pane: &Pane) -> Option<Vec<String>> {
    let parser = pane.parser.lock().ok()?;
    Some(screen_tail_lines(parser.screen()))
}

pub(crate) fn screen_has_visible_text(screen: &vt100::Screen) -> bool {
    let (rows, cols) = screen.size();
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    return true;
                }
            }
        }
    }
    false
}

fn pane_screen_has_visible_text(pane: &Pane) -> bool {
    let Ok(parser) = pane.parser.lock() else {
        return false;
    };
    screen_has_visible_text(parser.screen())
}

pub(crate) fn codex_prompt_allows_peer_nudge_on_screen(screen: &vt100::Screen) -> Option<bool> {
    if screen.hide_cursor() {
        return Some(false);
    }
    let (rows, cols) = screen.size();
    let mut last_content_row = None;
    for row in 0..rows {
        let mut has_text = false;
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    has_text = true;
                    break;
                }
            }
        }
        if has_text {
            last_content_row = Some(row);
        }
    }
    let mut prompt_row = None;
    let (cursor_row, cursor_col) = screen.cursor_position();
    let end_row = last_content_row.unwrap_or(cursor_row).max(cursor_row);
    let start_row = end_row
        .saturating_add(1)
        .saturating_sub(CODEX_APPEND_ENTER_SNAPSHOT_LINES as u16);
    for row in (start_row..=end_row).rev() {
        let mut line = String::with_capacity(cols as usize);
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }
        if line.trim_start().starts_with('›') {
            prompt_row = Some(row);
            break;
        }
    }
    let prompt_row = prompt_row?;
    if cursor_row > prompt_row {
        return Some(false);
    }
    if cursor_row == prompt_row && cursor_col > 2 {
        return Some(false);
    }
    Some(true)
}

fn codex_prompt_allows_peer_nudge(pane: &Pane) -> Option<bool> {
    let Ok(parser) = pane.parser.lock() else {
        return None;
    };
    codex_prompt_allows_peer_nudge_on_screen(parser.screen())
}

fn codex_peer_screen_tail(pane: &Pane) -> Option<String> {
    Some(
        pane_screen_tail_lines(pane)?
            .join("\n")
            .to_ascii_lowercase(),
    )
}

fn pending_startup_starts_with(pane: &Pane, prefix: &str) -> bool {
    pane.pending_startup
        .as_ref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .is_some_and(|text| text.trim_start().starts_with(prefix))
}

fn pending_startup_looks_like_codex(pane: &Pane) -> bool {
    pending_startup_starts_with(pane, "codex")
}

/// Startup command for one of the pull-mode clients, before its title
/// has been observed. Copilot CLI launches as bare `copilot`.
fn pending_startup_looks_like_pull_client(pane: &Pane) -> bool {
    pending_startup_looks_like_codex(pane) || pending_startup_starts_with(pane, "copilot")
}

/// Which pull-mode agent owns `pane`, for nudge purposes.
///
/// A free function rather than a method because the flush loop holds a
/// mutable borrow of `self.workspaces` while it calls this, and only
/// disjoint *field* borrows survive that — a `&self` method would take
/// the whole struct and conflict.
fn pull_agent_for(kind: Option<&PeerClientKind>, pane: &Pane) -> Option<TurnAgent> {
    match kind {
        Some(PeerClientKind::Codex) => return Some(TurnAgent::Codex),
        Some(PeerClientKind::Copilot) => return Some(TurnAgent::Copilot),
        // Registration is authoritative: a Claude pane never gets a
        // PTY nudge, whatever its title happens to say right now.
        Some(PeerClientKind::Claude) => return None,
        None => {}
    }
    if pane.is_codex_running() {
        Some(TurnAgent::Codex)
    } else if pane.is_copilot_running() {
        Some(TurnAgent::Copilot)
    } else if pending_startup_looks_like_codex(pane) {
        Some(TurnAgent::Codex)
    } else if pending_startup_starts_with(pane, "copilot") {
        Some(TurnAgent::Copilot)
    } else {
        None
    }
}

pub(crate) fn format_codex_peer_message(msg: &PendingCodexPeerMessage) -> String {
    let mut header = format!("Peer request from id={}", msg.from_pane);
    if let Some(name) = &msg.from_name {
        // This string is typed into the target pane's PTY and followed
        // by Enter, so a control character in the sender's name is a
        // prompt injection into someone else's composer, not a display
        // glitch. `split` / `new_tab` accepted names verbatim before
        // #289 widened delivery to every tab.
        header.push_str(&format!(" name={}", ipc::sanitized_label(name)));
    }
    if let Some(kind) = msg.from_kind {
        header.push_str(&format!(" kind={}", kind.label()));
    }
    let guidance = "Run check_messages now. Treat each returned message as a direct coworker request: do the requested work, and use send_message only when a reply or status update is needed.";
    format!("{header}. {guidance}")
}

pub(crate) fn write_input_to_pane(
    pane: &mut Pane,
    data: &[u8],
    append_enter: bool,
) -> std::result::Result<(), ipc::CodedError> {
    pane.write_input(data)
        .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?;
    if append_enter {
        if !data.is_empty() && (pane.is_codex_running() || pending_startup_looks_like_codex(pane)) {
            std::thread::sleep(CODEX_APPEND_ENTER_DELAY);
        }
        pane.write_input(b"\r")
            .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?;
    }
    Ok(())
}

impl App {
    /// Route `body` from `from_pane` to `target` — in any tab since
    /// Issue #289 dropped the same-tab restriction. Target resolution
    /// is caller-scoped ([`Self::resolve_target_from`]): a numeric id
    /// reaches every tab, while a name or `focused` stays inside the
    /// *sender's* workspace — names are only unique per tab, so
    /// resolving them against the tab the human happens to be viewing
    /// would misroute background-tab senders. An unresolvable target
    /// fails with `pane_not_found` instead of pretending to deliver.
    /// Self-sends loop back to the sender pane: tooling like
    /// claude-org-ja's peer_notify resolves "secretary" from a shell
    /// running inside the secretary pane, and a silent drop there
    /// breaks the notification round-trip (see renga#215).
    pub(crate) fn handle_peer_send(
        &mut self,
        from_pane: usize,
        target: &PaneRef,
        body: String,
    ) -> std::result::Result<(), ipc::CodedError> {
        let (sender_ws, _) = self
            .resolve_pane_across_workspaces(&PaneRef::Id(from_pane))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("sender pane {from_pane} not found"),
                )
            })?;
        let (target_ws, target_id) =
            self.resolve_target_from(sender_ws, target).ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!(
                        "peer target not found: {target:?} (names only resolve inside the \
                         sender's tab; use the numeric pane id from list_peers for other tabs)"
                    ),
                )
            })?;
        if self.is_duplicate_peer_send(target_id, from_pane, &body) {
            // Same (target, from, body) within the dedupe window —
            // treat as a no-op so duplicate dispatcher acks /
            // worker false-fires don't paper the receiver's
            // transcript with phantom Human: turns. The sender
            // gets a successful Ok() reply so it can't probe the
            // dedupe state. (renga#221)
            return Ok(());
        }
        self.materialize_unfocused_codex_peer_notification();
        let from_name = self.workspaces[sender_ws]
            .pane_names
            .iter()
            .find(|(_, id)| **id == from_pane)
            .map(|(n, _)| n.clone());
        let from_kind = self.peer_client_kinds.get(&from_pane).copied();
        if self.pane_expects_pull_peer_delivery(target_ws, target_id) {
            let message = PendingCodexPeerMessage {
                from_pane,
                from_name: from_name.clone(),
                from_kind,
            };
            let target_is_focused = self.active_tab == target_ws
                && self.workspaces[target_ws].focus_target == FocusTarget::Pane
                && self.workspaces[target_ws].focused_pane_id == target_id;
            if target_is_focused {
                self.pending_codex_peer_messages.remove(&target_id);
                match self.codex_peer_notification.as_mut() {
                    Some(notification) if notification.target_pane == target_id => {
                        notification.register_message(message);
                    }
                    _ => {
                        self.codex_peer_notification = Some(CodexPeerNotificationState {
                            target_pane: target_id,
                            message,
                            pending_count: 1,
                        });
                    }
                }
                self.dirty = true;
            } else {
                self.push_pending_codex_peer_nudge(target_id, message);
            }
        }
        self.event_bus.emit(ipc::Event::PeerInbox {
            target_pane: target_id,
            from_pane,
            from_name,
            from_kind,
            body,
            ts_ms: ipc::events::now_ms(),
        });
        Ok(())
    }

    /// Return true when an identical (target, from, body) peer send
    /// arrived within [`PEER_SEND_DEDUPE_TTL`]. A side effect
    /// records the new send so future calls compare against it,
    /// and stale entries (older than the TTL) are evicted on every
    /// call so the map can't grow unbounded under heavy traffic.
    fn is_duplicate_peer_send(&mut self, target: usize, from: usize, body: &str) -> bool {
        let now = Instant::now();
        self.recent_peer_sends
            .retain(|_, ts| now.duration_since(*ts) < PEER_SEND_DEDUPE_TTL);
        let key = (target, from, body.to_string());
        match self.recent_peer_sends.get(&key).copied() {
            Some(prev) if now.duration_since(prev) < PEER_SEND_DEDUPE_TTL => {
                // Refresh the timestamp so a chatty sender keeps
                // getting its retries collapsed instead of slipping
                // a duplicate through right at the TTL boundary.
                self.recent_peer_sends.insert(key, now);
                true
            }
            _ => {
                self.recent_peer_sends.insert(key, now);
                false
            }
        }
    }

    pub(crate) fn handle_peer_register_client(
        &mut self,
        pane_id: usize,
        kind: PeerClientKind,
    ) -> std::result::Result<(), ipc::CodedError> {
        self.resolve_pane_across_workspaces(&PaneRef::Id(pane_id))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("pane {pane_id} not found for peer registration"),
                )
            })?;
        self.peer_client_kinds.insert(pane_id, kind);
        Ok(())
    }

    fn push_pending_codex_peer_nudge(&mut self, pane_id: usize, message: PendingCodexPeerMessage) {
        let queue = self.pending_codex_peer_messages.entry(pane_id).or_default();
        if queue.is_empty() {
            queue.push_back(PendingCodexPeerDelivery::Draft(message));
        }
    }

    pub(crate) fn codex_peer_notification_is_visible(&self) -> bool {
        if self.overlay.is_some() {
            return false;
        }
        let Some(notification) = self.codex_peer_notification.as_ref() else {
            return false;
        };
        self.ws().focus_target == FocusTarget::Pane
            && self.ws().focused_pane_id == notification.target_pane
            && self.ws().panes.contains_key(&notification.target_pane)
    }

    pub(crate) fn visible_codex_peer_notification(&self) -> Option<&CodexPeerNotificationState> {
        self.codex_peer_notification_is_visible()
            .then_some(self.codex_peer_notification.as_ref())
            .flatten()
    }

    pub(crate) fn dismiss_codex_peer_notification(&mut self) {
        if self.codex_peer_notification.take().is_some() {
            self.dirty = true;
        }
    }

    fn materialize_unfocused_codex_peer_notification(&mut self) {
        let Some(notification) = self.codex_peer_notification.clone() else {
            return;
        };
        if self.codex_peer_notification_is_visible() {
            return;
        }
        if self
            .resolve_pane_across_workspaces(&PaneRef::Id(notification.target_pane))
            .is_some()
        {
            self.push_pending_codex_peer_nudge(notification.target_pane, notification.message);
        }
        self.codex_peer_notification = None;
        self.dirty = true;
    }

    pub(crate) fn accept_codex_peer_notification(
        &mut self,
    ) -> std::result::Result<bool, ipc::CodedError> {
        let Some(notification) = self.codex_peer_notification.clone() else {
            return Ok(false);
        };
        if !self.codex_peer_notification_is_visible() {
            return Ok(false);
        }
        let payload = crate::mcp_peer::build_send_keys_payload(
            &format_codex_peer_message(&notification.message),
            None,
            false,
        )
        .expect("codex peer notification payload");
        let pane = self
            .ws_mut()
            .panes
            .get_mut(&notification.target_pane)
            .ok_or_else(|| ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished"))?;
        write_input_to_pane(pane, payload.as_bytes(), false)?;
        self.pending_codex_peer_messages
            .remove(&notification.target_pane);
        self.codex_peer_notification = None;
        self.dirty = true;
        Ok(true)
    }

    /// Whether `pane_id` receives peer mail by *pulling* it — i.e. it
    /// needs a PTY nudge telling it to run `check_messages`, because
    /// no channel push will reach it. True for Codex and Copilot.
    ///
    /// Deliberately keyed on [`PeerClientKind::receive_mode`] rather
    /// than on a per-client list, so a future client's mode is decided
    /// in exactly one place.
    ///
    /// Note this is **not** the same question as "does this pane
    /// behave like Codex for mouse purposes" — see
    /// [`Self::pane_looks_like_codex`], which the pointer code uses and
    /// which must stay Codex-only.
    pub(crate) fn pane_expects_pull_peer_delivery(&self, ws_index: usize, pane_id: usize) -> bool {
        // Registration is authoritative when present. Without this
        // short-circuit a Claude-registered pane whose current OSC
        // title transiently contains the substring "codex" (very
        // common for orchestration workers debugging Codex-related
        // issues) would fall through to the title heuristic and be
        // mis-classified as a pull recipient — see issue #209's
        // discussion of the related #208 regression.
        if let Some(kind) = self.peer_client_kinds.get(&pane_id) {
            return kind.receive_mode() == ipc::PeerReceiveMode::Pull;
        }
        self.workspaces[ws_index]
            .panes
            .get(&pane_id)
            .is_some_and(|pane| {
                pane.is_codex_running()
                    || pane.is_copilot_running()
                    || pending_startup_looks_like_pull_client(pane)
            })
    }

    /// Whether `pane_id` should be treated as a Codex pane for the
    /// mouse-protocol quirks in [`crate::pane`] (alternate-scroll
    /// fallback, transcript-overlay wheel handling).
    ///
    /// Split out of the delivery predicate when Copilot joined: Copilot
    /// pulls its peer mail exactly like Codex, but it renders on the
    /// alternate screen and has none of Codex's main-screen wheel
    /// quirks, so answering "yes" here for it would suppress the arrow
    /// fallback it actually wants.
    pub(crate) fn pane_looks_like_codex(&self, ws_index: usize, pane_id: usize) -> bool {
        if let Some(kind) = self.peer_client_kinds.get(&pane_id) {
            return *kind == PeerClientKind::Codex;
        }
        self.workspaces[ws_index]
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.is_codex_running() || pending_startup_looks_like_codex(pane))
    }

    /// Which client renga should *display* for `pane_id` — pane label,
    /// border accent, org-sidebar row — or `None` for a plain shell.
    ///
    /// Registration wins over the `*_ever_seen()` title latches, for the
    /// same reason it wins in [`Self::pane_expects_pull_peer_delivery`].
    /// The display path was left on the latches alone, and the Copilot
    /// arm made that reliably wrong rather than merely fragile: the
    /// latches are substring matches against a title the agent owns,
    /// every one of these clients rewrites that title to an
    /// LLM-generated summary of the task in flight, and the nudge renga
    /// itself types into a pull-mode worker carries `kind=claude`
    /// whenever the sender is a Claude pane. A Copilot worker driven by
    /// a Claude orchestrator therefore summarizes a task whose text
    /// contains "claude", latches `claude_seen`, and — because the
    /// dispatch tested Claude first — repainted itself as a Claude pane
    /// for the rest of the session: orange border, `claude` label, and
    /// the Claude status suffix reading another session's token counts
    /// out of the shared cwd. Measured on a live pane
    /// (`OSC 0;Run Check Messages Workflow - GitHub Copilot`), not
    /// hypothesized.
    ///
    /// The latches stay the fallback. A client launched without the
    /// renga-peers MCP server never registers, and for it the title is
    /// the only evidence there is.
    pub(crate) fn pane_display_client_kind(
        &self,
        ws_index: usize,
        pane_id: usize,
    ) -> Option<PeerClientKind> {
        if let Some(kind) = self.peer_client_kinds.get(&pane_id) {
            return Some(*kind);
        }
        let pane = self.workspaces.get(ws_index)?.panes.get(&pane_id)?;
        // Copilot before Claude, as in `user_turn_agent_for_pane`: the
        // latches are independent booleans, so a pane can hold more
        // than one, and the more specific title wins.
        if pane.copilot_ever_seen() {
            Some(PeerClientKind::Copilot)
        } else if pane.codex_ever_seen() {
            Some(PeerClientKind::Codex)
        } else if pane.claude_ever_seen() {
            Some(PeerClientKind::Claude)
        } else {
            None
        }
    }

    /// Whether a pull-mode pane is in a state where renga may type a
    /// `check_messages` nudge into its composer.
    ///
    /// Dispatches per agent because the two have very different amounts
    /// of evidence available. Codex gets the historical string-and-glyph
    /// heuristic below; Copilot gets the *strict* user-turn predicate,
    /// which is affordable precisely because Copilot frames its composer
    /// the way Claude does and so can be proven rather than guessed.
    ///
    /// `registered` must say whether the pane's kind came from an MCP
    /// registration rather than from a title or startup-command guess.
    /// Passing a blanket `true` would disarm
    /// [`Self::codex_peer_delivery_ready`]'s opening guard: a pane
    /// matched only by `pending_startup_looks_like_codex` has not run
    /// its startup command yet, so the screen still belongs to the
    /// *shell*, and a shell theme whose prompt char is `›` would satisfy
    /// the remaining glyph check and get the nudge typed into it.
    pub(crate) fn pull_peer_delivery_ready(
        agent: TurnAgent,
        registered: bool,
        pane: &Pane,
    ) -> bool {
        match agent {
            TurnAgent::Codex => Self::codex_peer_delivery_ready(registered, pane),
            TurnAgent::Copilot => {
                let Ok(parser) = pane.parser.lock() else {
                    return false;
                };
                turn_readiness_on_screen(agent, parser.screen()) == TurnReadiness::Ready
            }
            // Claude is push-mode and never reaches the nudge path.
            TurnAgent::Claude => false,
        }
    }

    pub(crate) fn codex_peer_delivery_ready(registered_codex: bool, pane: &Pane) -> bool {
        if !registered_codex && !pane.is_codex_running() {
            return false;
        }
        let Some(tail) = codex_peer_screen_tail(pane) else {
            return false;
        };
        if tail.contains("esc to interrupt") || tail.contains("tab to queue message") {
            return false;
        }
        if !pane_screen_has_visible_text(pane) {
            return false;
        }
        if let Some(allowed) = codex_prompt_allows_peer_nudge(pane) {
            return allowed;
        }
        tail.contains("enter to send") || tail.contains("ready for input")
    }

    pub(crate) fn flush_pending_codex_peer_messages(&mut self) {
        self.materialize_unfocused_codex_peer_notification();
        let now = Instant::now();
        let active_tab = self.active_tab;
        // A user-turn delivery owns that composer until it finishes.
        // This flush runs first in the frame, so typing a nudge into a
        // composer that already holds a body would make the submitted
        // turn the concatenation of the two (Issue #323).
        let user_turn_panes = self.panes_with_user_turn_in_flight();
        let mut empty_panes = Vec::new();
        for (ws_idx, ws) in self.workspaces.iter_mut().enumerate() {
            let pane_ids: Vec<usize> = ws.panes.keys().copied().collect();
            for pane_id in pane_ids {
                if user_turn_panes.contains(&pane_id) {
                    continue;
                }
                // Only the pane the human is actually looking at is
                // exempt from PTY nudges (the focused-pane overlay
                // covers it). A background tab's `focused_pane_id` is
                // just a bookmark — skipping it too would strand
                // cross-tab nudges forever on single-pane tabs, where
                // the only pane is always the workspace-focused one
                // (Issue #289).
                if ws_idx == active_tab && ws.focused_pane_id == pane_id {
                    // A nudge that was queued while the pane was hidden
                    // would otherwise stall for as long as the human
                    // stays on it — no overlay exists because
                    // `handle_peer_send` only creates one when the
                    // target was focused *at send time*.
                    match self
                        .pending_codex_peer_messages
                        .get(&pane_id)
                        .and_then(|q| q.front())
                        .cloned()
                    {
                        // Promote a still-undelivered draft into the
                        // notification overlay (the designed UX for a
                        // focused target); the inverse of
                        // `materialize_unfocused_...`, and only when
                        // the overlay would be immediately visible so
                        // the two conversions cannot fight. If the
                        // overlay is busy elsewhere, stay queued and
                        // retry on a later flush.
                        Some(PendingCodexPeerDelivery::Draft(message))
                            if ws.focus_target == FocusTarget::Pane && self.overlay.is_none() =>
                        {
                            match self.codex_peer_notification.as_mut() {
                                Some(n) if n.target_pane == pane_id => {
                                    n.register_message(message);
                                    self.pending_codex_peer_messages.remove(&pane_id);
                                    self.dirty = true;
                                }
                                None => {
                                    self.pending_codex_peer_messages.remove(&pane_id);
                                    self.codex_peer_notification =
                                        Some(CodexPeerNotificationState {
                                            target_pane: pane_id,
                                            message,
                                            pending_count: 1,
                                        });
                                    self.dirty = true;
                                }
                                Some(_) => {}
                            }
                        }
                        // A half-delivered nudge loses its owner the
                        // moment the human watches the pane: the typed
                        // draft is on screen for them to submit or
                        // edit, and once they can touch the composer a
                        // deferred Enter could submit *their* content,
                        // not ours. Cancel the pending submit instead
                        // of resuming it later (Codex review of #289).
                        Some(PendingCodexPeerDelivery::SubmitAt(_)) => {
                            self.pending_codex_peer_messages.remove(&pane_id);
                            self.dirty = true;
                        }
                        _ => {}
                    }
                    continue;
                }
                let Some(queue) = self.pending_codex_peer_messages.get_mut(&pane_id) else {
                    continue;
                };
                let Some(delivery) = queue.front().cloned() else {
                    empty_panes.push(pane_id);
                    continue;
                };
                if let Some(pane) = ws.panes.get_mut(&pane_id) {
                    match delivery {
                        PendingCodexPeerDelivery::Draft(message) => {
                            let registration = self.peer_client_kinds.get(&pane_id);
                            let Some(agent) = pull_agent_for(registration, pane) else {
                                continue;
                            };
                            if !Self::pull_peer_delivery_ready(agent, registration.is_some(), pane)
                            {
                                continue;
                            }
                            let payload = crate::mcp_peer::build_send_keys_payload(
                                &format_codex_peer_message(&message),
                                None,
                                false,
                            )
                            .expect("codex peer draft payload");
                            if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                queue.pop_front();
                                queue.push_front(PendingCodexPeerDelivery::SubmitAt(
                                    now + CODEX_PEER_NUDGE_SUBMIT_DELAY,
                                ));
                                self.dirty = true;
                            }
                        }
                        PendingCodexPeerDelivery::SubmitAt(ready_at) => {
                            if now < ready_at {
                                continue;
                            }
                            let payload = crate::mcp_peer::build_send_keys_payload("", None, true)
                                .expect("codex peer submit payload");
                            if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                queue.pop_front();
                                self.dirty = true;
                            }
                        }
                    }
                }
                if queue.is_empty() {
                    empty_panes.push(pane_id);
                }
            }
        }
        for pane_id in empty_panes {
            self.pending_codex_peer_messages.remove(&pane_id);
        }
    }
}
