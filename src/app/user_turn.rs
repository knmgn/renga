//! `deliver="user_turn"` — delivering a peer message as a real user
//! turn (Issue #323).
//!
//! A channel message is *shown* to the recipient without taking its
//! turn. Some instructions only take effect as a genuine user turn:
//! `/loop`, `/clear` and slash commands generally are not armed by a
//! `<channel>` tag. Reaching that today means driving `send_keys` by
//! hand — write the text, check it landed, send Enter as a *separate*
//! call — a ritual enforced by prose, and therefore a ritual that
//! eventually breaks.
//!
//! This module owns that ritual instead, in one place:
//!
//! 1. **Prove readiness** ([`App::user_turn_readiness`]). Positive
//!    identification only: an agent renga recognizes, an empty composer
//!    framed the way that agent frames it, caret evidence inside it, and
//!    no "interrupt" affordance on screen. Anything unproven is refused
//!    with zero bytes written — a modal must fail closed, never be typed
//!    into (see the design note on #323).
//! 2. **Write the body** — once, without Enter.
//! 3. **Settle, then confirm** the draft is on screen and stable, so a
//!    submit can't fire into a composer somebody else just touched.
//! 4. **Enter as a separate PTY write**, then observe that the draft was
//!    actually consumed before reporting success.
//!
//! Nothing here touches `send_keys`. Dialog control (folder-trust Enter,
//! permission `y`/`n`, `Shift+Tab`, `Ctrl+C`) is precisely the state
//! where "did the text land in the input box?" has no meaning, and a
//! readiness check there would either refuse the keystroke or delay it
//! past the moment it was meant for.

use super::codex_peer::{
    codex_prompt_allows_peer_nudge_on_screen, screen_has_visible_text, screen_tail_lines,
    write_input_to_pane,
};
use super::*;
use crate::ui::{find_prompt_row, resolve_input_row_last};

/// How long to wait after writing the body before reading the composer
/// back. Covers the agent's render round-trip: the PTY echo, the TUI's
/// own repaint, and renga's next `drain_pty_events`.
pub(crate) const USER_TURN_SETTLE_DELAY: Duration = Duration::from_millis(300);

/// Gap between two identical composer reads required before Enter. A
/// single read can catch a half-painted frame; requiring the same
/// content twice means the draft has stopped moving.
pub(crate) const USER_TURN_CONFIRM_DELAY: Duration = Duration::from_millis(120);

/// How many times the confirm stage may accept a *changed* composer and
/// restart its stability window. A couple of restarts absorb a slow
/// multi-frame repaint; an unbounded retry would let a human typing in
/// the pane keep the delivery alive until the deadline and then submit
/// whatever they happened to pause on.
const USER_TURN_CONFIRM_MAX_RESTARTS: u8 = 3;

/// Whole-delivery budget, measured from the moment the App accepts the
/// command. Deliberately well inside [`crate::ipc::APP_REPLY_TIMEOUT`]
/// (5s) so a slow delivery reports `user_turn_stalled` — which says
/// something specific about the target — instead of `app_timeout`,
/// which claims renga itself is wedged.
pub(crate) const USER_TURN_DEADLINE: Duration = Duration::from_millis(3000);

/// Window during which an identical `(target, from, body)` user turn is
/// suppressed. Mirrors [`super::codex_peer::PEER_SEND_DEDUPE_TTL`] in
/// size but is a **separate** ledger: a channel report and a user turn
/// are different intentional operations, so neither may swallow the
/// other.
pub(crate) const USER_TURN_DEDUPE_TTL: Duration = Duration::from_secs(5);

/// Minimum non-blank cells a row needs before it can count as one of
/// the composer's frame rows. Keeps a dialog's `│ … │` side borders —
/// two non-blank cells on an otherwise empty line — from passing as a
/// horizontal rule.
const COMPOSER_FRAME_MIN_CELLS: usize = 4;

