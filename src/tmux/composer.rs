//! Claude Code composer (input box) reads for the injected-send path.
//!
//! An `aoe send` / `POST /api/sessions/{id}/send` types into the same input
//! box a human may be mid-sentence in. Pasting into a composer that already
//! holds unsent text fuses the two and the trailing Enter submits the merged
//! blob under the human's name — their unsent words delivered as though they
//! had said them. Everything here exists to see that draft BEFORE anything is
//! typed, and to describe a refusal without echoing the draft.
//!
//! Pure functions over a raw (`capture-pane -e`) capture; no tmux calls, so
//! every shape is pinned by a fixture captured from a live pane.

use super::utils::strip_ansi;

/// Trailing non-empty rows scanned for the `❯` prompt.
const CLAUDE_COMPOSER_TAIL: usize = 10;

/// Rows below the `❯` prompt scanned for the composer box's closing rule.
/// Sized above any composer a human keeps open. Exhausting it means the
/// capture is not a composer shape we recognize, so the scan falls back to
/// the prompt row rather than reporting unrelated output as a draft.
const CLAUDE_COMPOSER_MAX_REGION: usize = 12;

/// How long a verified send waits for the composer to accept input before
/// sending anyway (a pane in a state the detector does not recognise must
/// still get its message; the verify phase recovers the Enter).
pub(crate) const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(7);
pub(crate) const READY_POLL: std::time::Duration = std::time::Duration::from_millis(200);
/// How long a verified send waits for a human's parked draft to clear before
/// refusing. Bounded separately from the ready window.
pub(crate) const DRAFT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Settle time between a submit and the capture that checks it landed.
pub(crate) const VERIFY_SETTLE: std::time::Duration = std::time::Duration::from_millis(1000);
/// Bare-Enter resubmits attempted for a message parked unsubmitted.
pub(crate) const MAX_SUBMIT_RETRIES: u32 = 3;
/// Rows captured for the ready / draft / stuck checks.
pub(crate) const VERIFY_CAPTURE_LINES: usize = 30;

/// A numbered menu option, optionally preceded by the `❯`/`>` selection
/// cursor: `❯ 1. Yes`, `2. No`, `3. No, and tell Claude ...`. The
/// folder-trust dialog, resume picker and approval menus all render their
/// cursor this way; none of them is a composer.
fn claude_line_is_numbered_choice(line: &str) -> bool {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix('❯')
        .or_else(|| trimmed.strip_prefix('>'))
        .map(str::trim_start)
        .unwrap_or(trimmed);
    let mut chars = rest.chars();
    matches!(chars.next(), Some('1'..='9')) && matches!(chars.next(), Some('.'))
}

/// The box-drawing rule that closes the Claude composer. A short run of the
/// drawing glyphs is enough, since a human does not readily type them and a
/// row made only of them is chrome; an ASCII run has to be long, because a
/// draft may legitimately hold a markdown `---`.
fn claude_line_is_horizontal_rule(trimmed: &str) -> bool {
    let len = trimmed.chars().count();
    let drawn = trimmed
        .chars()
        .all(|c| matches!(c, '─' | '━' | '┄' | '┅' | '┈' | '┉' | '╌' | '╍' | '═'));
    (drawn && len >= 3) || (trimmed.chars().all(|c| c == '-') && len >= 10)
}

/// Erases the renderer's dim (`SGR 2`) spans from a raw `-e` capture,
/// leaving everything else for `strip_ansi`. Claude Code paints its
/// autocomplete ghost text dim while the operator's own text never is, so a
/// dim span is chrome, not a draft — reading it as one refuses sends against
/// an EMPTY composer wearing a ghost (the dominant false positive in a live
/// census: 19 of 22 refusals). Dim state persists across newlines, since
/// tmux re-emits SGR only on change, and ends on SGR 0 or 22. A literal `2`
/// inside an extended-color argument list (`38;5;2`) is a color index, not
/// dim.
fn strip_dim_spans(raw: &str) -> String {
    fn sgr_dim_state(params: &str, dim: &mut bool) {
        let parts: Vec<Option<u16>> = params
            .split(';')
            .map(|p| {
                if p.is_empty() {
                    Some(0)
                } else {
                    p.parse().ok()
                }
            })
            .collect();
        let mut i = 0;
        while i < parts.len() {
            match parts[i] {
                Some(0) | Some(22) => *dim = false,
                Some(2) => *dim = true,
                Some(38) | Some(48) | Some(58) => match parts.get(i + 1) {
                    Some(Some(5)) => i += 2,
                    Some(Some(2)) => i += 4,
                    _ => {}
                },
                _ => {}
            }
            i += 1;
        }
    }

    let mut out = String::with_capacity(raw.len());
    let mut dim = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            let mut params = String::new();
            for p in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&p) {
                    if p == 'm' {
                        sgr_dim_state(&params, &mut dim);
                    }
                    break;
                }
                params.push(p);
            }
            continue;
        }
        if !dim || c == '\n' {
            out.push(c);
        }
    }
    out
}

