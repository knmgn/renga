//! Issue #288 — the pane-control IPC requests resolve against the
//! **caller's** tab, not the tab the user happens to be looking at.
//!
//! Every test here builds the same shape: workspace 0 holds the caller
//! (a background tab), workspace 1 is the active tab the human is
//! watching. Pre-#288 all of these operated on workspace 1.

use super::super::*;

/// Two tabs. Returns `(caller_pane_in_ws0, active_pane_in_ws1)` with
/// workspace 1 active — i.e. the caller is *not* in the visible tab.
fn two_tabs() -> (App, usize, usize) {
    let mut app = App::new(40, 120).expect("App::new");
    let caller = app.ws().focused_pane_id;
    app.new_tab().expect("new_tab");
    let active = app.ws().focused_pane_id;
    assert_eq!(app.active_tab, 1, "the new tab is the visible one");
    assert_ne!(caller, active);
    (app, caller, active)
}

// ─── list_panes ───────────────────────────────────────────

#[test]
fn list_with_from_pane_returns_the_callers_tab_not_the_active_one() {
    let (mut app, caller, active) = two_tabs();

    let scoped = app.handle_list(Some(caller)).expect("scoped list");
    let ids: Vec<usize> = scoped.iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![caller], "caller's tab holds exactly its own pane");

    // And the legacy (CLI) call still describes the visible tab.
    let legacy = app.handle_list(None).expect("legacy list");
    assert_eq!(
        legacy.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![active]
    );

    app.shutdown();
}

#[test]
fn list_rejects_an_unknown_from_pane_instead_of_falling_back() {
    let (mut app, _caller, _active) = two_tabs();
    let err = app
        .handle_list(Some(9999))
        .expect_err("unknown caller pane");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

/// The geometry fields exist so an agent can act on them. A hidden tab
/// is not relaid out on a terminal resize, so listing it without
/// refreshing hands back coordinates from a terminal that is gone.
#[test]
fn list_reports_geometry_for_the_current_terminal_not_the_one_at_last_render() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);

    app.on_terminal_resize(60, 20);
    let infos = app.handle_list(Some(caller)).expect("scoped list");
    let rect = infos
        .iter()
        .find(|p| p.id == caller)
        .expect("caller in its own list");
    assert!(
        rect.width <= 60 && rect.height <= 20,
        "hidden tab reported {}x{} for a 60x20 terminal",
        rect.width,
        rect.height
    );
    assert!(rect.width > 0 && rect.height > 0);
    app.shutdown();
}

// ─── inspect_pane ─────────────────────────────────────────

#[test]
fn inspect_focused_resolves_in_the_callers_tab() {
    let (mut app, caller, active) = two_tabs();

    let payload = app
        .handle_inspect(&ipc::PaneRef::Focused, None, false, Some(caller))
        .expect("inspect focused");
    assert_eq!(
        payload["pane"]["id"].as_u64(),
        Some(caller as u64),
        "`focused` must mean the caller tab's focused pane"
    );

    let legacy = app
        .handle_inspect(&ipc::PaneRef::Focused, None, false, None)
        .expect("legacy inspect");
    assert_eq!(legacy["pane"]["id"].as_u64(), Some(active as u64));

    app.shutdown();
}

#[test]
fn inspect_by_id_reaches_across_tabs() {
    let (mut app, caller, active) = two_tabs();
    let payload = app
        .handle_inspect(&ipc::PaneRef::Id(active), None, false, Some(caller))
        .expect("explicit cross-tab id");
    assert_eq!(payload["pane"]["id"].as_u64(), Some(active as u64));
    app.shutdown();
}

// ─── send_keys ────────────────────────────────────────────

#[test]
fn send_focused_writes_to_the_callers_pane() {
    let (mut app, caller, _active) = two_tabs();
    app.handle_send(&ipc::PaneRef::Focused, b"hi", false, Some(caller))
        .expect("send to caller's focused pane");
    app.shutdown();
}

