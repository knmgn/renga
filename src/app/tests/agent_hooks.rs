//! Agent lifecycle-hook state (`agent_hooks.rs`): what a report does,
//! how long it stays true, and the one direction it may move a
//! delivery decision.

use super::super::agent_hooks::{agent_activity_of, AgentHookRecord};
use super::super::user_turn::{with_agent_activity, TurnAgent, TurnReadiness};
use super::super::*;
use super::user_turn::{copilot_screen, seed_focused_pane_screen, COPILOT_IDLE_FOOTER};
use crate::ipc::{AgentActivity, AgentHookEffect};

/// An idle, registered Copilot pane: the screen alone says `Ready`.
/// `App::new` sizes the pane at `rows - 5` by `cols - 2`, so this is the
/// 9x40 pane `copilot_screen` is drawn for.
fn app_with_idle_copilot_pane() -> (App, usize) {
    let mut app = App::new(14, 42).expect("App::new");
    let pane_id = seed_focused_pane_screen(&mut app, &copilot_screen(COPILOT_IDLE_FOOTER, ""));
    app.peer_client_kinds
        .insert(pane_id, PeerClientKind::Copilot);
    (app, pane_id)
}

fn hook(app: &mut App, pane_id: usize, event: &str, notification_type: Option<&str>) {
    app.handle_agent_hook(
        pane_id,
        PeerClientKind::Copilot,
        event,
        notification_type,
        None,
    )
    .expect("hook accepted");
}

fn activity(app: &App, pane_id: usize) -> Option<AgentActivity> {
    app.agent_activity(app.ws().panes.get(&pane_id).expect("pane"))
}

// ── mapping ───────────────────────────────────────────────────

/// The sequence a live Copilot 1.0.82 turn produced, in order, with the
/// state renga must hold after each report.
#[test]
fn a_recorded_copilot_turn_maps_to_working_then_idle() {
    let copilot =
        |event, nt, rec| AgentActivity::from_hook(PeerClientKind::Copilot, event, nt, rec);
    use AgentActivity::*;
    use AgentHookEffect::*;
    assert_eq!(copilot("userPromptSubmitted", None, None), Set(Working));
    assert_eq!(copilot("UserPromptSubmit", None, None), Set(Working));
    // Fires after the first prompt, so it cannot mean "ready".
    assert_eq!(copilot("sessionStart", None, None), Ignore);
    assert_eq!(copilot("postToolUse", None, None), Set(Working));
    assert_eq!(copilot("agentStop", None, None), Set(Idle));
    assert_eq!(copilot("Stop", None, None), Set(Idle));
    assert_eq!(copilot("sessionEnd", None, None), Clear);
}

#[test]
fn only_waiting_notifications_mean_blocked() {
    let n = |nt| AgentActivity::from_hook(PeerClientKind::Copilot, "notification", nt, None);
    assert_eq!(
        n(Some("permission_prompt")),
        AgentHookEffect::Set(AgentActivity::Blocked)
    );
    assert_eq!(
        n(Some("elicitation_dialog")),
        AgentHookEffect::Set(AgentActivity::Blocked)
    );
    // Shell and agent completion notices are not waits.
    assert_eq!(n(Some("shell_completed")), AgentHookEffect::Ignore);
    assert_eq!(n(None), AgentHookEffect::Ignore);
}

#[test]
fn only_an_unrecoverable_error_ends_the_turn() {
    let e = |rec| AgentActivity::from_hook(PeerClientKind::Copilot, "errorOccurred", None, rec);
    assert_eq!(e(Some(true)), AgentHookEffect::Ignore);
    assert_eq!(e(Some(false)), AgentHookEffect::Set(AgentActivity::Idle));
    assert_eq!(e(None), AgentHookEffect::Set(AgentActivity::Idle));
}

/// Event names are only defined for the client renga installs hooks
/// into; the same name from another client is not guessed at.
#[test]
fn reports_from_other_clients_are_ignored() {
    for kind in [PeerClientKind::Claude, PeerClientKind::Codex] {
        assert_eq!(
            AgentActivity::from_hook(kind, "userPromptSubmitted", None, None),
            AgentHookEffect::Ignore
        );
    }
}