/// The Claude Code composer is rendered and accepting input: a `❯` prompt
/// line in the trailing region that is NOT a numbered menu choice. This is
/// the "truly ready" signal a post-restart send must gate on. Measured on a
/// live boot: the pane's shell is replaced ~600ms before the composer
/// renders, and a paste landing in that gap keeps its text but loses its
/// submitting Enter (the boot-time terminal-mode churn consumes it), leaving
/// the message sitting unsubmitted.
pub(crate) fn claude_pane_input_ready(raw_content: &str) -> bool {
    let clean = strip_ansi(raw_content);
    clean
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<&str>>()
        .iter()
        .rev()
        .take(CLAUDE_COMPOSER_TAIL)
        .any(|line| {
            let trimmed = line.trim();
            // Claude Code renders the prompt as `❯` + U+00A0 NO-BREAK SPACE
            // when text follows (captured live), so a literal `"❯ "` match
            // misses any composer holding a draft — accept `❯` followed by
            // nothing or by any whitespace character.
            trimmed.strip_prefix('❯').is_some_and(|rest| {
                rest.is_empty() || rest.chars().next().is_some_and(char::is_whitespace)
            }) && !claude_line_is_numbered_choice(trimmed)
        })
}

/// The non-empty text a human has parked in the Claude composer, if any.
/// [`claude_pane_input_ready`] counts a composer holding a half-typed draft
/// as "ready", but injecting into it fuses the operator's text with the
/// injected message and submits the blob under their name. Numbered menu
/// cursors (`❯ 1. ...`) are dialogs, not drafts, and return `None`, as does
/// an empty composer.
///
/// Reads the composer's whole region — the `❯` row plus every continuation
/// row down to the box's closing rule — not just the prompt row. A draft
/// whose first line is empty (any paste beginning with a newline) renders
/// with its text on a continuation row and an empty `❯` row, so a
/// prompt-row-only read reports "no draft" and the caller injects straight
/// into it (observed in the wild on a live session). Blank rows inside the
/// box are interior, not content, and must not be filtered out before the
/// rows are located.
pub(crate) fn claude_composer_draft(raw_content: &str) -> Option<String> {
    let clean = strip_ansi(&strip_dim_spans(raw_content));
    let lines: Vec<&str> = clean.lines().collect();

    // Locate the `❯` row, scanning up from the bottom over the same trailing
    // window the readiness check uses. Blank rows are box interior, so they
    // do not consume the budget.
    let mut examined = 0usize;
    let mut prompt = None;
    for (row, line) in lines.iter().enumerate().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        examined += 1;
        if examined > CLAUDE_COMPOSER_TAIL {
            break;
        }
        if claude_line_is_numbered_choice(trimmed) {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('❯') {
            if rest.is_empty() || rest.chars().next().is_some_and(char::is_whitespace) {
                prompt = Some((row, rest.trim()));
                break;
            }
        }
    }
    let (prompt_row, prompt_text) = prompt?;

    let mut body = vec![prompt_text.to_string()];
    let mut closed = false;
    let mut row = prompt_row + 1;
    while row < lines.len() && row - prompt_row <= CLAUDE_COMPOSER_MAX_REGION {
        let trimmed = lines[row].trim();
        if claude_line_is_horizontal_rule(trimmed) {
            closed = true;
            break;
        }
        body.push(trimmed.to_string());
        row += 1;
    }
    if row >= lines.len() && body.len() == 1 {
        // The `❯` row is the last row captured (short pane, composer on the
        // bottom row): the end of the capture closes the box just as the rule
        // would. A capture that runs past continuation rows and never finds a
        // rule is a shape we cannot delimit, so it takes the fallback instead
        // of swallowing whatever followed the prompt.
        closed = true;
    }

    let joined = if closed {
        body.join("\n")
    } else {
        body[0].clone()
    };
    let draft = joined.trim();
    if draft.is_empty() {
        return None;
    }
    // With messages queued, Claude Code parks this dim hint on the `❯` line
    // (captured live) — UI chrome, not a draft.
    if draft == "Press up to edit queued messages" {
        return None;
    }
    Some(draft.to_string())
}

