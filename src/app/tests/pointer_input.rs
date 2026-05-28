use super::super::*;

// -- pane_local_coords / pane_local_coords_clamped ---------------

#[test]
fn pane_local_coords_rejects_border_clicks() {
    // A 10×5 pane at origin (2, 3): border at col 2 / col 11 /
    // row 3 / row 7. A click on any border cell must decline to
    // forward so the caller can fall through to the border-drag
    // handler instead.
    let rect = Rect::new(2, 3, 10, 5);
    assert!(pane_local_coords(rect, 2, 5).is_none(), "left border");
    assert!(pane_local_coords(rect, 11, 5).is_none(), "right border");
    assert!(pane_local_coords(rect, 5, 3).is_none(), "top border");
    assert!(pane_local_coords(rect, 5, 7).is_none(), "bottom border");
}

#[test]
fn pane_local_coords_translates_to_content_0_origin() {
    // Pane outer at (2, 3), content starts at (3, 4). A click at
    // screen (3, 4) must land on content (0, 0); (10, 6) maps
    // to (7, 2).
    let rect = Rect::new(2, 3, 10, 5);
    assert_eq!(pane_local_coords(rect, 3, 4), Some((0, 0)));
    assert_eq!(pane_local_coords(rect, 10, 6), Some((7, 2)));
}

#[test]
fn pane_local_coords_clamped_stays_inside_content() {
    // Clamp is used on Drag/Up where the cursor may wander off-
    // pane. Ensure clamp never produces an out-of-bounds cell.
    let rect = Rect::new(2, 3, 10, 5);
    // Cursor well to the right of the pane — should pin to the
    // last content column (width - 2 = 8 inner cells, 0..=7).
    assert_eq!(pane_local_coords_clamped(rect, 50, 50), (7, 2));
    // Cursor above/left of the pane — should pin to (0, 0).
    assert_eq!(pane_local_coords_clamped(rect, 0, 0), (0, 0));
    // Cursor inside — untouched.
    assert_eq!(pane_local_coords_clamped(rect, 5, 5), (2, 1));
}

#[test]
fn pane_local_coords_rejects_rects_too_small_for_content() {
    // A 2×2 or narrower rect has no interior after stripping the
    // 1-cell border. Codex review flagged that the pre-fix version
    // underflowed with `rect.width == 1`; the guard keeps such a
    // press from ever reaching the forward path.
    for (w, h) in [(0, 5), (1, 5), (2, 5), (5, 0), (5, 1), (5, 2)] {
        let rect = Rect::new(2, 3, w, h);
        assert!(
            pane_local_coords(rect, 3, 4).is_none(),
            "{}×{} rect must be rejected before the arithmetic fires",
            w,
            h
        );
    }
}

#[test]
fn pane_local_coords_survives_extreme_u16_origins() {
    // `rect.x = u16::MAX - 5` means `rect.x + rect.width` would
    // overflow in unchecked arithmetic. Saturating math keeps this
    // from panicking in debug builds.
    let rect = Rect::new(u16::MAX - 5, 0, 10, 5);
    // The call must return, not panic. Result content is
    // secondary — whatever it yields, it yields safely.
    let _ = pane_local_coords(rect, u16::MAX - 3, 2);
    let _ = pane_local_coords_clamped(rect, u16::MAX, u16::MAX);
}

// -- mouse_forward_disabled env gate -----------------------------

#[test]
fn mouse_forward_disabled_reads_env_var() {
    // Serialize against the global env so parallel tests don't
    // see each other's values. We only toggle within this test.
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();

    std::env::remove_var("RENGA_DISABLE_MOUSE_FORWARD");
    assert!(!mouse_forward_disabled(), "unset → false");

    std::env::set_var("RENGA_DISABLE_MOUSE_FORWARD", "1");
    assert!(mouse_forward_disabled(), "\"1\" → true");

    std::env::set_var("RENGA_DISABLE_MOUSE_FORWARD", "0");
    assert!(
        !mouse_forward_disabled(),
        "\"0\" must be treated as opt-in-off, matching the wheel-handler convention"
    );

    std::env::set_var("RENGA_DISABLE_MOUSE_FORWARD", "");
    assert!(!mouse_forward_disabled(), "empty string → false");

    std::env::set_var("RENGA_DISABLE_MOUSE_FORWARD", "yes");
    assert!(
        mouse_forward_disabled(),
        "any non-empty non-\"0\" value → true (permissive)"
    );

    std::env::remove_var("RENGA_DISABLE_MOUSE_FORWARD");
}