/// Box-drawing glyphs a composer frame row may be built from. Covers
/// both shapes Claude Code has shipped: the bordered input box
/// (`╭──╮` / `╰──╯`) and the current pair of plain horizontal rules.
const FRAME_GLYPHS: &[&str] = &[
    "─", "━", "│", "┃", "╭", "╮", "╰", "╯", "┌", "┐", "└", "┘", "├", "┤", "═", "╌", "┄", "┈",
];

/// Substrings that mean "this agent is mid-turn". Scanned only in the
/// status rows *below* the composer, never in transcript text, so a
/// message that happens to discuss interrupts cannot fake a busy pane.
///
/// This is the one string-shaped signal in the predicate, and it can
/// only ever turn a *delivery* into a *refusal* — the composer proof
/// above it is what authorizes writing. If a future Claude Code drops
/// or translates these, the failure is "a turn is delivered to a busy
/// agent, which queues it", not "a turn is typed into a dialog".
const BUSY_MARKERS: &[&str] = &["to interrupt", "tab to queue"];

/// Which agent renga believes owns a pane's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnAgent {
    Claude,
    Codex,
}

/// Outcome of the readiness predicate. Every non-`Ready` value
/// guarantees that no bytes were written to the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnReadiness {
    /// An empty, focused composer was positively identified.
    Ready,
    /// A composer was identified but the agent is mid-turn.
    Busy,
    /// No empty composer could be proven — a draft is present, a modal
    /// is up, or the screen is simply not one renga recognizes.
    NotReady,
    /// The pane is not running an agent that takes turns at all.
    Unsupported,
}

impl TurnReadiness {
    fn into_error(self, pane_id: usize) -> Option<ipc::CodedError> {
        match self {
            TurnReadiness::Ready => None,
            TurnReadiness::Busy => Some(ipc::CodedError::new(
                ipc::err_code::USER_TURN_BUSY,
                format!(
                    "pane {pane_id} is mid-turn; nothing was written. Retry once it goes idle, \
                     or interrupt it with send_keys first."
                ),
            )),
            TurnReadiness::NotReady => Some(ipc::CodedError::new(
                ipc::err_code::USER_TURN_NOT_READY,
                format!(
                    "pane {pane_id} is not accepting a turn: no empty agent composer is on \
                     screen (a draft, a permission prompt or another modal is up, or the UI is \
                     one renga cannot read). Nothing was written — resolve it with send_keys \
                     and retry."
                ),
            )),
            TurnReadiness::Unsupported => Some(ipc::CodedError::new(
                ipc::err_code::USER_TURN_UNSUPPORTED_TARGET,
                format!(
                    "pane {pane_id} is not running an agent that takes user turns (deliver=\
                     \"user_turn\" needs a Claude or Codex pane). Use deliver=\"channel\", or \
                     send_keys for raw input."
                ),
            )),
        }
    }
}

/// Stage of an accepted delivery. The body has already been written by
/// the time any of these exist.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // every stage genuinely awaits something
pub(crate) enum UserTurnStage {
    /// Waiting out [`USER_TURN_SETTLE_DELAY`], then for the composer to
    /// stop reading as `empty` — the snapshot taken just *before* the
    /// body was written.
    ///
    /// The reference snapshot is load-bearing. An empty composer is not
    /// blank text: it renders as its prompt glyph (`❯`), so "the block
    /// is non-empty" would be true before a single byte arrived and the
    /// machine would submit into an empty composer.
    AwaitDraft { ready_at: Instant, empty: String },
    /// Draft seen; waiting for a second identical read before Enter.
    AwaitConfirm {
        ready_at: Instant,
        draft: String,
        restarts: u8,
    },
    /// Enter written; waiting for the draft to be consumed.
    AwaitSubmit { draft: String },
}

/// What [`step_user_turn`] wants the caller to do with a delivery.
///
/// Splitting the decision from the I/O keeps every timing and
/// observation rule — settle, stability, restart budget, deadline,
/// "was the draft consumed?" — in one pure function that tests can
/// drive with explicit clocks and explicit screens, instead of with
/// sleeps against a live PTY.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UserTurnStep {
    /// Nothing to do this frame.
    Wait,
    /// Adopt the new stage; no bytes to write.
    Advance(UserTurnStage),
    /// Write Enter, then adopt the new stage.
    Submit(UserTurnStage),
    /// Terminal: the draft was consumed.
    Submitted,
    /// Terminal: bytes were written but submission was not observed.
    Stalled(&'static str),
}

