use super::super::*;

use crate::app::layout_geometry;
use crate::app::org_sidebar::OrgSidebarTarget;
use crate::config::OrgSidebarMode;

/// `App::new` leaves the sidebar off (it is turned on by
/// `apply_config`), so every test here opts in explicitly.
fn app_with_sidebar(rows: u16, cols: u16) -> App {
    let mut app = App::new(rows, cols).expect("App::new");
    app.last_term_size = (cols, rows);
    app.org_sidebar_mode = OrgSidebarMode::Coexist;
    app.org_sidebar_visible = true;
    app
}

fn add_tabs(app: &mut App, n: usize) {
    for _ in 0..n {
        app.new_tab().expect("new_tab");
    }
}

// ─── switch_tab ───────────────────────────────────────────

#[test]
fn switch_tab_carries_org_sidebar_focus_into_the_incoming_tab() {
    // `focus_target` is per-workspace, so without an explicit carry
    // the incoming tab restores whatever it was focused on last —
    // knocking the user out of the very panel they are driving.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    app.switch_tab(0);
    app.ws_mut().focus_target = FocusTarget::OrgSidebar;

    assert!(app.switch_tab(1), "switch to a different tab should apply");
    assert_eq!(
        app.ws().focus_target,
        FocusTarget::OrgSidebar,
        "sidebar focus must survive a tab switch"
    );
}

#[test]
fn switch_tab_leaves_non_sidebar_focus_to_the_incoming_tab() {
    // The carry is deliberately sidebar-only: the file tree and
    // preview are per-tab panels, so each tab keeps its own answer.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    app.switch_tab(0);
    app.ws_mut().focus_target = FocusTarget::FileTree;
    app.switch_tab(1);
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn switch_tab_rejects_an_out_of_range_index() {
    // `ws()` indexes `workspaces` directly and would panic. The
    // sidebar renders from a snapshot an MCP `close_tab` can
    // invalidate before the click lands.
    let mut app = app_with_sidebar(40, 160);
    assert!(!app.switch_tab(7));
    assert_eq!(app.active_tab, 0);
}

#[test]
fn switch_tab_to_the_current_tab_is_a_noop() {
    // The old keyboard paths called `suspend_overlay` unconditionally,
    // so Alt+1 on the tab you were already on stashed the IME draft
    // for no reason.
    let mut app = app_with_sidebar(40, 160);
    app.last_tab_click = Some((0, Instant::now()));
    assert!(!app.switch_tab(0));
    assert!(
        app.last_tab_click.is_some(),
        "a no-op switch must not clear caches"
    );
}

#[test]
fn switch_tab_clears_every_tab_keyed_cache() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    app.switch_tab(0);
    let now = Instant::now();
    app.last_tab_click = Some((0, now));
    app.last_edge_click = Some((0, 5, 5, now));
    app.last_boundary_click = Some((0, 5, 5, now));

    app.switch_tab(1);

    assert!(app.last_tab_click.is_none());
    assert!(app.last_edge_click.is_none());
    assert!(app.last_boundary_click.is_none());
    assert!(app.selection.is_none());
}

// ─── close_tab index shift ────────────────────────────────

#[test]
fn closing_a_tab_before_the_active_one_follows_the_index_shift() {
    // Regression guard. `workspaces.remove(index)` shifts every later
    // tab down one slot, but the old code only clamped when
    // `active_tab` ran off the end. With four tabs and the user on
    // index 2, closing index 1 left `active_tab` numerically 2 —
    // which now pointed at what used to be tab 3. The user silently
    // ended up on a workspace they never selected.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 3); // tabs 0..=3
    app.switch_tab(2);
    let watching = app.ws().focused_pane_id;

    app.close_tab(1);

    assert_eq!(app.workspaces.len(), 3);
    assert_eq!(app.active_tab, 1, "active tab must follow the shift");
    assert_eq!(
        app.ws().focused_pane_id,
        watching,
        "the user must still be looking at the same workspace"
    );
}

#[test]
fn closing_a_tab_after_the_active_one_leaves_the_active_index_alone() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 3);
    app.switch_tab(1);
    let watching = app.ws().focused_pane_id;

    app.close_tab(3);

    assert_eq!(app.active_tab, 1);
    assert_eq!(app.ws().focused_pane_id, watching);
}

#[test]
fn closing_the_active_tab_lands_on_the_tab_that_takes_its_slot() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 2); // tabs 0..=2
    app.switch_tab(1);
    let next_tab_pane = app.workspaces[2].focused_pane_id;

    app.close_tab(1);

    assert_eq!(app.active_tab, 1);
    assert_eq!(app.ws().focused_pane_id, next_tab_pane);
}

