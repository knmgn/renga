//! `deliver="user_turn"` (Issue #323).
//!
//! Two halves, deliberately kept apart:
//!
//! - the **readiness predicate**, driven against synthetic vt100
//!   screens copied from what real Claude Code / Codex panes actually
//!   render (captured with `inspect_pane` against live panes), and
//!
//! - the **delivery state machine**, driven through the pure
//!   [`step_user_turn`] with explicit clocks and explicit composer
//!   contents, so none of it depends on sleeping next to a live PTY.

use super::super::user_turn::{
    claude_turn_readiness, codex_turn_readiness, normalize_user_turn_body, step_user_turn,
    TurnAgent, TurnReadiness, UserTurnStage, UserTurnStep, USER_TURN_CONFIRM_DELAY,
    USER_TURN_DEADLINE, USER_TURN_SETTLE_DELAY,
};
use super::super::*;

/// Paint `bytes` onto the focused pane's vt100 screen without going
/// near its PTY, and hand back the pane id.
fn seed_focused_pane_screen(app: &mut App, bytes: &[u8]) -> usize {
    let pane_id = app.ws().focused_pane_id;
    let pane = app
        .ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists");
    let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.process(bytes);
    drop(parser);
    pane_id
}

/// Run a screen through the Claude predicate in isolation.
fn claude_readiness_of(bytes: &[u8], rows: u16, cols: u16) -> TurnReadiness {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    claude_turn_readiness(parser.screen())
}

fn codex_readiness_of(bytes: &[u8], rows: u16, cols: u16) -> TurnReadiness {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    codex_turn_readiness(parser.screen())
}

/// The composer as Claude Code 2.x actually draws it: a horizontal
/// rule, the prompt glyph alone on its own row with the hardware cursor
/// parked just after it, another rule, then the mode footer.
///
/// Captured from a live pane — this exact shape (not the older
/// `╭──╮` box) is what the predicate has to accept.
fn claude_idle_screen(footer: &str) -> Vec<u8> {
    let rule = "─".repeat(40);
    // \x1b[?25h keeps the cursor visible; the final CUP parks it on the
    // prompt row at the first edit cell, exactly like Claude does.
    format!(
        "\x1b[2J\x1b[H\x1b[?25hsome transcript text\r\n\r\n{rule}\r\n\u{276F}\r\n{rule}\r\n{footer}\x1b[4;3H"
    )
    .into_bytes()
}

// ── readiness: Claude ─────────────────────────────────────────

#[test]
fn claude_idle_composer_is_ready() {
    assert_eq!(
        claude_readiness_of(
            &claude_idle_screen("\u{23F5}\u{23F5} auto mode on (shift+tab to cycle)"),
            8,
            40
        ),
        TurnReadiness::Ready
    );
}

/// Claude Code keeps an *empty* composer on screen while it works —
/// verified against three live busy panes — so emptiness alone says
/// nothing about idleness. The interrupt affordance in the footer is
/// what separates "accepting a turn" from "mid-turn", and getting this
/// wrong means every delivery races a permission dialog.
#[test]
fn claude_busy_footer_is_busy_not_ready() {
    assert_eq!(
        claude_readiness_of(
            &claude_idle_screen("\u{23F5}\u{23F5} auto mode on \u{00B7} esc to interrupt"),
            8,
            40
        ),
        TurnReadiness::Busy
    );
}