/// `message` is sitting unsubmitted in the Claude composer: the paste landed
/// but the submitting Enter was swallowed (the post-restart boot race). The
/// composer renders the draft on its `❯` prompt line, so match a bounded
/// prefix of the message's first line right after the cursor. Bounding the
/// prefix keeps line wrapping and narrow panes from breaking the match;
/// requiring the message's own text keeps an unrelated draft (someone else's
/// half-typed input) from triggering a recovery Enter that would submit text
/// this send does not own.
pub(crate) fn claude_message_stuck_in_composer(raw_content: &str, message: &str) -> bool {
    let first_line = message.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return false;
    }
    let prefix: String = first_line.chars().take(32).collect();
    let clean = strip_ansi(raw_content);
    clean
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<&str>>()
        .iter()
        .rev()
        .take(CLAUDE_COMPOSER_TAIL)
        .any(|line| {
            line.trim()
                .strip_prefix('❯')
                .map(str::trim_start)
                .is_some_and(|draft| draft.starts_with(&prefix))
        })
}

/// Explains a refused injection WITHOUT reproducing the operator's draft.
///
/// This is a TYPE and not a message because of where it has to travel. The
/// refusal leaves `send_keys_verified` as an `anyhow::Error`, the same
/// channel carrying genuine tmux transport failures, and the API layer at
/// the far end has to tell them apart: a refusal delivered nothing and says
/// so with certainty, whereas a transport failure leaves delivery unknown.
/// Prose cannot survive that boundary — it can only be logged — so without
/// the type the API reports both as `500 {"error":"tmux_error"}` and every
/// caller has to guess whether retrying would double-deliver.
///
/// It carries the draft's SIZE and never its text: enough for the operator
/// to recognise their own half-written message, useless to anyone else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedDraftRefusal {
    /// Characters in the parked draft. Never the draft itself.
    pub chars: usize,
    /// Lines in the parked draft, floored at 1 — a draft with no newline is
    /// still one line, and `0 lines` would name nothing recognisable.
    pub lines: usize,
}

impl ParkedDraftRefusal {
    pub(crate) fn from_draft(draft: &str) -> Self {
        Self {
            chars: draft.chars().count(),
            lines: draft.lines().count().max(1),
        }
    }
}

impl std::fmt::Display for ParkedDraftRefusal {
    /// The wording is a WIRE FORMAT: it lands in daemon logs and API
    /// `detail` fields that other tooling greps, and the tests pin it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (chars, lines) = (self.chars, self.lines);
        write!(
            f,
            "operator draft parked in the composer ({chars} chars, {lines} line(s), \
             content withheld); message NOT SENT. Injecting would submit the human's \
             unsent draft under their name. Retry once the composer is clear."
        )
    }
}

impl std::error::Error for ParkedDraftRefusal {}

/// The message's text reached the composer but its submitting Enter never
/// registered: after the bounded resubmit budget the text is still parked,
/// unsubmitted, at `❯`.
///
/// Returning `Ok` here — a WARN and nothing else — would make the API answer
/// `{"sent":true}` for a message the target never received; the parked text
/// then poisons every LATER send as a phantom "operator draft". Like
/// [`ParkedDraftRefusal`] this is a TYPE so it survives the anyhow boundary:
/// the API layer downcasts it to report the delivery state it actually knows
/// — typed, not submitted — instead of guessing between "sent" and
/// "transport broke".
///
/// Carries counts only, never the message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitUnconfirmed {
    /// Bare-Enter resubmits attempted before giving up.
    pub attempts: u32,
    /// True when the text wedging the composer is a PRIOR machine message
    /// (recognised against the send history) that bare Enter could not
    /// deliver either — the pane itself is not accepting submits.
    pub prior_machine_message: bool,
}

impl std::fmt::Display for SubmitUnconfirmed {
    /// Stable wording: it lands in daemon logs and API `detail` fields.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let which = if self.prior_machine_message {
            "a prior machine message"
        } else {
            "the message"
        };
        write!(
            f,
            "{which} is typed into the composer but still UNSUBMITTED after \
             {} bare-Enter resend(s); the pane is not accepting submits. \
             Delivery state: typed, not submitted — a retry of the same \
             message will submit the parked copy instead of double-pasting.",
            self.attempts
        )
    }
}

impl std::error::Error for SubmitUnconfirmed {}