/// One step of the delivery state machine.
///
/// `composer` is the target's input block as it reads *right now*, or
/// `None` when no composer could be found at all.
pub(crate) fn step_user_turn(
    stage: &UserTurnStage,
    composer: Option<&str>,
    now: Instant,
    deadline: Instant,
) -> UserTurnStep {
    let expired = now >= deadline;
    match stage {
        UserTurnStage::AwaitDraft { ready_at, empty } => {
            if now < *ready_at {
                return if expired {
                    UserTurnStep::Stalled("the settle window never elapsed")
                } else {
                    UserTurnStep::Wait
                };
            }
            match composer {
                Some(text) if text != empty => UserTurnStep::Advance(UserTurnStage::AwaitConfirm {
                    ready_at: now + USER_TURN_CONFIRM_DELAY,
                    draft: text.to_string(),
                    restarts: 0,
                }),
                _ if expired => {
                    UserTurnStep::Stalled("the body never appeared in the target's composer")
                }
                _ => UserTurnStep::Wait,
            }
        }
        UserTurnStage::AwaitConfirm {
            ready_at,
            draft,
            restarts,
        } => {
            if now < *ready_at {
                return if expired {
                    UserTurnStep::Stalled("the draft never stopped changing")
                } else {
                    UserTurnStep::Wait
                };
            }
            let Some(current) = composer else {
                return UserTurnStep::Stalled("the composer disappeared before submit");
            };
            if current == draft {
                return UserTurnStep::Submit(UserTurnStage::AwaitSubmit {
                    draft: draft.clone(),
                });
            }
            if *restarts >= USER_TURN_CONFIRM_MAX_RESTARTS {
                return UserTurnStep::Stalled(
                    "the composer kept changing, so the draft was never confirmed — another \
                     writer may own it",
                );
            }
            if expired {
                return UserTurnStep::Stalled("the draft never stopped changing");
            }
            UserTurnStep::Advance(UserTurnStage::AwaitConfirm {
                ready_at: now + USER_TURN_CONFIRM_DELAY,
                draft: current.to_string(),
                restarts: restarts.saturating_add(1),
            })
        }
        UserTurnStage::AwaitSubmit { draft } => {
            // Submission is "our draft is gone", not "the screen
            // changed": a spinner repaint is not a submit, and a slash
            // command that repaints the whole screen is.
            let consumed = composer.is_none_or(|current| current != draft);
            if consumed {
                UserTurnStep::Submitted
            } else if expired {
                UserTurnStep::Stalled("Enter was written but the draft is still in the composer")
            } else {
                UserTurnStep::Wait
            }
        }
    }
}

/// One in-flight user-turn delivery, parked in
/// [`App::pending_user_turns`] with the IPC reply it still owes.
#[derive(Debug)]
pub(crate) struct PendingUserTurn {
    target_pane: usize,
    reply: Option<oneshot::Sender<std::result::Result<serde_json::Value, ipc::CodedError>>>,
    stage: UserTurnStage,
    deadline: Instant,
}

impl PendingUserTurn {
    fn answer(&mut self, outcome: std::result::Result<serde_json::Value, ipc::CodedError>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(outcome);
        }
    }
}

fn submitted_payload(target_id: usize) -> serde_json::Value {
    serde_json::json!({
        "delivery": "user_turn",
        "status": "submitted",
        "target_id": target_id,
    })
}

fn duplicate_payload(target_id: usize) -> serde_json::Value {
    serde_json::json!({
        "delivery": "user_turn",
        "status": "duplicate_suppressed",
        "target_id": target_id,
    })
}