// ─── detect_outer_edge (Issue #245) ────────────────────────────

/// A single 100×50 pane filling the whole workspace. Every test in
/// this group uses this layout unless noted.
fn single_pane_rects() -> Vec<(usize, Rect)> {
    vec![(1, Rect::new(0, 0, 100, 50))]
}

#[test]
fn detect_outer_edge_returns_each_side_for_single_pane() {
    // A solo pane is the simplest case: all four outer edges belong
    // to pane 1 and corner cells must be rejected.
    let rects = single_pane_rects();
    let area = Rect::new(0, 0, 100, 50);

    // Mid-edge of each side: detection must classify the side and
    // return the only pane id.
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 0),
        Some((EdgeSide::Top, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 49),
        Some((EdgeSide::Bottom, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 0, 25),
        Some((EdgeSide::Left, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 25),
        Some((EdgeSide::Right, 1))
    );
}

#[test]
fn detect_outer_edge_rejects_corner_cells() {
    // Issue §3.5 v1: corners are ambiguous (two sides meet) so they
    // must not trigger a split.
    let rects = single_pane_rects();
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(detect_outer_edge(area, &rects, 0, 0), None, "top-left");
    assert_eq!(detect_outer_edge(area, &rects, 99, 0), None, "top-right");
    assert_eq!(detect_outer_edge(area, &rects, 0, 49), None, "bottom-left");
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 49),
        None,
        "bottom-right"
    );
}

#[test]
fn detect_outer_edge_rejects_inner_cells() {
    // Interior PTY cells must never look like an edge click.
    let rects = single_pane_rects();
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(detect_outer_edge(area, &rects, 50, 25), None);
    assert_eq!(detect_outer_edge(area, &rects, 1, 1), None);
    assert_eq!(detect_outer_edge(area, &rects, 98, 48), None);
}

#[test]
fn detect_outer_edge_rejects_shared_internal_boundary_cells() {
    // Two panes split vertically at x=50. The shared internal column
    // (x=50, the boundary between pane 1's right border and pane 2's
    // left border) is NOT an outer edge — only the workspace's true
    // outer rim is. Clicks on x=50 must return None so the
    // resize-drag boundary handler (which runs first in real code)
    // keeps owning that column.
    let rects = vec![(1, Rect::new(0, 0, 50, 50)), (2, Rect::new(50, 0, 50, 50))];
    let area = Rect::new(0, 0, 100, 50);
    // A point inside the shared boundary column but in the interior
    // (not on a row at the top/bottom outer edge) is fully internal.
    assert_eq!(detect_outer_edge(area, &rects, 50, 25), None);
    assert_eq!(detect_outer_edge(area, &rects, 49, 25), None);
}

#[test]
fn detect_outer_edge_nested_layout_does_not_overreject() {
    // Codex round 2: the boundary check must consider whether the
    // internal boundary actually spans the clicked outer row/col.
    // Layout: full-width top pane, bottom split left/right at col 50.
    //   A (0,0,100,25)
    //   B (0,25,50,25)  |  C (50,25,50,25)
    // A click at (50, 0) is on the top outer edge of pane A. The
    // x=50 vertical boundary only exists in the lower half, so it
    // must NOT block the click from registering as A's top edge.
    let rects = vec![
        (1, Rect::new(0, 0, 100, 25)),
        (2, Rect::new(0, 25, 50, 25)),
        (3, Rect::new(50, 25, 50, 25)),
    ];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 0),
        Some((EdgeSide::Top, 1)),
        "nested-bottom boundary at col 50 must not over-reject top-edge click"
    );
    // Symmetric: full-width bottom pane, top split left/right.
    let rects = vec![
        (1, Rect::new(0, 0, 50, 25)),
        (2, Rect::new(50, 0, 50, 25)),
        (3, Rect::new(0, 25, 100, 25)),
    ];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 49),
        Some((EdgeSide::Bottom, 3)),
        "nested-top boundary at col 50 must not over-reject bottom-edge click"
    );
}