#[test]
fn tab_set_changes_drop_the_sidebar_click_cache() {
    // Row indices and tab indices both shift underneath the cache.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 2);
    app.org_sidebar_row_targets = vec![OrgSidebarTarget::tab(0)];
    app.org_sidebar_scroll = 4;
    app.org_sidebar_selection = Some(OrgSidebarTarget::tab(2));

    app.close_tab(2);

    assert!(app.org_sidebar_row_targets.is_empty());
    assert_eq!(app.org_sidebar_scroll, 0);
    assert!(app.org_sidebar_selection.is_none());

    app.org_sidebar_scroll = 3;
    app.new_tab().expect("new_tab");
    assert_eq!(app.org_sidebar_scroll, 0);
}

// ─── focus cycle ──────────────────────────────────────────

#[test]
fn focus_cycle_visits_the_sidebar_last() {
    // Order is `panes → file tree → preview → org sidebar → panes`;
    // the sidebar goes on the end so existing Ctrl+Right muscle
    // memory is untouched.
    let mut app = app_with_sidebar(40, 160);
    app.ws_mut().file_tree_visible = true;
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);

    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);
    app.focus_next_pane(); // preview is inactive, so straight on
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);
    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn focus_cycle_runs_backwards_symmetrically() {
    let mut app = app_with_sidebar(40, 160);
    app.ws_mut().file_tree_visible = true;

    app.focus_prev_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);
    app.focus_prev_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);
    app.focus_prev_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn focus_cycle_skips_the_sidebar_when_it_is_off() {
    // The pre-#291 cycle must be reproduced exactly for users who
    // never turn the panel on.
    let mut app = App::new(40, 160).expect("App::new");
    app.ws_mut().file_tree_visible = true;

    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);
    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn focus_leaves_a_sidebar_that_disappeared_underneath_it() {
    let mut app = app_with_sidebar(40, 160);
    app.ws_mut().focus_target = FocusTarget::OrgSidebar;
    app.org_sidebar_visible = false;

    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

// ─── row model ────────────────────────────────────────────

#[test]
fn rows_list_tabs_then_their_panes_in_layout_order() {
    // Pane order comes from `collect_pane_ids` (the layout tree's own
    // first-then-second walk), not `HashMap` iteration, so the list
    // matches what is on screen.
    let mut app = app_with_sidebar(40, 160);
    app.split_focused_pane(SplitDirection::Vertical, None)
        .expect("split");
    add_tabs(&mut app, 1);

    let expected_tab0: Vec<usize> = app.workspaces[0].layout.collect_pane_ids();
    assert_eq!(expected_tab0.len(), 2, "tab 0 should have two panes");

    let rows = app.org_sidebar_rows();
    let targets: Vec<OrgSidebarTarget> = rows.iter().map(|r| r.target).collect();

    assert_eq!(targets[0], OrgSidebarTarget::tab(0));
    assert_eq!(targets[1].pane_id, Some(expected_tab0[0]));
    assert_eq!(targets[2].pane_id, Some(expected_tab0[1]));
    assert_eq!(targets[3], OrgSidebarTarget::tab(1));
}

#[test]
fn selection_is_stored_by_identity_not_row_index() {
    // A row index would silently retarget when a tab above the
    // selection is closed.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 2);
    app.org_sidebar_selection = Some(OrgSidebarTarget::tab(2));

    let rows = app.org_sidebar_rows();
    let before = app.org_sidebar_selected_index(&rows);
    assert_eq!(rows[before].target, OrgSidebarTarget::tab(2));
}

#[test]
fn a_stale_selection_falls_back_to_the_active_tab_row() {
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_selection = Some(OrgSidebarTarget {
        tab: 9,
        pane_id: Some(999),
    });

    let rows = app.org_sidebar_rows();
    let idx = app.org_sidebar_selected_index(&rows);
    assert_eq!(rows[idx].target, OrgSidebarTarget::tab(app.active_tab));
}

#[test]
fn activating_a_pane_row_switches_tab_and_focuses_that_pane() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    let other_pane = app.workspaces[1].focused_pane_id;
    app.switch_tab(0);
    app.ws_mut().focus_target = FocusTarget::OrgSidebar;

    app.org_sidebar_activate_target(OrgSidebarTarget {
        tab: 1,
        pane_id: Some(other_pane),
    });

    assert_eq!(app.active_tab, 1);
    assert_eq!(app.ws().focused_pane_id, other_pane);
    assert_eq!(
        app.ws().focus_target,
        FocusTarget::Pane,
        "a pane row is an explicit 'take me there'"
    );
}