fn stalled_error(pane_id: usize, detail: &str) -> ipc::CodedError {
    ipc::CodedError::new(
        ipc::err_code::USER_TURN_STALLED,
        format!(
            "user turn to pane {pane_id} was written but submission was not observed ({detail}). \
             The body may be sitting in the composer — inspect the pane before retrying."
        ),
    )
}

// ── screen reading ────────────────────────────────────────────

fn cell_text(screen: &vt100::Screen, row: u16, col: u16) -> String {
    screen
        .cell(row, col)
        .map(|c| c.contents().to_string())
        .unwrap_or_default()
}

fn row_text(screen: &vt100::Screen, row: u16) -> String {
    let cols = screen.size().1;
    let mut line = String::with_capacity(cols as usize);
    for col in 0..cols {
        line.push_str(&cell_text(screen, row, col));
    }
    line.trim_end().to_string()
}

/// Whether `row` looks like one of the composer's horizontal frame
/// rows: enough non-blank cells to be a rule, and every one of them a
/// box-drawing glyph.
fn row_is_composer_frame(screen: &vt100::Screen, row: u16) -> bool {
    if row >= screen.size().0 {
        return false;
    }
    let cols = screen.size().1;
    let mut seen = 0usize;
    for col in 0..cols {
        let s = cell_text(screen, row, col);
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if !FRAME_GLYPHS.contains(&s) {
            return false;
        }
        seen += 1;
    }
    seen >= COMPOSER_FRAME_MIN_CELLS
}

/// Column of the prompt glyph on `row`, searching the same leading
/// columns [`row_starts_with_prompt`] does.
fn prompt_glyph_col(screen: &vt100::Screen, row: u16) -> Option<u16> {
    let cols = screen.size().1.min(crate::ui::CLAUDE_PROMPT_SCAN_COLS);
    (0..cols).find(|&col| {
        let s = cell_text(screen, row, col);
        crate::ui::CLAUDE_PROMPT_GLYPHS.contains(&s.as_str())
    })
}

/// Whether the composer at `prompt_row` holds nothing. Everything right
/// of the prompt glyph must be blank, ignoring the box's right-hand
/// border, and the input block must not extend onto wrapped rows.
fn composer_is_empty(screen: &vt100::Screen, prompt_row: u16) -> bool {
    let Some(glyph_col) = prompt_glyph_col(screen, prompt_row) else {
        return false;
    };
    if resolve_input_row_last(screen, prompt_row) != prompt_row {
        return false;
    }
    let cols = screen.size().1;
    for col in glyph_col.saturating_add(1)..cols {
        let s = cell_text(screen, prompt_row, col);
        let s = s.trim();
        if s.is_empty() || FRAME_GLYPHS.contains(&s) {
            continue;
        }
        return false;
    }
    true
}

/// Caret evidence that the composer, and not something else on screen,
/// currently owns input. Accepts both eras of Claude Code rendering:
/// the visible hardware cursor parked on the prompt row (2.x), and the
/// painted inverse caret cell older builds used.
fn caret_is_in_composer(screen: &vt100::Screen, prompt_row: u16) -> bool {
    if !screen.hide_cursor() && screen.cursor_position().0 == prompt_row {
        return true;
    }
    let cols = screen.size().1;
    (0..cols).any(|col| screen.cell(prompt_row, col).is_some_and(|c| c.inverse()))
}

/// Scan the status rows below the composer for an "interrupt"
/// affordance. Bounded to those rows on purpose — see [`BUSY_MARKERS`].
fn busy_below_composer(screen: &vt100::Screen, prompt_row: u16) -> bool {
    let rows = screen.size().0;
    let mut haystack = String::new();
    for row in prompt_row.saturating_add(1)..rows {
        haystack.push_str(&row_text(screen, row).to_lowercase());
        haystack.push('\n');
    }
    BUSY_MARKERS.iter().any(|m| haystack.contains(m))
}