#[test]
fn send_by_name_never_leaves_the_callers_tab() {
    let (mut app, caller, active) = two_tabs();
    // Same name registered in both tabs; the caller must get its own.
    app.workspaces[0].pane_names.insert("worker".into(), caller);
    app.workspaces[1].pane_names.insert("worker".into(), active);

    let (ws_idx, pane_id) = app
        .resolve_request_target(Some(caller), &ipc::PaneRef::Name("worker".into()))
        .expect("name resolves");
    assert_eq!((ws_idx, pane_id), (0, caller));

    // A name that only exists in the *active* tab is invisible to the
    // caller — no silent cross-tab fallback.
    app.workspaces[1]
        .pane_names
        .insert("only-active".into(), active);
    let err = app
        .resolve_request_target(Some(caller), &ipc::PaneRef::Name("only-active".into()))
        .expect_err("name from another tab must not resolve");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));

    app.shutdown();
}

#[test]
fn a_dead_from_pane_is_rejected_even_when_the_target_id_is_valid() {
    let (mut app, _caller, active) = two_tabs();
    let err = app
        .handle_send(&ipc::PaneRef::Id(active), b"x", false, Some(4242))
        .expect_err("bogus caller");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    assert!(
        err.message.contains("caller pane"),
        "error must name the caller, not the target: {}",
        err.message
    );
    app.shutdown();
}

// ─── focus_pane ───────────────────────────────────────────

#[test]
fn focus_when_the_caller_is_already_visible_switches_nothing() {
    let mut app = App::new(40, 120).expect("App::new");
    let a = app.ws().focused_pane_id;
    let b = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(a),
        )
        .expect("split");
    assert_eq!(app.active_tab, 0);

    app.handle_focus(&ipc::PaneRef::Id(a), Some(b))
        .expect("focus sibling");
    assert_eq!(app.active_tab, 0, "no tab switch inside the visible tab");
    assert_eq!(app.ws().focused_pane_id, a);
    app.shutdown();
}

/// The contract is "focus means the keystrokes land there". A pane in a
/// tab the user cannot see cannot receive keystrokes, so *any* focus
/// that resolves outside the visible tab brings that tab forward —
/// including the caller focusing a pane in its own (hidden) tab. The
/// alternative, quietly setting `focused_pane_id` on a hidden
/// workspace, reports success while changing nothing the user or the
/// keyboard can observe.
#[test]
fn focus_resolving_into_a_hidden_tab_brings_that_tab_forward() {
    let (mut app, caller, _active) = two_tabs();
    app.handle_focus(&ipc::PaneRef::Focused, Some(caller))
        .expect("focus own pane from a background tab");
    assert_eq!(
        app.active_tab, 0,
        "focus the keyboard cannot reach is not focus — the tab must follow"
    );
    assert_eq!(app.workspaces[0].focused_pane_id, caller);
    assert!(matches!(app.workspaces[0].focus_target, FocusTarget::Pane));
    app.shutdown();
}

#[test]
fn focus_across_tabs_by_id_also_switches_the_visible_tab() {
    let (mut app, caller, active) = two_tabs();
    // Caller sits in hidden tab 0 and explicitly names a pane in the
    // visible tab 1 — allowed, and the visible tab stays put because
    // that is already where the target lives.
    app.handle_focus(&ipc::PaneRef::Id(active), Some(caller))
        .expect("cross-tab focus into the visible tab");
    assert_eq!(app.active_tab, 1);
    assert_eq!(app.workspaces[1].focused_pane_id, active);

    // Now the other direction: name the hidden tab's pane by id.
    app.handle_focus(&ipc::PaneRef::Id(caller), Some(active))
        .expect("cross-tab focus into the hidden tab");
    assert_eq!(app.active_tab, 0, "the hidden tab is brought forward");
    assert_eq!(app.workspaces[0].focused_pane_id, caller);
    app.shutdown();
}

// ─── spawn_* (Split) ──────────────────────────────────────

#[test]
fn split_lands_in_the_callers_tab_and_leaves_the_active_one_alone() {
    let (mut app, caller, active) = two_tabs();
    let active_focus_before = app.workspaces[1].focused_pane_id;
    let active_panes_before = app.workspaces[1].layout.pane_count();

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("spawned".into()),
            None,
            None,
            Some(caller),
        )
        .expect("split in caller's tab");

    assert!(
        app.workspaces[0].panes.contains_key(&new_id),
        "new pane belongs to the caller's workspace"
    );
    assert_eq!(
        app.workspaces[0].focused_pane_id, new_id,
        "focus follows the new pane inside its own tab"
    );
    assert_eq!(
        app.workspaces[0].pane_names.get("spawned").copied(),
        Some(new_id)
    );

    // The visible tab is completely untouched.
    assert_eq!(app.active_tab, 1);
    assert_eq!(app.workspaces[1].focused_pane_id, active_focus_before);
    assert_eq!(app.workspaces[1].layout.pane_count(), active_panes_before);
    assert!(!app.workspaces[1].panes.contains_key(&new_id));
    assert_ne!(new_id, active);

    app.shutdown();
}

