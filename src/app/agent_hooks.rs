//! Agent lifecycle-hook state (`renga-cp copilot-hook`).
//!
//! An agent that has renga's hooks installed runs `renga-cp
//! copilot-hook <event>` on each lifecycle event, which lands here as
//! [`ipc::Request::AgentHook`]. What that gives renga is the one fact
//! the screen can only guess at: whether the agent itself believes a
//! turn is running.
//!
//! The state is advisory in one direction only. It may turn a screen
//! renga would have written to into a refusal (a composer that looks
//! idle while the agent says it is working), and it is surfaced to
//! orchestrators through `list_panes` / `list_peers`. It never
//! authorizes a write on its own: hooks are optional, asynchronous and
//! fire-and-forget, so a missing report proves nothing.
//!
//! Only Copilot reports, so only a pane whose agent is Copilot reads
//! its record ([`App::agent_activity`]); after a Copilot crash the same
//! pane may be running Claude or a shell, and neither of those should
//! inherit a Copilot turn.
//!
//! A `working` or `blocked` report also has to be able to *end* without
//! a hook. Copilot fires nothing when a human cancels a turn (measured:
//! Ctrl+C prints "Operation cancelled by user" and no `agentStop`
//! follows), or when a permission prompt is dismissed. Left alone, one
//! cancel would pin the pane at `working` until a human typed again.
//! [`App::settle_agent_hook_states`] therefore demotes either state to
//! `idle` once the screen has read as an idle composer for
//! [`SCREEN_IDLE_GRACE`] without a break. During a real turn that does
//! not fire as long as renga recognizes Copilot's busy footer (`esc
//! interrupt`, matched on the bare word so it survives the footer's
//! narrow-pane reflow), which Copilot shows for the whole of a turn. If a
//! future Copilot changed that footer, the demotion would drop the veto
//! mid-turn — back to screen-only readiness, never to something worse.
//!
//! A record belongs to one run of the agent. It is keyed to the pane's
//! [`Pane::screen_epoch`] at the time it was written, and reads as
//! absent once that epoch moves — the agent left the alternate screen
//! or a new one entered it. Without that, a Copilot killed mid-turn
//! would leave `working` behind for its successor in the same pane,
//! whose first prompt could then never be delivered: `sessionStart`
//! fires only after the first prompt, so nothing from the new run
//! would ever overwrite it.

use super::user_turn::{turn_readiness_on_screen, TurnAgent, TurnReadiness};
use super::*;
use crate::ipc::{AgentActivity, AgentHookEffect};

/// How long the screen must read as an idle Copilot composer, without a
/// break, before a `working` / `blocked` report is taken to be over.
///
/// The veto exists for the moments the screen cannot yet show a turn —
/// the frames between Enter and the busy footer painting — and those are
/// sub-second. Three seconds of an uninterrupted idle composer is past
/// any of them, and still short enough that a cancelled turn frees the
/// pane before an orchestrator's retry loop gives up on it.
pub(crate) const SCREEN_IDLE_GRACE: Duration = Duration::from_secs(3);

/// One pane's last hook report, pinned to the agent run it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentHookRecord {
    pub(crate) activity: AgentActivity,
    /// [`Pane::screen_epoch`] when the report arrived.
    pub(crate) screen_epoch: u64,
    /// Start of the current unbroken run of idle-composer frames seen
    /// while `activity` is `working` / `blocked`; see
    /// [`settle_record`].
    pub(crate) screen_idle_since: Option<Instant>,
}

impl AgentHookRecord {
    pub(crate) fn new(activity: AgentActivity, screen_epoch: u64) -> Self {
        Self {
            activity,
            screen_epoch,
            screen_idle_since: None,
        }
    }
}

/// Advance one record by one look at its pane's screen: demote a
/// `working` / `blocked` report to `idle` once `screen_ready` has held
/// for [`SCREEN_IDLE_GRACE`] without a break. Returns whether the
/// visible activity changed.
pub(crate) fn settle_record(rec: &mut AgentHookRecord, screen_ready: bool, now: Instant) -> bool {
    if rec.activity == AgentActivity::Idle {
        rec.screen_idle_since = None;
        return false;
    }
    if !screen_ready {
        rec.screen_idle_since = None;
        return false;
    }
    let since = *rec.screen_idle_since.get_or_insert(now);
    if now.duration_since(since) >= SCREEN_IDLE_GRACE {
        rec.activity = AgentActivity::Idle;
        rec.screen_idle_since = None;
        return true;
    }
    false
}