/// The current contents of the agent's input block, used as the draft
/// fingerprint. Unlike [`composer_is_empty`] this makes no demands of
/// the content — it just reports what is there, so the state machine can
/// tell "still our draft" from "consumed".
fn composer_block_text(screen: &vt100::Screen) -> Option<String> {
    let prompt_row = find_prompt_row(screen)?;
    let last = resolve_input_row_last(screen, prompt_row);
    let mut out = String::new();
    for row in prompt_row..=last {
        out.push_str(&row_text(screen, row));
        out.push('\n');
    }
    Some(out)
}

/// Readiness for a Claude pane.
///
/// The composer is proven *positively*: a prompt glyph row sandwiched
/// between two frame rows, empty, with the caret in it. Claude Code
/// keeps that composer on screen while it works, so emptiness alone
/// does not mean idle — the interrupt affordance below it is what
/// separates "accepting a turn" from "mid-turn".
pub(crate) fn claude_turn_readiness(screen: &vt100::Screen) -> TurnReadiness {
    if screen.alternate_screen() {
        return TurnReadiness::Unsupported;
    }
    if !screen_has_visible_text(screen) {
        return TurnReadiness::NotReady;
    }
    let Some(prompt_row) = find_prompt_row(screen) else {
        return TurnReadiness::NotReady;
    };
    // A permission menu's `❯ 1. Yes` row is also "a row starting with a
    // prompt glyph". It fails here twice over: its neighbours are not
    // rules, and it is not empty.
    if prompt_row == 0 || !row_is_composer_frame(screen, prompt_row.saturating_sub(1)) {
        return TurnReadiness::NotReady;
    }
    if !row_is_composer_frame(screen, prompt_row.saturating_add(1)) {
        return TurnReadiness::NotReady;
    }
    if !composer_is_empty(screen, prompt_row) {
        return TurnReadiness::NotReady;
    }
    if !caret_is_in_composer(screen, prompt_row) {
        return TurnReadiness::NotReady;
    }
    if busy_below_composer(screen, prompt_row) {
        return TurnReadiness::Busy;
    }
    TurnReadiness::Ready
}

/// Readiness for a Codex pane.
///
/// Reuses the structural half of the existing nudge gate
/// ([`codex_prompt_allows_peer_nudge_on_screen`]: bottom-most `›`
/// composer, visible cursor, cursor at the empty edit position) but
/// deliberately drops that path's `"enter to send"` / `"ready for
/// input"` string fallback. A nudge that lands late is a cosmetic
/// nuisance; a user turn typed into an unproven screen is not.
pub(crate) fn codex_turn_readiness(screen: &vt100::Screen) -> TurnReadiness {
    if screen.alternate_screen() {
        return TurnReadiness::Unsupported;
    }
    if !screen_has_visible_text(screen) {
        return TurnReadiness::NotReady;
    }
    let tail = screen_tail_lines(screen).join("\n").to_ascii_lowercase();
    if BUSY_MARKERS.iter().any(|m| tail.contains(m)) {
        return TurnReadiness::Busy;
    }
    match codex_prompt_allows_peer_nudge_on_screen(screen) {
        Some(true) => TurnReadiness::Ready,
        _ => TurnReadiness::NotReady,
    }
}

// ── body handling ─────────────────────────────────────────────

/// Normalize a user-turn body, or explain why it cannot be typed.
///
/// Line endings collapse to `\n` so the multi-line decision downstream
/// sees one representation. Control characters are refused outright:
/// this string is written into another agent's PTY, so an `\x1b` in it
/// is a live escape sequence in somebody else's terminal, not a display
/// glitch — the same reasoning as `ipc::sanitized_label`. Channel
/// delivery is untouched by any of this and still accepts any body.
pub(crate) fn normalize_user_turn_body(body: &str) -> std::result::Result<String, ipc::CodedError> {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.trim().is_empty() {
        return Err(ipc::CodedError::new(
            ipc::err_code::USER_TURN_INVALID_BODY,
            "deliver=\"user_turn\" needs a non-empty message".to_string(),
        ));
    }
    if let Some(bad) = normalized
        .chars()
        .find(|c| c.is_control() && *c != '\n' && *c != '\t')
    {
        return Err(ipc::CodedError::new(
            ipc::err_code::USER_TURN_INVALID_BODY,
            format!(
                "message contains the control character {bad:?}, which would reach the target's \
                 terminal as an escape sequence rather than as text"
            ),
        ));
    }
    Ok(normalized)
}