#[test]
fn detect_outer_edge_rejects_outer_edge_x_internal_boundary_intersection() {
    // The intersection of an outer edge and an internal boundary is
    // ambiguous: cell (50, 0) in a vertical split sits on both the
    // top outer edge AND the internal x=50 boundary. In real flow
    // the boundary-resize hit-test runs first and consumes it, so
    // this is a defense-in-depth guard — but the helper's contract
    // ("reject shared boundaries") must hold standalone too.
    let rects = vec![(1, Rect::new(0, 0, 50, 50)), (2, Rect::new(50, 0, 50, 50))];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 0),
        None,
        "top edge ∩ internal vertical boundary"
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 49),
        None,
        "bottom edge ∩ internal vertical boundary"
    );
    // Horizontal split rejects left/right edge ∩ internal horizontal
    // boundary symmetrically.
    let rects = vec![
        (1, Rect::new(0, 0, 100, 25)),
        (2, Rect::new(0, 25, 100, 25)),
    ];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 0, 25),
        None,
        "left edge ∩ internal horizontal boundary"
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 25),
        None,
        "right edge ∩ internal horizontal boundary"
    );
}

#[test]
fn detect_outer_edge_picks_correct_pane_in_multi_layout() {
    // Vertical split: top-edge click on the left half must target
    // pane 1, top-edge click on the right half must target pane 2.
    // Same for the bottom edge. Left/right outer edges respectively
    // belong to pane 1 / pane 2.
    let rects = vec![(1, Rect::new(0, 0, 50, 50)), (2, Rect::new(50, 0, 50, 50))];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 25, 0),
        Some((EdgeSide::Top, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 75, 0),
        Some((EdgeSide::Top, 2))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 25, 49),
        Some((EdgeSide::Bottom, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 75, 49),
        Some((EdgeSide::Bottom, 2))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 0, 25),
        Some((EdgeSide::Left, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 25),
        Some((EdgeSide::Right, 2))
    );
}

#[test]
fn detect_outer_edge_offset_origin_workspace() {
    // The workspace doesn't always start at (0, 0); a side bar or
    // status bar can offset the pane area. detect_outer_edge must
    // anchor off `pane_area`, not absolute coordinates.
    let rects = vec![(7, Rect::new(20, 3, 80, 40))];
    let area = Rect::new(20, 3, 80, 40);
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 3),
        Some((EdgeSide::Top, 7))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 42),
        Some((EdgeSide::Bottom, 7))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 20, 20),
        Some((EdgeSide::Left, 7))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 20),
        Some((EdgeSide::Right, 7))
    );
    // Cell on the workspace edge but BEYOND the pane area: not an
    // edge click for this layout.
    assert_eq!(detect_outer_edge(area, &rects, 5, 1), None);
}

// ─── split_intent_for_edge (Issue #245) ────────────────────────

#[test]
fn split_intent_top_left_place_new_pane_first() {
    // Top and Left clicks must spawn the new pane in the first
    // (top / left) slot so the clicked edge becomes the new pane's
    // edge — the spec's "click on top → new pane appears on top".
    assert_eq!(
        split_intent_for_edge(EdgeSide::Top),
        (SplitDirection::Horizontal, true)
    );
    assert_eq!(
        split_intent_for_edge(EdgeSide::Left),
        (SplitDirection::Vertical, true)
    );
}

#[test]
fn split_intent_bottom_right_place_new_pane_second() {
    // Bottom and Right clicks place the new pane in the trailing
    // slot, matching the historical Ctrl+D / Ctrl+E placement.
    assert_eq!(
        split_intent_for_edge(EdgeSide::Bottom),
        (SplitDirection::Horizontal, false)
    );
    assert_eq!(
        split_intent_for_edge(EdgeSide::Right),
        (SplitDirection::Vertical, false)
    );
}