/// A permission menu's option row also "starts with a prompt glyph".
/// It must not be mistaken for a composer — that is the exact failure
/// the #323 design note calls out in herdr's `agent.prompt`.
#[test]
fn claude_permission_dialog_is_not_ready() {
    let screen = "\x1b[2J\x1b[H\x1b[?25h\
         \u{256D}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256E}\r\n\
         \u{2502} Bash command   \u{2502}\r\n\
         \u{2502}                \u{2502}\r\n\
         \u{2502} \u{276F} 1. Yes       \u{2502}\r\n\
         \u{2502}   2. No        \u{2502}\r\n\
         \u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256F}\x1b[4;4H";
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// A folder-trust / "load development channel?" style dialog has the
/// same shape and must be refused the same way — answering it stays
/// `send_keys`' job.
#[test]
fn claude_trust_dialog_is_not_ready() {
    let screen = "\x1b[2J\x1b[H\x1b[?25hDo you trust the files in this folder?\r\n\r\n\
         \u{276F} 1. Yes, proceed\r\n  2. No, exit\x1b[3;3H";
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// A human's half-typed draft owns the composer. Delivering into it
/// would submit their words concatenated with ours.
#[test]
fn claude_composer_holding_a_draft_is_not_ready() {
    let rule = "─".repeat(40);
    let screen = format!(
        "\x1b[2J\x1b[H\x1b[?25h\r\n{rule}\r\n\u{276F} half-typed thought\r\n{rule}\r\n? for shortcuts\x1b[3;22H"
    );
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// The frame rows are half the structural proof. Without them any
/// bottom-most `>` on screen — a shell prompt, a quoted line of
/// transcript — would read as a composer.
#[test]
fn claude_prompt_glyph_without_frame_rows_is_not_ready() {
    let screen = "\x1b[2J\x1b[H\x1b[?25hsome output\r\n\u{276F}\r\nmore output\x1b[2;3H";
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// The composer must also own the caret. A cursor parked elsewhere
/// means something else is taking input.
#[test]
fn claude_caret_outside_composer_is_not_ready() {
    let rule = "─".repeat(40);
    // Same idle composer, but the cursor is left up in the transcript.
    let screen = format!(
        "\x1b[2J\x1b[H\x1b[?25htranscript\r\n{rule}\r\n\u{276F}\r\n{rule}\r\nfooter\x1b[1;1H"
    );
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// A hidden cursor is fine when the older inverse-video caret cell is
/// painted in the composer instead — Claude Code has shipped both.
#[test]
fn claude_inverse_caret_satisfies_the_caret_check() {
    let rule = "─".repeat(40);
    let screen = format!(
        "\x1b[2J\x1b[H\x1b[?25ltranscript\r\n{rule}\r\n\u{276F} \x1b[7m \x1b[0m\r\n{rule}\r\nfooter"
    );
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::Ready
    );
}

/// A full-screen TUI (vim, lazygit) has taken the terminal; there is no
/// composer to be found and retrying will not change that.
#[test]
fn claude_alternate_screen_is_unsupported() {
    let mut bytes = b"\x1b[?1049h".to_vec();
    bytes.extend_from_slice(&claude_idle_screen("footer"));
    assert_eq!(
        claude_readiness_of(&bytes, 8, 40),
        TurnReadiness::Unsupported
    );
}

/// A blank screen is a pane that has not painted yet, not a refusal to
/// take turns — the caller should retry.
#[test]
fn claude_blank_screen_is_not_ready() {
    assert_eq!(
        claude_readiness_of(b"\x1b[2J\x1b[H", 8, 40),
        TurnReadiness::NotReady
    );
}

// ── readiness: Codex ──────────────────────────────────────────

#[test]
fn codex_ready_prompt_is_ready() {
    assert_eq!(
        codex_readiness_of(b"\x1b[2J\x1b[H\x1b[?25h\xE2\x80\xBA \x1b[1;3H", 8, 40),
        TurnReadiness::Ready
    );
}

#[test]
fn codex_busy_banner_is_busy() {
    assert_eq!(
        codex_readiness_of(
            b"\x1b[2J\x1b[H\x1b[?25hworking\xE2\x80\xA6 esc to interrupt\r\n\xE2\x80\xBA \x1b[1;3H",
            8,
            40
        ),
        TurnReadiness::Busy
    );
}

/// The nudge path accepts a bare `ready for input` banner as good
/// enough. A user turn does not: a late nudge is a nuisance, a turn
/// typed into an unproven screen is damage. This pins the difference.
#[test]
fn codex_ready_for_input_string_alone_is_not_enough() {
    assert_eq!(
        codex_readiness_of(b"\x1b[2J\x1b[Hready for input", 8, 40),
        TurnReadiness::NotReady
    );
    // ...while the existing nudge gate still accepts it, unchanged.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[Hready for input");
    let pane = app.ws().panes.get(&pane_id).expect("pane");
    assert!(App::codex_peer_delivery_ready(true, pane));
    app.shutdown();
}

// ── agent resolution ──────────────────────────────────────────

/// Registration is authoritative, matching
/// `pane_expects_codex_peer_delivery`. Requiring the live OSC title
/// instead would make a pane unaddressable exactly while it works,
/// because agents rewrite that title to the in-flight task.
#[test]
fn registered_kind_beats_the_window_title() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.peer_client_kinds
        .insert(pane_id, PeerClientKind::Claude);
    assert_eq!(app.user_turn_agent(0, pane_id), Some(TurnAgent::Claude));

    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
    assert_eq!(app.user_turn_agent(0, pane_id), Some(TurnAgent::Codex));
    app.shutdown();
}

/// An unregistered plain shell is not a turn-taking target.
#[test]
fn unregistered_shell_pane_has_no_turn_agent() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    assert_eq!(app.user_turn_agent(0, pane_id), None);
    assert_eq!(
        app.user_turn_readiness(0, pane_id),
        TurnReadiness::Unsupported
    );
    app.shutdown();
}

// ── body normalization ────────────────────────────────────────

#[test]
fn body_normalization_collapses_line_endings() {
    assert_eq!(
        normalize_user_turn_body("a\r\nb\rc").expect("normalizes"),
        "a\nb\nc"
    );
}

#[test]
fn empty_body_is_rejected() {
    let err = normalize_user_turn_body("   \n  ").expect_err("empty body refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
}

/// The body is written into somebody else's PTY, so an escape byte in
/// it is a live control sequence in their terminal, not a display
/// glitch — the same reasoning as `ipc::sanitized_label`.
#[test]
fn control_characters_in_body_are_rejected() {
    let err = normalize_user_turn_body("hello\x1b[31mworld").expect_err("escape refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
    // Tab and newline are ordinary composer content and stay allowed.
    assert!(normalize_user_turn_body("a\tb\nc").is_ok());
}

// ── delivery state machine ────────────────────────────────────

fn t0() -> Instant {
    Instant::now()
}

/// An empty Claude composer renders as its prompt glyph, not as blank
/// text — so "the block is non-empty" is true before a single byte
/// arrives. The settle stage compares against the pre-write snapshot
/// instead; without that, the machine submits into an empty composer.
#[test]
fn settle_stage_waits_for_the_composer_to_differ_from_the_pre_write_snapshot() {
    let now = t0();
    let stage = UserTurnStage::AwaitDraft {
        ready_at: now,
        empty: "\u{276F}\n".to_string(),
    };
    let deadline = now + USER_TURN_DEADLINE;

    // Composer still reads exactly as it did before the write.
    assert_eq!(
        step_user_turn(&stage, Some("\u{276F}\n"), now, deadline),
        UserTurnStep::Wait
    );

    // Draft visible → move to confirmation.
    match step_user_turn(&stage, Some("\u{276F} /loop\n"), now, deadline) {
        UserTurnStep::Advance(UserTurnStage::AwaitConfirm {
            draft, restarts, ..
        }) => {
            assert_eq!(draft, "\u{276F} /loop\n");
            assert_eq!(restarts, 0);
        }
        other => panic!("expected AwaitConfirm, got {other:?}"),
    }
}

#[test]
fn settle_stage_holds_until_the_settle_delay_elapses() {
    let now = t0();
    let stage = UserTurnStage::AwaitDraft {
        ready_at: now + USER_TURN_SETTLE_DELAY,
        empty: "\u{276F}\n".to_string(),
    };
    assert_eq!(
        step_user_turn(&stage, Some("\u{276F} hi\n"), now, now + USER_TURN_DEADLINE),
        UserTurnStep::Wait
    );
}

#[test]
fn a_stable_draft_is_submitted() {
    let now = t0();
    let stage = UserTurnStage::AwaitConfirm {
        ready_at: now,
        draft: "\u{276F} /loop\n".to_string(),
        restarts: 0,
    };
    match step_user_turn(
        &stage,
        Some("\u{276F} /loop\n"),
        now,
        now + USER_TURN_DEADLINE,
    ) {
        UserTurnStep::Submit(UserTurnStage::AwaitSubmit { draft }) => {
            assert_eq!(draft, "\u{276F} /loop\n")
        }
        other => panic!("expected Submit, got {other:?}"),
    }
}

/// A draft still being repainted restarts the stability window rather
/// than being submitted half-drawn.
#[test]
fn a_changing_draft_restarts_the_stability_window() {
    let now = t0();
    let stage = UserTurnStage::AwaitConfirm {
        ready_at: now,
        draft: "\u{276F} /lo\n".to_string(),
        restarts: 0,
    };
    match step_user_turn(
        &stage,
        Some("\u{276F} /loop\n"),
        now,
        now + USER_TURN_DEADLINE,
    ) {
        UserTurnStep::Advance(UserTurnStage::AwaitConfirm {
            draft,
            restarts,
            ready_at,
        }) => {
            assert_eq!(draft, "\u{276F} /loop\n");
            assert_eq!(restarts, 1);
            assert_eq!(ready_at, now + USER_TURN_CONFIRM_DELAY);
        }
        other => panic!("expected a restarted AwaitConfirm, got {other:?}"),
    }
}

/// A composer that never settles is somebody else's — a human typing,
/// most likely. Give up rather than submit whatever they paused on.
#[test]
fn an_endlessly_changing_draft_stalls_instead_of_submitting() {
    let now = t0();
    let stage = UserTurnStage::AwaitConfirm {
        ready_at: now,
        draft: "\u{276F} a\n".to_string(),
        restarts: 3,
    };
    assert!(matches!(
        step_user_turn(&stage, Some("\u{276F} ab\n"), now, now + USER_TURN_DEADLINE),
        UserTurnStep::Stalled(_)
    ));
}

#[test]
fn a_consumed_draft_counts_as_submitted() {
    let now = t0();
    let stage = UserTurnStage::AwaitSubmit {
        draft: "\u{276F} /loop\n".to_string(),
    };
    // Composer back to empty — the ordinary case.
    assert_eq!(
        step_user_turn(&stage, Some("\u{276F}\n"), now, now + USER_TURN_DEADLINE),
        UserTurnStep::Submitted
    );
    // `/clear` repaints the screen out from under us; the composer may
    // not be findable at all. The draft is still gone.
    assert_eq!(
        step_user_turn(&stage, None, now, now + USER_TURN_DEADLINE),
        UserTurnStep::Submitted
    );
}

/// A spinner repaint is not a submit. Only the draft's disappearance
/// counts, so an unchanged composer keeps waiting and eventually
/// stalls — it never reports success it did not observe.
#[test]
fn an_unconsumed_draft_waits_then_stalls() {
    let now = t0();
    let stage = UserTurnStage::AwaitSubmit {
        draft: "\u{276F} /loop\n".to_string(),
    };
    assert_eq!(
        step_user_turn(
            &stage,
            Some("\u{276F} /loop\n"),
            now,
            now + USER_TURN_DEADLINE
        ),
        UserTurnStep::Wait
    );
    assert!(matches!(
        step_user_turn(&stage, Some("\u{276F} /loop\n"), now, now),
        UserTurnStep::Stalled(_)
    ));
}

// ── handler-level behavior ────────────────────────────────────

fn user_turn_result(
    app: &mut App,
    from: usize,
    target: ipc::PaneRef,
    body: &str,
) -> std::result::Result<serde_json::Value, ipc::CodedError> {
    let (tx, rx) = oneshot::channel();
    app.handle_peer_send_user_turn(from, &target, body.to_string(), tx);
    rx.try_recv().expect("handler answered synchronously")
}

/// Refusals must be provably byte-free — that is what makes them
/// retryable — and must never leak the body out as a channel tag
/// instead.
#[test]
fn refusal_writes_nothing_and_emits_no_peer_inbox() {
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let pane_id = app.ws().focused_pane_id;
    while rx.try_recv().is_ok() {}

    // Plain shell pane: no turn-taking agent behind it.
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("unsupported target");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_UNSUPPORTED_TARGET));

    while let Ok(ev) = rx.try_recv() {
        assert!(
            !matches!(ev, ipc::Event::PeerInbox { .. }),
            "a user turn must never also arrive as a channel tag: {ev:?}"
        );
    }
    assert!(app.pending_user_turns.is_empty());
    app.shutdown();
}

#[test]
fn unknown_target_is_pane_not_found() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(9999), "hi")
        .expect_err("unknown target");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

/// A refused delivery records nothing in the dedupe ledger, so the
/// caller's retry after clearing the blocker is not swallowed.
#[test]
fn a_refused_user_turn_leaves_no_dedupe_trace() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let _ = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop");
    assert!(
        app.recent_user_turn_sends.is_empty(),
        "a refusal must stay freely retryable"
    );
    app.shutdown();
}