/// Bytes to write for `body`, or an error when the pane cannot accept
/// them.
///
/// A multi-line body typed raw submits at its first newline and drives
/// the UI with the remainder, so it goes out as a bracketed paste — and
/// only when the application has actually enabled bracketed paste,
/// which is renga's existing signal for "this app treats a paste as
/// composer content" (see `App::paste_to_pane`). Single-line bodies are
/// written verbatim so a leading `/` still reads as a slash command.
fn user_turn_payload(
    body: &str,
    pane: &Pane,
    pane_id: usize,
) -> std::result::Result<Vec<u8>, ipc::CodedError> {
    if !body.contains('\n') {
        return Ok(body.as_bytes().to_vec());
    }
    if !pane.is_bracketed_paste_enabled() {
        return Err(ipc::CodedError::new(
            ipc::err_code::USER_TURN_INVALID_BODY,
            format!(
                "message is multi-line but pane {pane_id} has not enabled bracketed paste; \
                 typing it raw would submit the first line and drive the UI with the rest. \
                 Send a single-line body, or use deliver=\"channel\"."
            ),
        ));
    }
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    Ok(out)
}

impl App {
    /// Which agent, if any, owns `pane_id`'s input.
    ///
    /// Registration is authoritative when present and the live OSC
    /// title is the fallback — the same precedence
    /// [`App::pane_expects_codex_peer_delivery`] uses, and for the same
    /// reason in reverse: an agent rewrites its window title to the
    /// in-flight task, so requiring the live title would make a
    /// registered pane unaddressable exactly while it is working.
    /// Neither signal authorizes writing on its own; the composer proof
    /// does that.
    pub(crate) fn user_turn_agent(&self, ws_index: usize, pane_id: usize) -> Option<TurnAgent> {
        match self.peer_client_kinds.get(&pane_id) {
            Some(PeerClientKind::Claude) => return Some(TurnAgent::Claude),
            Some(PeerClientKind::Codex) => return Some(TurnAgent::Codex),
            None => {}
        }
        let pane = self.workspaces.get(ws_index)?.panes.get(&pane_id)?;
        if pane.is_codex_running() {
            Some(TurnAgent::Codex)
        } else if pane.is_claude_running() {
            Some(TurnAgent::Claude)
        } else {
            None
        }
    }

    /// Full readiness predicate for one pane.
    pub(crate) fn user_turn_readiness(&self, ws_index: usize, pane_id: usize) -> TurnReadiness {
        let Some(pane) = self
            .workspaces
            .get(ws_index)
            .and_then(|w| w.panes.get(&pane_id))
        else {
            return TurnReadiness::NotReady;
        };
        // A pane whose startup command has not been flushed yet has no
        // agent behind it: the shell prompt hasn't even been observed.
        if pane.pending_startup.is_some() {
            return TurnReadiness::NotReady;
        }
        let Some(agent) = self.user_turn_agent(ws_index, pane_id) else {
            return TurnReadiness::Unsupported;
        };
        let Ok(parser) = pane.parser.lock() else {
            return TurnReadiness::NotReady;
        };
        match agent {
            TurnAgent::Claude => claude_turn_readiness(parser.screen()),
            TurnAgent::Codex => codex_turn_readiness(parser.screen()),
        }
    }

    /// Return true when an identical user turn was accepted within
    /// [`USER_TURN_DEDUPE_TTL`], recording the new one otherwise.
    /// Deliberately mirrors `is_duplicate_peer_send` — including the
    /// timestamp refresh and the TTL sweep — over its own map.
    fn is_duplicate_user_turn(&mut self, target: usize, from: usize, body: &str) -> bool {
        let now = Instant::now();
        self.recent_user_turn_sends
            .retain(|_, ts| now.duration_since(*ts) < USER_TURN_DEDUPE_TTL);
        let key = (target, from, body.to_string());
        match self.recent_user_turn_sends.get(&key).copied() {
            Some(prev) if now.duration_since(prev) < USER_TURN_DEDUPE_TTL => {
                self.recent_user_turn_sends.insert(key, now);
                true
            }
            _ => false,
        }
    }