#[test]
fn split_in_a_hidden_tab_reports_real_geometry_not_zeros() {
    let (mut app, caller, _active) = two_tabs();
    // Give the hidden workspace the geometry a render would have left.
    app.relayout_workspace(0);

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
        )
        .expect("split hidden tab");

    let infos = app.handle_list(Some(caller)).expect("list caller tab");
    let new_info = infos
        .iter()
        .find(|p| p.id == new_id)
        .expect("new pane in list");
    assert!(
        new_info.width > 0 && new_info.height > 0,
        "a hidden workspace never renders, so the split has to refresh its \
         rects itself — otherwise list_panes reports {new_info:?}"
    );
    app.shutdown();
}

#[test]
fn a_refused_split_leaves_both_workspaces_focus_untouched() {
    let (mut app, caller, _active) = two_tabs();
    let caller_focus_before = app.workspaces[0].focused_pane_id;
    let active_focus_before = app.workspaces[1].focused_pane_id;

    // Force a refusal without touching the layout: a minimum pane size
    // wider than the terminal makes every split too small.
    app.min_pane_width = 10_000;
    app.relayout_workspace(0);

    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
        )
        .expect_err("split must be refused");
    assert_eq!(err.code, Some(ipc::err_code::SPLIT_REFUSED));

    assert_eq!(app.workspaces[0].focused_pane_id, caller_focus_before);
    assert_eq!(app.workspaces[1].focused_pane_id, active_focus_before);
    assert_eq!(app.active_tab, 1);
    app.shutdown();
}

#[test]
fn split_inherits_the_target_panes_cwd_across_tabs() {
    let (mut app, caller, _active) = two_tabs();
    let base = std::env::temp_dir()
        .canonicalize()
        .expect("temp dir canonicalizes");
    if let Some(pane) = app.workspaces[0].panes.get_mut(&caller) {
        pane.cwd = base.clone();
    }

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            // Relative cwd is resolved against the *target* pane, which
            // is the caller here. `.` keeps us inside `base`.
            Some(".".into()),
            Some(caller),
        )
        .expect("split with relative cwd");

    let new_cwd = app.workspaces[0]
        .panes
        .get(&new_id)
        .map(|p| p.cwd.clone())
        .expect("new pane exists");
    assert_eq!(
        new_cwd,
        crate::app::layout_ops::strip_verbatim_prefix(base),
        "relative cwd resolves against the target pane in the caller's tab"
    );
    app.shutdown();
}

/// `poll_events` is process-wide, so an orchestrator in a background
/// tab waits on the event stream for the worker it just named. Pane ids
/// are unique App-wide, so resolving the new pane's metadata in the
/// *active* workspace does not error — it quietly returns nothing, and
/// the event goes out with `name: null, role: null`.
#[test]
fn a_cross_tab_spawn_emits_pane_started_with_its_name_and_role() {
    let (mut app, caller, _active) = two_tabs();
    let (_sub_id, rx) = app.event_bus.subscribe();

    let id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("worker-1".into()),
            Some("worker".into()),
            None,
            Some(caller),
        )
        .expect("cross-tab split");

    let mut observed: Option<(Option<String>, Option<String>)> = None;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PaneStarted {
            id: ev_id,
            name,
            role,
            ..
        } = ev
        {
            if ev_id == id {
                observed = Some((name, role));
                break;
            }
        }
    }
    let (name, role) = observed.expect("PaneStarted for the new pane");
    assert_eq!(name.as_deref(), Some("worker-1"));
    assert_eq!(role.as_deref(), Some("worker"));
    app.shutdown();
}

/// The root invariant behind the caller-scoped geometry: a terminal
/// resize relayouts *every* workspace, not just the visible one. Without
/// it, a hidden workspace keeps the rects it had when it was last on
/// screen, and every reader (`list_panes`, `inspect_pane`, the split
/// min-size guard) has to remember to refresh — three chances to forget
/// the fourth.
#[test]
fn a_terminal_resize_relayouts_hidden_workspaces_too() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);

    app.on_terminal_resize(46, 20);
    let width = app.workspaces[0]
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == caller)
        .map(|(_, r)| r.width)
        .expect("caller rect");
    assert!(
        width <= 46,
        "hidden tab still reports {width} cols after a resize to 46"
    );
    app.shutdown();
}