#[test]
fn activating_a_tab_row_keeps_focus_in_the_sidebar() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    app.switch_tab(0);
    app.ws_mut().focus_target = FocusTarget::OrgSidebar;

    app.org_sidebar_activate_target(OrgSidebarTarget::tab(1));

    assert_eq!(app.active_tab, 1);
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);
}

#[test]
fn activating_a_vanished_tab_is_ignored() {
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_activate_target(OrgSidebarTarget::tab(5));
    assert_eq!(app.active_tab, 0);
}

// ─── toggle / modes ───────────────────────────────────────

#[test]
fn toggle_is_inert_when_the_mode_is_off() {
    // `off` frees Ctrl+B for the PTY, the same way `[ime] mode = off`
    // frees Ctrl+;.
    let mut app = App::new(40, 160).expect("App::new");
    app.org_sidebar_mode = OrgSidebarMode::Off;
    app.org_sidebar_visible = false;

    app.toggle_org_sidebar();

    assert!(!app.org_sidebar_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn toggle_follows_the_file_tree_three_way_shape() {
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_visible = false;

    app.toggle_org_sidebar(); // hidden → shown + focused
    assert!(app.org_sidebar_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);

    app.ws_mut().focus_target = FocusTarget::Pane;
    app.toggle_org_sidebar(); // shown but unfocused → focus only
    assert!(app.org_sidebar_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);

    app.toggle_org_sidebar(); // shown and focused → hide
    assert!(!app.org_sidebar_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn replace_mode_hands_the_slot_between_the_two_panels() {
    // The panels share one column range in `replace`, so Ctrl+F has to
    // close the sidebar — otherwise the tree would take focus while
    // the layout kept painting the sidebar over it.
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_mode = OrgSidebarMode::Replace;
    app.ws_mut().file_tree_visible = true;

    // Sidebar up ⇒ the tree is not painted, so Ctrl+F must reopen it.
    app.toggle_file_tree();
    assert!(
        !app.org_sidebar_visible,
        "opening the tree closes the panel"
    );
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);
    assert!(app.ws().file_tree_visible);
}

// ─── geometry agreement ───────────────────────────────────

#[test]
fn relayout_places_panes_past_the_sidebar_and_the_tree() {
    // `render_main_area` and `relayout_panes` now resolve geometry
    // through the same helper; this pins the observable half of that
    // contract — the origin `renga list` and mouse hit-testing read
    // out of `last_pane_rects`.
    let mut app = app_with_sidebar(40, 160);
    app.ws_mut().file_tree_visible = true;
    let expected_x = app.org_sidebar_width + app.file_tree_width;

    app.relayout_panes();

    let pane_id = app.ws().focused_pane_id;
    let rect = app
        .ws()
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .map(|(_, r)| *r)
        .expect("relayout should populate the focused pane's rect");
    assert_eq!(rect.x, expected_x);
    assert_eq!(rect.y, 1, "still below the tab strip");
}

#[test]
fn relayout_matches_the_layout_helper_across_narrow_widths() {
    // Walk the whole degrade ladder and assert the PTY-side origin
    // agrees with the helper at every step, including the widths
    // where a panel drops out.
    for cols in [160u16, 106, 105, 66, 65, 46, 45, 36, 35, 24] {
        let mut app = app_with_sidebar(40, cols);
        app.ws_mut().file_tree_visible = true;
        app.relayout_panes();

        let expected = layout_geometry::compute(app.main_area_input(Rect::new(0, 1, cols, 39)));
        let pane_id = app.ws().focused_pane_id;
        let rect = app
            .ws()
            .last_pane_rects
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, r)| *r)
            .expect("focused pane rect");
        assert_eq!(
            rect.x, expected.panes.x,
            "pane origin drifted from the shared helper at {cols} cols"
        );
        assert_eq!(
            rect.width, expected.panes.width,
            "pane width drifted from the shared helper at {cols} cols"
        );
    }
}

#[test]
fn a_hidden_sidebar_reproduces_the_pre_291_pane_origin() {
    // Back-compat guard: users who never enable the panel must see
    // byte-identical geometry.
    let mut app = App::new(40, 120).expect("App::new");
    app.last_term_size = (120, 40);
    app.ws_mut().file_tree_visible = true;
    let tree_w = app.file_tree_width;

    app.relayout_panes();

    let pane_id = app.ws().focused_pane_id;
    let rect = app
        .ws()
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .map(|(_, r)| *r)
        .expect("focused pane rect");
    assert_eq!(rect.x, tree_w);
}