    fn record_user_turn(&mut self, target: usize, from: usize, body: &str) {
        self.recent_user_turn_sends
            .insert((target, from, body.to_string()), Instant::now());
    }

    /// Accept (or refuse) a `deliver="user_turn"` peer send.
    ///
    /// Everything that can fail without touching the target's PTY fails
    /// here, synchronously, and answers `reply` immediately. Once the
    /// body has been written the delivery is parked in
    /// [`Self::pending_user_turns`] and `reply` is answered later by
    /// [`Self::flush_pending_user_turns`].
    ///
    /// Never emits `Event::PeerInbox`: a user turn must not also show up
    /// as a `<channel>` tag or a Codex `check_messages` body.
    pub(crate) fn handle_peer_send_user_turn(
        &mut self,
        from_pane: usize,
        target: &PaneRef,
        body: String,
        reply: oneshot::Sender<std::result::Result<serde_json::Value, ipc::CodedError>>,
    ) {
        let mut reply = Some(reply);
        let mut answer = |outcome: std::result::Result<serde_json::Value, ipc::CodedError>| {
            if let Some(tx) = reply.take() {
                let _ = tx.send(outcome);
            }
        };

        let Some((sender_ws, _)) = self.resolve_pane_across_workspaces(&PaneRef::Id(from_pane))
        else {
            answer(Err(ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("sender pane {from_pane} not found"),
            )));
            return;
        };
        let Some((target_ws, target_id)) = self.resolve_target_from(sender_ws, target) else {
            answer(Err(ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!(
                    "peer target not found: {target:?} (names only resolve inside the sender's \
                     tab; use the numeric pane id from list_peers for other tabs)"
                ),
            )));
            return;
        };

        let normalized = match normalize_user_turn_body(&body) {
            Ok(v) => v,
            Err(e) => {
                answer(Err(e));
                return;
            }
        };

        // A second delivery to the same composer while the first is
        // mid-flight would interleave two drafts and submit their
        // concatenation. Refuse rather than race.
        if self
            .pending_user_turns
            .iter()
            .any(|p| p.target_pane == target_id)
        {
            answer(Err(ipc::CodedError::new(
                ipc::err_code::USER_TURN_NOT_READY,
                format!(
                    "a user-turn delivery to pane {target_id} is already in flight; nothing was \
                     written. Retry once it settles."
                ),
            )));
            return;
        }

        if self.is_duplicate_user_turn(target_id, from_pane, &normalized) {
            // Unlike the channel path, say so out loud. A caller that
            // just got `user_turn_stalled` needs to know its retry was
            // collapsed rather than delivered — the whole point of the
            // window is that an uncertain `/clear` is not fired twice.
            answer(Ok(duplicate_payload(target_id)));
            return;
        }

        if let Some(e) = self
            .user_turn_readiness(target_ws, target_id)
            .into_error(target_id)
        {
            answer(Err(e));
            return;
        }

        let Some(pane) = self
            .workspaces
            .get_mut(target_ws)
            .and_then(|w| w.panes.get_mut(&target_id))
        else {
            answer(Err(ipc::CodedError::new(
                ipc::err_code::PANE_VANISHED,
                format!("pane {target_id} vanished before delivery"),
            )));
            return;
        };
        let payload = match user_turn_payload(&normalized, pane, target_id) {
            Ok(v) => v,
            Err(e) => {
                answer(Err(e));
                return;
            }
        };

        // Snapshot the *empty* composer before writing. This is the
        // reference the settle stage compares against to decide the
        // draft actually landed — see `UserTurnStage::AwaitDraft`.
        let empty = pane
            .parser
            .lock()
            .ok()
            .and_then(|p| composer_block_text(p.screen()))
            .unwrap_or_default();

        // Past this line bytes may be on the wire, so the dedupe entry
        // has to exist before the write — not after it, and not before
        // the refusals above, which must stay freely retryable.
        self.record_user_turn(target_id, from_pane, &normalized);

        let pane = match self
            .workspaces
            .get_mut(target_ws)
            .and_then(|w| w.panes.get_mut(&target_id))
        {
            Some(p) => p,
            None => {
                answer(Err(ipc::CodedError::new(
                    ipc::err_code::PANE_VANISHED,
                    format!("pane {target_id} vanished before delivery"),
                )));
                return;
            }
        };
        if let Err(e) = write_input_to_pane(pane, &payload, false) {
            answer(Err(e));
            return;
        }

        let now = Instant::now();
        self.pending_user_turns.push(PendingUserTurn {
            target_pane: target_id,
            reply: reply.take(),
            stage: UserTurnStage::AwaitDraft {
                ready_at: now + USER_TURN_SETTLE_DELAY,
                empty,
            },
            deadline: now + USER_TURN_DEADLINE,
        });
        self.dirty = true;
    }

    /// Advance every in-flight user turn by one frame. Called from the
    /// main event loop next to `flush_pending_codex_peer_messages`;
    /// cheap and a no-op when nothing is in flight.
    ///
    /// This never sleeps: each stage either makes progress against the
    /// current screen or leaves the delivery parked for the next frame.
    pub(crate) fn flush_pending_user_turns(&mut self) {
        if self.pending_user_turns.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut still_pending = Vec::with_capacity(self.pending_user_turns.len());
        for mut pending in std::mem::take(&mut self.pending_user_turns) {
            match self.advance_user_turn(&mut pending, now) {
                Some(outcome) => {
                    pending.answer(outcome);
                    self.dirty = true;
                }
                None => still_pending.push(pending),
            }
        }
        self.pending_user_turns = still_pending;
    }

    /// Advance one delivery by a frame, performing the I/O
    /// [`step_user_turn`] asks for. `Some(_)` is terminal.
    fn advance_user_turn(
        &mut self,
        pending: &mut PendingUserTurn,
        now: Instant,
    ) -> Option<std::result::Result<serde_json::Value, ipc::CodedError>> {
        let target_id = pending.target_pane;
        let Some((ws_index, _)) = self.resolve_pane_across_workspaces(&PaneRef::Id(target_id))
        else {
            return Some(Err(ipc::CodedError::new(
                ipc::err_code::PANE_VANISHED,
                format!(
                    "pane {target_id} closed mid-delivery; the body had already been written, so \
                     whether it was submitted is unknown"
                ),
            )));
        };
        let composer = self
            .workspaces
            .get(ws_index)
            .and_then(|w| w.panes.get(&target_id))
            .and_then(|pane| {
                pane.parser
                    .lock()
                    .ok()
                    .and_then(|p| composer_block_text(p.screen()))
            });

        match step_user_turn(&pending.stage, composer.as_deref(), now, pending.deadline) {
            UserTurnStep::Wait => None,
            UserTurnStep::Advance(stage) => {
                pending.stage = stage;
                None
            }
            UserTurnStep::Submit(stage) => {
                let Some(pane) = self
                    .workspaces
                    .get_mut(ws_index)
                    .and_then(|w| w.panes.get_mut(&target_id))
                else {
                    return Some(Err(stalled_error(target_id, "pane vanished before submit")));
                };
                // Enter goes out as its own write, deliberately: the
                // agent has to have taken the body as input before the
                // submit key arrives, which is exactly what a combined
                // write does not guarantee.
                if let Err(e) = write_input_to_pane(pane, b"\r", false) {
                    return Some(Err(e));
                }
                pending.stage = stage;
                None
            }
            UserTurnStep::Submitted => Some(Ok(submitted_payload(target_id))),
            UserTurnStep::Stalled(detail) => Some(Err(stalled_error(target_id, detail))),
        }
    }
}