/// A resize is not the only thing that moves every workspace's pane
/// area — a status-bar toggle, a sidebar drag and a layout swap do too,
/// and those refresh only the active tab. The IPC boundary is what
/// guarantees a background caller never reads the difference, so these
/// go through `handle_app_command` rather than calling the handler
/// directly.
#[test]
fn a_global_layout_change_does_not_leak_stale_geometry_to_a_background_caller() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);
    let before = app.workspaces[0]
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == caller)
        .map(|(_, r)| r.height)
        .expect("caller rect");

    // Alt+S: a global metric, refreshed for the active tab only.
    app.status_bar_visible = !app.status_bar_visible;
    app.mark_layout_change();

    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: Some(caller),
        reply: reply_tx,
    });
    let infos = reply_rx.recv().expect("list reply").expect("list ok");
    let reported = infos
        .iter()
        .find(|p| p.id == caller)
        .map(|p| p.height)
        .expect("caller listed");

    assert_ne!(
        before, reported,
        "precondition: toggling the status bar changes the pane height"
    );
    assert_eq!(
        reported, app.workspaces[0].last_pane_rects[0].1.height,
        "the reported height is the workspace's current one"
    );
    app.shutdown();
}

/// Consequence of that invariant: a cross-tab split is judged against
/// the terminal that exists now, so it refuses exactly where the same
/// split in the visible tab would.
#[test]
fn a_cross_tab_split_guards_against_the_current_terminal() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);
    app.set_min_pane_size(20, 5);
    app.on_terminal_resize(46, 20);

    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
        )
        .expect_err("46 cols cannot hold two 20-column panes");
    assert_eq!(err.code, Some(ipc::err_code::SPLIT_REFUSED));
    app.shutdown();
}

/// Below the layout threshold `relayout_workspace` cannot run at all, so
/// every workspace's rects describe a terminal that is gone. Splitting
/// on them would be guesswork; refuse instead.
#[test]
fn a_split_is_refused_while_the_terminal_is_too_small_to_lay_out() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);
    app.on_terminal_resize(10, 3);

    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
        )
        .expect_err("no usable geometry at 10x3");
    assert_eq!(err.code, Some(ipc::err_code::SPLIT_REFUSED));
    app.shutdown();
}

/// A background agent spawning a pane must not drop the text the user
/// is selecting in the tab they are looking at — that tab's geometry did
/// not move. A selection anchored to the *split* workspace is stale and
/// still has to go.
#[test]
fn a_cross_tab_split_keeps_a_selection_that_belongs_to_another_tab() {
    let (mut app, caller, active) = two_tabs();
    let sel = |pane_id| TextSelection {
        target: SelectionTarget::Pane(pane_id),
        start_row: 0,
        start_col: 0,
        end_row: 0,
        end_col: 4,
        content_rect: Rect::new(0, 0, 10, 2),
    };

    app.selection = Some(sel(active));
    app.handle_split(
        &ipc::PaneRef::Focused,
        ipc::Direction::Vertical,
        None,
        None,
        None,
        None,
        Some(caller),
    )
    .expect("split the hidden tab");
    assert!(
        app.selection.is_some(),
        "the visible tab's selection survives a split in another tab"
    );

    // Anchored in the workspace being split: that geometry just moved.
    app.selection = Some(sel(caller));
    app.handle_split(
        &ipc::PaneRef::Id(caller),
        ipc::Direction::Horizontal,
        None,
        None,
        None,
        None,
        Some(caller),
    )
    .expect("second split");
    assert!(
        app.selection.is_none(),
        "a selection in the split workspace is stale and must be dropped"
    );
    app.shutdown();
}

// ─── legacy (CLI) semantics ───────────────────────────────

#[test]
fn without_from_pane_an_id_stays_inside_the_active_tab() {
    let (mut app, caller, _active) = two_tabs();
    // `renga send --id <caller>` from a shell must keep behaving the
    // way it did before #288: active tab only, no cross-tab widening.
    let err = app
        .resolve_request_target(None, &ipc::PaneRef::Id(caller))
        .expect_err("legacy id must not reach into another tab");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}