impl App {
    pub(crate) fn handle_agent_hook(
        &mut self,
        pane_id: usize,
        kind: PeerClientKind,
        event: &str,
        notification_type: Option<&str>,
        recoverable: Option<bool>,
    ) -> std::result::Result<(), ipc::CodedError> {
        let (ws_idx, _) = self
            .resolve_pane_across_workspaces(&PaneRef::Id(pane_id))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("pane {pane_id} not found for agent hook"),
                )
            })?;
        let screen_epoch = self.workspaces[ws_idx]
            .panes
            .get(&pane_id)
            .map(|p| p.screen_epoch())
            .unwrap_or_default();
        match AgentActivity::from_hook(kind, event, notification_type, recoverable) {
            AgentHookEffect::Set(activity) => {
                self.agent_hook_states
                    .insert(pane_id, AgentHookRecord::new(activity, screen_epoch));
                self.dirty = true;
            }
            AgentHookEffect::Clear => {
                if self.agent_hook_states.remove(&pane_id).is_some() {
                    self.dirty = true;
                }
            }
            AgentHookEffect::Ignore => {}
        }
        Ok(())
    }

    /// The hook-reported activity of the agent currently running in
    /// `pane`, or `None` when no hook has reported for *this* run of it
    /// or the pane's agent is not Copilot.
    pub(crate) fn agent_activity(&self, pane: &Pane) -> Option<AgentActivity> {
        if !self.pane_runs_copilot(pane) {
            return None;
        }
        agent_activity_of(self.agent_hook_states.get(&pane.id), pane.screen_epoch())
    }

    /// Whether renga takes `pane`'s agent to be Copilot — the same
    /// precedence as [`App::user_turn_agent`]: registration first, then
    /// the live window title.
    fn pane_runs_copilot(&self, pane: &Pane) -> bool {
        match self.peer_client_kinds.get(&pane.id) {
            Some(kind) => *kind == PeerClientKind::Copilot,
            None => !pane.is_codex_running() && pane.is_copilot_running(),
        }
    }

    /// Per-frame upkeep of [`App::agent_hook_states`]: drop records
    /// that no longer describe the pane's agent run, and demote
    /// `working` / `blocked` reports the screen has outlived (see the
    /// module docs).
    pub(crate) fn settle_agent_hook_states(&mut self) {
        if self.agent_hook_states.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut stale = Vec::new();
        let mut changed = false;
        let ids: Vec<usize> = self.agent_hook_states.keys().copied().collect();
        for pane_id in ids {
            let Some(pane) = self.workspaces.iter().find_map(|ws| ws.panes.get(&pane_id)) else {
                stale.push(pane_id);
                continue;
            };
            let epoch = pane.screen_epoch();
            let runs_copilot = self.pane_runs_copilot(pane);
            let Some(rec) = self.agent_hook_states.get(&pane_id).copied() else {
                continue;
            };
            if rec.screen_epoch != epoch || !runs_copilot {
                stale.push(pane_id);
                continue;
            }
            if rec.activity == AgentActivity::Idle {
                continue;
            }
            let screen_ready = !pane.is_scrolled_back()
                && pane.parser.lock().is_ok_and(|parser| {
                    turn_readiness_on_screen(TurnAgent::Copilot, parser.screen())
                        == TurnReadiness::Ready
                });
            if let Some(rec) = self.agent_hook_states.get_mut(&pane_id) {
                changed |= settle_record(rec, screen_ready, now);
            }
        }
        for pane_id in stale {
            self.agent_hook_states.remove(&pane_id);
        }
        if changed {
            self.dirty = true;
        }
    }

    /// [`Self::agent_activity`] for a pane that may already be gone.
    pub(crate) fn agent_activity_of_pane(&self, pane: Option<&Pane>) -> Option<AgentActivity> {
        pane.and_then(|p| self.agent_activity(p))
    }
}

/// Pure core of [`App::agent_activity`], split out for tests.
pub(crate) fn agent_activity_of(
    record: Option<&AgentHookRecord>,
    current_epoch: u64,
) -> Option<AgentActivity> {
    record
        .filter(|r| r.screen_epoch == current_epoch)
        .map(|r| r.activity)
}