/// What the text parked in the composer IS, judged against what this send is
/// carrying and what the machine recently sent to this pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MachineDraft {
    /// The parked text is THIS send's own message (a prior attempt whose
    /// Enter was swallowed): a bare Enter completes this delivery.
    Outgoing,
    /// The parked text is an EARLIER machine send (a swallowed-Enter wake or
    /// message): submit it bare to deliver it, then proceed with this send.
    Prior,
    /// Unrecognised: a human's unsent words until proven otherwise. Refuse,
    /// never submit.
    No,
}

/// Whitespace-collapsed form: the composer renders a message wrapped to pane
/// width, so byte equality never holds across a re-wrap.
fn normalize_draft_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A rendered draft "is" a machine message when, whitespace-normalized, it
/// equals the message — or is a ≥40-char prefix/suffix of it (the capture
/// window can clip a long draft at either end). The length floor keeps a
/// short human note that happens to open like a machine message from being
/// submitted under the recovery; an unmatched draft stays a human draft and
/// is refused, so a false negative costs a retry while a false positive
/// would forge authorship. Judged against the OUTGOING text first: its parked
/// copy means this very delivery is one Enter from complete.
pub(crate) fn classify_machine_draft(
    draft: &str,
    outgoing: &str,
    machine_history: &[String],
) -> MachineDraft {
    fn matches_one(draft_n: &str, msg_n: &str) -> bool {
        if draft_n.is_empty() || msg_n.is_empty() {
            return false;
        }
        if draft_n == msg_n {
            return true;
        }
        draft_n.chars().count() >= 40 && (msg_n.starts_with(draft_n) || msg_n.ends_with(draft_n))
    }
    let draft_n = normalize_draft_text(draft);
    if matches_one(&draft_n, &normalize_draft_text(outgoing)) {
        return MachineDraft::Outgoing;
    }
    if machine_history
        .iter()
        .any(|m| matches_one(&draft_n, &normalize_draft_text(m)))
    {
        return MachineDraft::Prior;
    }
    MachineDraft::No
}