// ── lifetime ──────────────────────────────────────────────────

#[test]
fn a_record_is_only_true_for_the_screen_epoch_it_was_written_in() {
    let rec = AgentHookRecord::new(AgentActivity::Working, 3);
    assert_eq!(
        agent_activity_of(Some(&rec), 3),
        Some(AgentActivity::Working)
    );
    assert_eq!(agent_activity_of(Some(&rec), 4), None);
    assert_eq!(agent_activity_of(None, 3), None);
}

/// The failure the epoch exists for: a Copilot killed mid-turn leaves
/// `working` behind, and its successor in the same pane reports nothing
/// until *after* its first prompt — so without invalidation, that first
/// prompt could never be delivered.
#[test]
fn a_relaunched_agent_does_not_inherit_its_predecessors_working_state() {
    let (mut app, pane_id) = app_with_idle_copilot_pane();
    hook(&mut app, pane_id, "userPromptSubmitted", None);
    assert_eq!(activity(&app, pane_id), Some(AgentActivity::Working));
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Busy);

    app.ws()
        .panes
        .get(&pane_id)
        .expect("pane")
        .bump_screen_epoch_for_test();
    assert_eq!(activity(&app, pane_id), None);
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);
    app.shutdown();
}

#[test]
fn session_end_forgets_the_state() {
    let (mut app, pane_id) = app_with_idle_copilot_pane();
    hook(&mut app, pane_id, "userPromptSubmitted", None);
    hook(&mut app, pane_id, "sessionEnd", None);
    assert_eq!(activity(&app, pane_id), None);
    assert!(!app.agent_hook_states.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn a_hook_for_an_unknown_pane_is_refused() {
    let (mut app, _) = app_with_idle_copilot_pane();
    let err = app
        .handle_agent_hook(9_999, PeerClientKind::Copilot, "agentStop", None, None)
        .expect_err("no such pane");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

// ── effect on deliveries ─────────────────────────────────────

/// The hook may only ever take a delivery away.
#[test]
fn hook_state_can_refuse_a_ready_screen_but_never_approve_one() {
    use AgentActivity::*;
    use TurnReadiness::*;
    assert_eq!(with_agent_activity(Ready, Some(Working)), Busy);
    assert_eq!(with_agent_activity(Ready, Some(Blocked)), NotReady);
    assert_eq!(with_agent_activity(Ready, Some(Idle)), Ready);
    assert_eq!(with_agent_activity(Ready, None), Ready);
    for screen in [Busy, NotReady, Unsupported] {
        for act in [Some(Working), Some(Blocked), Some(Idle), None] {
            assert_eq!(
                with_agent_activity(screen, act),
                screen,
                "{act:?} must not change a {screen:?} screen"
            );
        }
    }
}

/// The screen reads idle — Copilot keeps an empty framed composer up
/// for the whole turn — but the agent says it is working.
#[test]
fn a_working_report_holds_user_turns_and_nudges_on_an_idle_looking_screen() {
    let (mut app, pane_id) = app_with_idle_copilot_pane();
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);

    hook(&mut app, pane_id, "userPromptSubmitted", None);
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Busy);
    let pane = app.ws().panes.get(&pane_id).expect("pane");
    assert!(!App::pull_peer_delivery_ready(
        TurnAgent::Copilot,
        true,
        pane,
        app.agent_activity(pane)
    ));

    hook(&mut app, pane_id, "notification", Some("permission_prompt"));
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::NotReady);

    hook(&mut app, pane_id, "postToolUse", None);
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Busy);

    hook(&mut app, pane_id, "agentStop", None);
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);
    let pane = app.ws().panes.get(&pane_id).expect("pane");
    assert!(App::pull_peer_delivery_ready(
        TurnAgent::Copilot,
        true,
        pane,
        app.agent_activity(pane)
    ));
    app.shutdown();
}