/// Paint an idle Claude composer onto the focused pane, sized to that
/// pane's *actual* PTY geometry — a pane is smaller than the terminal
/// (status bar, borders), so a fixed-width rule would wrap and shift
/// every row the predicate looks at.
fn seed_claude_idle_pane(app: &mut App, prefix: &[u8]) -> usize {
    let pane_id = app.ws().focused_pane_id;
    let (rows, cols) = {
        let pane = app.ws().panes.get(&pane_id).expect("focused pane exists");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().size()
    };
    assert!(rows >= 5 && cols >= 8, "test pane too small: {rows}x{cols}");
    let rule = "─".repeat(cols.saturating_sub(1) as usize);
    let mut bytes = prefix.to_vec();
    bytes.extend_from_slice(
        format!(
            "\x1b[2J\x1b[H\x1b[?25htranscript\
             \x1b[{};1H{rule}\
             \x1b[{};1H\u{276F}\
             \x1b[{};1H{rule}\
             \x1b[{};1H\u{23F5}\u{23F5} auto mode on (shift+tab to cycle)\
             \x1b[{};3H",
            rows - 3,
            rows - 2,
            rows - 1,
            rows,
            rows - 2,
        )
        .as_bytes(),
    );
    seed_focused_pane_screen(app, &bytes)
}