/// Is the Claude composer clear for a queued machine delivery right now?
///
/// The daemon drains the server-owned prompt queue into a terminal session
/// only through this gate (`server::send_queue`): the composer must be
/// rendered and accepting input, and hold nothing a human typed. Text
/// already parked there is tolerated only when it is provably machine text
/// (`classify_machine_draft`): the queued message's own earlier attempt, or
/// a prior machine send whose Enter was swallowed, which the verified send
/// then completes with a bare Enter. Anything else is an operator's unsent
/// draft and the delivery waits. It never types beside the draft and never
/// submits it, so the WO#1872 guarantee holds for queued traffic exactly as
/// it does for a live send. A dialog or a booting pane is "not ready", not
/// "clear".
pub(crate) fn composer_clear_for_delivery(
    raw_content: &str,
    outgoing: &str,
    machine_history: &[String],
) -> bool {
    if !claude_pane_input_ready(raw_content) {
        return false;
    }
    match claude_composer_draft(raw_content) {
        None => true,
        Some(draft) => !matches!(
            classify_machine_draft(&draft, outgoing, machine_history),
            MachineDraft::No
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_ready_on_fresh_composer() {
        // A freshly booted pane whose composer has rendered: the lone `❯`
        // prompt line between the box rules, bypass footer below. Captured
        // from a live boot probe.
        let pane = "\
 ▐▛███▜▌   Claude Code v2.1.197
▝▜█████▛▘  Sonnet 4.5 · Claude Max
  ▘▘ ▝▝    /home/user/project

────────────────────────────────────────────────────────
 ❯
────────────────────────────────────────────────────────
   ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert!(claude_pane_input_ready(pane));
    }

    #[test]
    fn input_ready_false_on_booting_banner() {
        // Mid-boot: the version banner is up but the composer has not
        // rendered yet. A send now is exactly the race being fixed.
        let pane = "\
 ▐▛███▜▌   Claude Code v2.1.197
▝▜█████▛▘  Sonnet 4.5 · Claude Max
  ▘▘ ▝▝    /home/user/project";
        assert!(!claude_pane_input_ready(pane));
    }

    #[test]
    fn input_ready_false_on_empty_pane() {
        assert!(!claude_pane_input_ready(""));
        assert!(!claude_pane_input_ready("\n\n\n"));
    }

    #[test]
    fn input_ready_false_on_trust_dialog() {
        // The folder-trust dialog renders a `❯` cursor, but on a numbered
        // menu choice: pasting here types into a menu, not the composer.
        let pane = "\
 Do you trust the files in this folder?

 /home/user/project

 ❯ 1. Yes, I trust this folder
   2. No, exit";
        assert!(!claude_pane_input_ready(pane));
    }

    #[test]
    fn input_ready_false_on_resume_picker() {
        let pane = "\
  Resuming the full session will consume a substantial portion of your usage limits. We recommend resuming from a summary.
  ❯ 1. Resume from summary (recommended)
    2. Resume full session as-is";
        assert!(!claude_pane_input_ready(pane));
    }

    #[test]
    fn input_ready_with_ansi_and_running_turn() {
        // Mid-turn the composer stays rendered below the spinner (steering
        // input is legitimate), and live capture carries ANSI. Ready.
        let pane = "\x1b[2m✶ Working… (4s · ↓ 88 tokens)\x1b[0m\n\
────────────────────────────────\n\
\x1b[1m ❯ \x1b[0m\n\
────────────────────────────────\n\
   ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert!(claude_pane_input_ready(pane));
    }

    #[test]
    fn input_ready_nbsp_after_prompt() {
        // Captured live: when a draft is parked, Claude Code renders the
        // prompt as `❯` + U+00A0 NO-BREAK SPACE, not an ASCII space. The
        // ready check must not demand `"❯ "` literally, or a pane holding a
        // draft is never "ready" — the send then rides the ready-timeout
        // branch and the paste fuses with the operator's text.
        let pane = "\
────────────────────────────────────────────────────────
\x1b[38;5;246m❯\u{a0}\x1b[39mOPERATOR DRAFT half-typed
────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert!(claude_pane_input_ready(pane));
        assert_eq!(
            claude_composer_draft(pane),
            Some("OPERATOR DRAFT half-typed".to_string())
        );
    }

    #[test]
    fn stuck_in_composer_on_swallowed_enter() {
        // The boot race outcome observed live: the paste landed in the
        // composer but the trailing Enter was consumed by boot-time
        // terminal-mode churn, so the message sits unsubmitted after `❯`.
        let pane = "\
────────────────────────────────────────────────────────
 ❯ RACE-PROBE-MESSAGE this text was pasted during boot
────────────────────────────────────────────────────────
   ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert!(claude_message_stuck_in_composer(
            pane,
            "RACE-PROBE-MESSAGE this text was pasted during boot"
        ));
        // Someone else's draft must not trigger a recovery Enter.
        assert!(!claude_message_stuck_in_composer(
            pane,
            "a different message"
        ));
        // A submitted message leaves the composer empty.
        assert!(!claude_message_stuck_in_composer(
            "──────\n ❯ \n──────",
            "RACE-PROBE-MESSAGE this text was pasted during boot"
        ));
        assert!(!claude_message_stuck_in_composer(pane, ""));
    }

    #[test]
    fn draft_returns_human_draft() {
        // A half-typed approval parked at the composer. An injection landing
        // now would fuse with it; the daemon must see the draft.
        let pane = "\
────────────────────────────────────────────────────────
 ❯ send h-1c2bc36f
────────────────────────────────────────────────────────
   ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(
            claude_composer_draft(pane),
            Some("send h-1c2bc36f".to_string())
        );
    }

    #[test]
    fn draft_none_on_empty_composer() {
        let pane = "\
────────────────────────────────\n ❯ \n────────────────────────────────";
        assert_eq!(claude_composer_draft(pane), None);
        let bare = "────────\n ❯\n────────";
        assert_eq!(claude_composer_draft(bare), None);
    }

    #[test]
    fn draft_none_on_numbered_menu() {
        let pane = "\
 Do you trust the files in this folder?

 ❯ 1. Yes, I trust this folder
   2. No, exit";
        assert_eq!(claude_composer_draft(pane), None);
    }

    #[test]
    fn draft_none_without_composer() {
        assert_eq!(claude_composer_draft(""), None);
        assert_eq!(
            claude_composer_draft(" ▐▛███▜▌   Claude Code v2.1.197"),
            None
        );
    }

    #[test]
    fn draft_none_on_queued_messages_hint() {
        let pane = "──────\n❯ Press up to edit queued messages\n──────";
        assert_eq!(claude_composer_draft(pane), None);
    }

    #[test]
    fn draft_strips_ansi() {
        let pane = "\x1b[1m ❯ \x1b[0msend h-1c2bc36f\n   ⏵⏵ bypass permissions on";
        assert_eq!(
            claude_composer_draft(pane),
            Some("send h-1c2bc36f".to_string())
        );
    }

    #[test]
    fn draft_none_on_dim_ghost_suggestion() {
        // Captured live: an EMPTY composer over which Claude Code paints a
        // dim autocomplete ghost (`\x1b[2m...`). The operator typed none of
        // it; dim spans are the renderer's and must strip before the draft
        // is read.
        let pane = concat!(
            "\x1b[38;5;244m────────────────────────────────────────\x1b[39m\n",
            "\x1b[38;5;246m❯ \x1b[39m\x1b[2mdid the preview job come back\x1b[0m\n",
            "\x1b[38;5;244m────────────────────────────────────────\x1b[39m\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)"
        );
        assert_eq!(claude_composer_draft(pane), None);
    }

    #[test]
    fn draft_keeps_typed_text_before_dim_ghost() {
        // Half-typed word plus the dim completion Claude Code offers for it.
        // The typed prefix is the operator's and must survive; the ghost
        // tail must not.
        let pane = concat!(
            "❯ wake \x1b[2mthe other session and hand it off\x1b[0m\n",
            "\x1b[38;5;244m────────────────────────────────────────\x1b[39m"
        );
        assert_eq!(claude_composer_draft(pane), Some("wake".to_string()));
    }

    #[test]
    fn draft_dim_ghost_wraps_across_rows() {
        // tmux emits SGR state only on change, so a wrapped ghost carries
        // its `\x1b[2m` from the prompt row across the continuation row with
        // no re-emit. Dim state must persist across newlines.
        let pane = concat!(
            "❯ \x1b[2mdid the preview job finish and did\n",
            "anything need attention afterwards\x1b[0m\n",
            "\x1b[38;5;244m────────────────────────────────────────\x1b[39m"
        );
        assert_eq!(claude_composer_draft(pane), None);
    }

    #[test]
    fn draft_sgr22_ends_dim_span() {
        let pane = "❯ \x1b[2mghost \x1b[22mtyped tail\n────────────";
        assert_eq!(claude_composer_draft(pane), Some("typed tail".to_string()));
    }

    #[test]
    fn draft_color_sgr_is_not_dim() {
        // A colored draft is still a draft. `\x1b[32m` must not read as dim,
        // and neither may `\x1b[38;5;2m`, where the literal `2` is an
        // extended-color index argument, not SGR 2.
        let green = "❯ \x1b[32msend it\x1b[0m\n────────────";
        assert_eq!(claude_composer_draft(green), Some("send it".to_string()));
        let indexed = "❯ \x1b[38;5;2mrun the green build\x1b[39m\n────────────";
        assert_eq!(
            claude_composer_draft(indexed),
            Some("run the green build".to_string())
        );
    }

    #[test]
    fn draft_compound_sgr_sets_dim() {
        let pane = "❯ \x1b[2;3mitalic dim ghost\x1b[0m\n────────────";
        assert_eq!(claude_composer_draft(pane), None);
    }

    #[test]
    fn draft_sees_text_on_continuation_row() {
        // Captured live: the composer's `❯` row is EMPTY and the operator's
        // parked text sits on a continuation row two rows below it, which is
        // what Claude Code renders for a draft that begins with a newline.
        // Reading only the `❯` row reports "no draft" and the send fuses.
        let pane = concat!(
            "────────────────────────────────────────\n",
            "❯ \n",
            "\n",
            "\n",
            "  [Pasted text #5 +83 lines]\n",
            "────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)"
        );
        assert_eq!(
            claude_composer_draft(pane),
            Some("[Pasted text #5 +83 lines]".to_string())
        );
    }

    #[test]
    fn draft_joins_multi_row_draft() {
        // Both rows belong to the operator, so both are reported. The
        // refusal payload's char and line counts derive from this string.
        let pane = concat!(
            "────────────────────────────────────────\n",
            "❯ alpha one\n",
            "  beta two\n",
            "────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)"
        );
        assert_eq!(
            claude_composer_draft(pane),
            Some("alpha one\nbeta two".to_string())
        );
    }

    #[test]
    fn draft_none_on_empty_multi_row_composer() {
        // Captured live: a genuinely EMPTY composer that still renders two
        // blank continuation rows. Scanning the whole region must not invent
        // a draft here.
        let pane = concat!(
            "────────────────────────────────────────\n",
            "❯ \n",
            "\n",
            "\n",
            "────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)"
        );
        assert_eq!(claude_composer_draft(pane), None);
    }

    #[test]
    fn draft_stops_at_the_closing_rule() {
        // The chrome under the composer is not the operator's text. A region
        // scan that ran past the closing rule would report the permissions
        // footer as a parked draft and refuse every send to every pane.
        let pane = concat!(
            "────────────────────────────────────────\n",
            "❯ \n",
            "────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
            "  ⏸ 2 background tasks"
        );
        assert_eq!(claude_composer_draft(pane), None);
    }

    #[test]
    fn draft_reads_bottom_row_composer() {
        // Captured live: on a short pane the composer is the bottom row of
        // the screen, so the capture ends without a closing rule. The `❯`
        // row still carries the draft.
        let pane = concat!(
            "   … +55 completed\n",
            "                    new task? /clear to save 151.2k tokens\n",
            "─────────────────────────── draft ──\n",
            "❯ go with B on marketing"
        );
        assert_eq!(
            claude_composer_draft(pane),
            Some("go with B on marketing".to_string())
        );
    }

    #[test]
    fn draft_markdown_dashes_are_content_not_a_rule() {
        // A short ASCII `---` inside a draft is markdown; only a long run
        // closes the box.
        let pane = concat!(
            "❯ heading\n",
            "---\n",
            "body\n",
            "────────────────────────────────────────\n"
        );
        assert_eq!(
            claude_composer_draft(pane),
            Some("heading\n---\nbody".to_string())
        );
    }

    /// A refusal is reported to the caller and lands in the daemon log. The
    /// draft is the human's unsent text; echoing it there republishes it to
    /// every agent that reads the error.
    #[test]
    fn refusal_never_echoes_the_draft() {
        let draft = "yes, authorize the vendor bump and send the escalation emails";
        let msg = ParkedDraftRefusal::from_draft(draft).to_string();
        assert!(!msg.contains(draft), "refusal leaked the draft: {msg:?}");
        assert!(
            !msg.contains("vendor"),
            "refusal leaked draft words: {msg:?}"
        );
    }

    /// Not-sent must be unambiguous. A caller that reads this as "maybe
    /// sent" will retry and double-deliver, or drop the message silently.
    #[test]
    fn refusal_states_the_message_was_not_delivered() {
        let msg = ParkedDraftRefusal::from_draft("half a sentence").to_string();
        let low = msg.to_lowercase();
        assert!(low.contains("not sent"), "{msg:?}");
        assert!(low.contains("draft"), "{msg:?}");
    }

    /// Without a size the operator cannot tell a stray keystroke from a
    /// paragraph they are mid-way through writing.
    #[test]
    fn refusal_reports_the_drafts_size_not_its_text() {
        let draft = "line one\nline two";
        let refusal = ParkedDraftRefusal::from_draft(draft);
        assert_eq!(17, refusal.chars);
        assert_eq!(2, refusal.lines);
        assert!(refusal.to_string().contains("17 chars, 2 line(s)"));
        assert_eq!(1, ParkedDraftRefusal::from_draft("no newline here").lines);
    }

    /// The refusal travels to the API layer as an `anyhow::Error`, shared
    /// with genuine tmux transport failures. Carrying it as a downcastable
    /// type is what lets the API tell "nothing was sent" from "delivery
    /// unknown".
    #[test]
    fn refusal_survives_the_anyhow_boundary_as_a_type() {
        let err: anyhow::Error = ParkedDraftRefusal::from_draft("line one\nline two").into();
        let recovered = err
            .downcast_ref::<ParkedDraftRefusal>()
            .expect("refusal must remain identifiable after crossing anyhow");
        assert_eq!(17, recovered.chars);
        assert_eq!(2, recovered.lines);
        // ...and a transport failure must NOT be mistakable for a refusal.
        let err = anyhow::anyhow!("tmux: no server running on /tmp/tmux-501/default");
        assert!(err.downcast_ref::<ParkedDraftRefusal>().is_none());
    }

    /// Same boundary guarantee for the unconfirmed-submit outcome, and the
    /// two never downcast to each other.
    #[test]
    fn submit_unconfirmed_survives_anyhow_and_pins_its_prose() {
        let err: anyhow::Error = SubmitUnconfirmed {
            attempts: 3,
            prior_machine_message: false,
        }
        .into();
        let recovered = err
            .downcast_ref::<SubmitUnconfirmed>()
            .expect("unconfirmed submit must remain identifiable after crossing anyhow");
        assert_eq!(3, recovered.attempts);
        let prose = recovered.to_string();
        assert!(prose.contains("typed, not submitted"), "{prose}");
        assert!(prose.contains("3 bare-Enter resend(s)"), "{prose}");
        assert!(SubmitUnconfirmed {
            attempts: 1,
            prior_machine_message: true,
        }
        .to_string()
        .starts_with("a prior machine message"));
        let refusal: anyhow::Error = ParkedDraftRefusal::from_draft("draft").into();
        assert!(refusal.downcast_ref::<SubmitUnconfirmed>().is_none());
        assert!(err.downcast_ref::<ParkedDraftRefusal>().is_none());
    }

    /// The authorship line: a parked draft is submitted bare ONLY when it
    /// provably matches machine text (this send's own message, or a recent
    /// machine send to this pane). Everything else stays a human draft and
    /// is refused, so a false negative costs one retry while a false
    /// positive would forge authorship.
    #[test]
    fn classify_machine_draft_cases() {
        let outgoing = "STATUS: shipped — daemon rebuilt, verify green, see commit abc1234";
        let history = vec![
            "wake up: pick up what you were doing".to_string(),
            "TASK-1500: audit the pane liveness detector and report".to_string(),
        ];
        let long_prior = &history[1];
        let cases = [
            // A human's words match nothing → refused, never submitted.
            ("just checking in on this", MachineDraft::No),
            // Exact copy of the outgoing text: this send's own swallowed Enter.
            (outgoing, MachineDraft::Outgoing),
            // The composer re-wraps to pane width; whitespace-normalized
            // equality still recognises the outgoing text.
            (
                "STATUS: shipped — daemon rebuilt,\n  verify green, see\n  commit abc1234",
                MachineDraft::Outgoing,
            ),
            // Exact match against history → a prior machine send.
            ("wake up: pick up what you were doing", MachineDraft::Prior),
            // A short prefix of the outgoing text (< 40 chars) is NOT enough:
            // a human note may open with the same words.
            ("STATUS: shipped", MachineDraft::No),
            // A ≥40-char clipped tail of a history entry still identifies
            // the machine message.
            (&long_prior[long_prior.len() - 41..], MachineDraft::Prior),
            // Empty draft is never a machine message.
            ("", MachineDraft::No),
        ];
        for (draft, expected) in cases {
            assert_eq!(
                classify_machine_draft(draft, outgoing, &history),
                expected,
                "{draft:?}"
            );
        }
        // Outgoing wins over Prior when the same text is both: the delivery
        // in flight is the one a bare Enter completes.
        let mut history_with_self = history.clone();
        history_with_self.push(outgoing.to_string());
        assert_eq!(
            classify_machine_draft(outgoing, outgoing, &history_with_self),
            MachineDraft::Outgoing
        );
    }
}

#[cfg(test)]
mod delivery_gate_tests {
    use super::composer_clear_for_delivery;

    const EMPTY: &str = "\
────────────────────────────────────────────────────────
 ❯ 
────────────────────────────────────────────────────────
   ⏵⏵ bypass permissions on (shift+tab to cycle)";

    fn with_draft(draft: &str) -> String {
        format!(
            "────────────────────────────────────────────────────────\n\
             \x1b[38;5;246m❯\u{a0}\x1b[39m{draft}\n\
             ────────────────────────────────────────────────────────\n\
               ⏵⏵ bypass permissions on (shift+tab to cycle)"
        )
    }

    #[test]
    fn empty_composer_is_clear() {
        assert!(composer_clear_for_delivery(EMPTY, "STATUS: shipped", &[]));
    }

    #[test]
    fn operator_draft_blocks_delivery() {
        // The whole point: a human's half-typed words hold the queue.
        let pane = with_draft("yes, authorize the vendor bump and send it");
        assert!(!composer_clear_for_delivery(&pane, "STATUS: shipped", &[]));
        // ...even when the queue holds other machine text.
        assert!(!composer_clear_for_delivery(
            &pane,
            "STATUS: shipped",
            &["WO#1897 queued report body".to_string()]
        ));
    }

    #[test]
    fn own_parked_copy_is_clear() {
        // A prior attempt typed the message but lost its Enter: the verified
        // send completes it with a bare Enter, so the gate opens.
        let msg = "STATUS: shipped - WO#1897 queue landed, build 9f221e45 live on the VM";
        let pane = with_draft(msg);
        assert!(composer_clear_for_delivery(&pane, msg, &[]));
    }

    #[test]
    fn prior_machine_message_is_clear() {
        let earlier = "STATUS: blocked - WO#1889 gateway vocabulary still emits digit tokens";
        let pane = with_draft(earlier);
        assert!(composer_clear_for_delivery(
            &pane,
            "STATUS: shipped",
            &[earlier.to_string()]
        ));
    }

    #[test]
    fn not_ready_pane_is_not_clear() {
        // A numbered dialog (no `❯` prompt row): nothing may be typed.
        let dialog = "Do you want to proceed?\n  1. Yes\n  2. No\n";
        assert!(!composer_clear_for_delivery(dialog, "STATUS: shipped", &[]));
        assert!(!composer_clear_for_delivery("", "STATUS: shipped", &[]));
    }
}