#[test]
fn listings_surface_the_hook_state() {
    let (mut app, pane_id) = app_with_idle_copilot_pane();
    let state_in_list = |app: &App| {
        app.handle_list(Some(pane_id), None)
            .expect("list")
            .into_iter()
            .find(|p| p.id == pane_id)
            .expect("pane listed")
            .agent_state
    };
    assert_eq!(state_in_list(&app), None);
    hook(&mut app, pane_id, "agentStop", None);
    assert_eq!(state_in_list(&app), Some(AgentActivity::Idle));
    app.shutdown();
}

#[test]
fn closing_a_pane_drops_its_record() {
    let mut app = App::new(40, 120).expect("App::new");
    let second = app
        .split_focused_pane(SplitDirection::Vertical, None)
        .expect("split")
        .expect("new pane");
    hook(&mut app, second, "agentStop", None);
    assert!(app.agent_hook_states.contains_key(&second));
    app.handle_close(&ipc::PaneRef::Id(second), None)
        .expect("close");
    assert!(!app.agent_hook_states.contains_key(&second));
    app.shutdown();
}

// ── recovery without a hook ───────────────────────────────────

/// Copilot fires nothing when a human cancels a turn, so `working`
/// must be able to end on screen evidence alone — but only evidence
/// that holds without a break for the whole grace period.
#[test]
fn an_unbroken_idle_screen_demotes_working_after_the_grace_period() {
    use super::super::agent_hooks::{settle_record, SCREEN_IDLE_GRACE};
    let t0 = Instant::now();
    let mut rec = AgentHookRecord::new(AgentActivity::Working, 0);

    assert!(!settle_record(&mut rec, true, t0));
    assert!(!settle_record(&mut rec, true, t0 + SCREEN_IDLE_GRACE / 2));
    // One busy frame restarts the clock.
    assert!(!settle_record(&mut rec, false, t0 + SCREEN_IDLE_GRACE / 2));
    assert!(!settle_record(&mut rec, true, t0 + SCREEN_IDLE_GRACE));
    assert_eq!(rec.activity, AgentActivity::Working);
    assert!(settle_record(&mut rec, true, t0 + SCREEN_IDLE_GRACE * 2));
    assert_eq!(rec.activity, AgentActivity::Idle);

    // `blocked` ends the same way once the dialog has gone.
    let mut rec = AgentHookRecord::new(AgentActivity::Blocked, 0);
    settle_record(&mut rec, true, t0);
    assert!(settle_record(&mut rec, true, t0 + SCREEN_IDLE_GRACE));
    assert_eq!(rec.activity, AgentActivity::Idle);
}

#[test]
fn a_cancelled_turn_frees_the_pane_once_the_screen_has_been_idle() {
    let (mut app, pane_id) = app_with_idle_copilot_pane();
    hook(&mut app, pane_id, "userPromptSubmitted", None);
    app.settle_agent_hook_states();
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Busy);

    // Pretend the idle composer has been on screen for longer than the
    // grace period, then let one frame of upkeep run.
    let long_ago = Instant::now() - super::super::agent_hooks::SCREEN_IDLE_GRACE * 2;
    app.agent_hook_states
        .get_mut(&pane_id)
        .expect("record")
        .screen_idle_since = Some(long_ago);
    app.settle_agent_hook_states();
    assert_eq!(activity(&app, pane_id), Some(AgentActivity::Idle));
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);
    app.shutdown();
}

/// A Copilot crash leaves no `1049l`, so a Claude started next in the
/// same pane shares the epoch. Its registration must hide the record.
#[test]
fn a_non_copilot_agent_never_sees_a_copilot_record() {
    let (mut app, pane_id) = app_with_idle_copilot_pane();
    hook(&mut app, pane_id, "userPromptSubmitted", None);
    assert_eq!(activity(&app, pane_id), Some(AgentActivity::Working));

    app.peer_client_kinds
        .insert(pane_id, PeerClientKind::Claude);
    assert_eq!(activity(&app, pane_id), None);
    let listed = app
        .handle_list(Some(pane_id), None)
        .expect("list")
        .into_iter()
        .find(|p| p.id == pane_id)
        .expect("pane listed");
    assert_eq!(listed.agent_state, None);

    app.settle_agent_hook_states();
    assert!(
        !app.agent_hook_states.contains_key(&pane_id),
        "upkeep drops a record whose pane no longer runs Copilot"
    );
    app.shutdown();
}