/// Stand up a pane that the predicate will accept: registered as
/// Claude, painting an idle composer.
fn app_with_ready_claude_pane() -> (App, usize) {
    let mut app = App::new(40, 120).expect("App::new");
    let pane_id = seed_claude_idle_pane(&mut app, b"");
    app.peer_client_kinds
        .insert(pane_id, PeerClientKind::Claude);
    (app, pane_id)
}

/// The happy path defers: the handler writes the body and parks the
/// reply rather than answering, because "did this submit?" cannot be
/// known yet.
#[test]
fn an_accepted_user_turn_is_parked_not_answered() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);

    let (tx, rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);

    assert!(
        rx.try_recv().is_err(),
        "an accepted delivery must not answer before it has observed a submit"
    );
    assert_eq!(app.pending_user_turns.len(), 1);
    assert_eq!(
        app.recent_user_turn_sends.len(),
        1,
        "the dedupe entry must exist before the bytes, not after"
    );
    app.shutdown();
}

/// Two drafts in one composer would submit their concatenation.
#[test]
fn a_second_delivery_while_one_is_in_flight_is_refused() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, _rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    assert_eq!(app.pending_user_turns.len(), 1);

    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "different")
        .expect_err("concurrent delivery refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_NOT_READY));
    app.shutdown();
}

/// An identical retry inside the window is collapsed — and says so,
/// unlike the channel path's indistinguishable `Ok`. A caller that just
/// got `user_turn_stalled` has to be able to tell "your retry was
/// swallowed" from "your retry was delivered", or it will keep firing
/// `/clear` at a pane it already cleared.
#[test]
fn an_identical_user_turn_within_the_window_is_suppressed() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, _rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    // Retire the in-flight delivery so the retry reaches the dedupe
    // check rather than the concurrency guard.
    app.pending_user_turns.clear();

    let out = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect("duplicate reports success");
    assert_eq!(
        out.get("status").and_then(|v| v.as_str()),
        Some("duplicate_suppressed")
    );
    assert!(
        app.pending_user_turns.is_empty(),
        "a suppressed retry must not write a second time"
    );
    app.shutdown();
}

/// A multi-line body typed raw would submit at its first newline and
/// drive the UI with the rest. Without bracketed paste there is no safe
/// encoding, so it is refused before anything is written.
#[test]
fn a_multiline_body_without_bracketed_paste_is_refused() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let err = user_turn_result(
        &mut app,
        pane_id,
        ipc::PaneRef::Id(pane_id),
        "line one\nline two",
    )
    .expect_err("multi-line without bracketed paste refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
    assert!(app.pending_user_turns.is_empty());
    assert!(
        app.recent_user_turn_sends.is_empty(),
        "a refusal must stay freely retryable"
    );
    app.shutdown();
}

/// With bracketed paste enabled the same body is accepted, because the
/// application has told us it treats a paste as composer content.
#[test]
fn a_multiline_body_is_accepted_when_bracketed_paste_is_on() {
    let mut app = App::new(40, 120).expect("App::new");
    // `\x1b[?2004h` is the application declaring it handles pastes.
    let pane_id = seed_claude_idle_pane(&mut app, b"\x1b[?2004h");
    app.peer_client_kinds
        .insert(pane_id, PeerClientKind::Claude);

    let (tx, rx) = oneshot::channel();
    app.handle_peer_send_user_turn(
        pane_id,
        &ipc::PaneRef::Id(pane_id),
        "line one\nline two".into(),
        tx,
    );
    assert!(rx.try_recv().is_err(), "accepted, so deferred");
    assert_eq!(app.pending_user_turns.len(), 1);
    app.shutdown();
}

/// The user-turn ledger is separate from the channel one: a `<channel>`
/// report must not suppress a later, deliberately different, real user
/// turn carrying the same text.
#[test]
fn channel_dedupe_does_not_suppress_a_later_user_turn() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.handle_peer_send(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".to_string())
        .expect("channel send");
    assert_eq!(app.recent_peer_sends.len(), 1);
    assert!(
        app.recent_user_turn_sends.is_empty(),
        "the channel ledger must not stand in for the user-turn one"
    );
    app.shutdown();
}
