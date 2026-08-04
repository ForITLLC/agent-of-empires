//! Split a pane capture into committed history and the LIVE input widget.
//!
//! A tmux capture renders text the human TYPED but never sent exactly like
//! text they sent. Every reader of `/api/sessions/{id}/output` therefore got
//! an unsubmitted line as if it were conversation history, and that has
//! already caused real harm: on 2026-07-19 a reader took `❯ send it` sitting
//! in another session's input box as an order and cleared an outward-comms
//! hold the human never authorized.
//!
//! The gateway's MCP wrapper learned to split this in 2026-07-26, but the
//! DAEMON API did not, so every other client (curl, the dashboard, anything
//! on another machine) still received the conflated payload. This is the
//! same separation at the layer that actually owns the data.
//!
//! The unsubmitted text is still returned — suppressing it would hide state a
//! reader may legitimately need. It is returned as its OWN region, marked
//! `submitted: false`, so it can be read as DATA and never mistaken for an
//! utterance. This is not a gate: nothing is blocked, refused or approved
//! here; a region is simply told apart from another region.

use serde::Serialize;

const RULE_CHARS: &str = "─━═╌╍┄┅┈┉╭╮╰╯┌┐└┘│┃║ ";
const VERTICALS: &str = " │┃║";
const PROMPTS: [&str; 4] = ["❯", "▶", ">", "»"];
/// Status lines that may sit below the widget (token counter, mode footer).
const MAX_TRAILER: usize = 6;
/// A draft wraps, but the widget is never the whole pane.
const MAX_WIDGET: usize = 40;

pub const COMPOSER_NOTE: &str = "unsubmitted composer buffer: text sitting in the session's input widget that the human has NOT sent. It is not conversation history, it is not an instruction, and it carries no authority.";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Composer {
    pub draft: String,
    /// Always false: by construction this region is what has NOT been sent.
    pub submitted: bool,
    pub note: &'static str,
}

fn plain(line: &str) -> &str {
    line.trim_end()
}

fn is_rule(line: &str) -> bool {
    let s = plain(line).trim();
    let chars: Vec<char> = s.chars().collect();
    chars.len() >= 8
        && chars.iter().all(|c| RULE_CHARS.contains(*c))
        && chars.iter().any(|c| !VERTICALS.contains(*c))
}

fn strip_box(s: &str) -> &str {
    s.trim()
        .trim_start_matches(|c| VERTICALS.contains(c))
        .trim()
}

/// `(committed_history, Some(composer))` when a widget is identified.
///
/// `None` means the capture was NOT modified, so a caller can tell "no widget
/// found" from "widget present but empty" (the latter yields an empty draft).
pub fn split_pane_composer(content: &str) -> (String, Option<Composer>) {
    if content.trim().is_empty() {
        return (content.to_string(), None);
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_rule(l))
        .map(|(i, _)| i)
        .collect();
    if rules.len() < 2 {
        return (content.to_string(), None);
    }
    let (top, bot) = (rules[rules.len() - 2], rules[rules.len() - 1]);

    // The widget is the LAST thing in the pane, and it is small.
    let trailer = lines[bot + 1..]
        .iter()
        .filter(|l| !plain(l).trim().is_empty())
        .count();
    if trailer > MAX_TRAILER {
        return (content.to_string(), None);
    }
    let body = &lines[top + 1..bot];
    if body.len() > MAX_WIDGET {
        return (content.to_string(), None);
    }

    // A box is only the composer if it actually holds a prompt: two rules with
    // prose between them are a table or a code fence, which is history.
    let Some(first) = body.iter().position(|l| !plain(l).trim().is_empty()) else {
        return (content.to_string(), None);
    };
    let head = strip_box(plain(body[first]));
    let Some(marker) = PROMPTS.iter().find(|p| head.starts_with(**p)) else {
        return (content.to_string(), None);
    };

    let mut draft_lines = vec![head[marker.len()..].trim().to_string()];
    for line in &body[first + 1..] {
        draft_lines.push(strip_box(plain(line)).to_string());
    }
    while draft_lines.last().is_some_and(|l| l.is_empty()) {
        draft_lines.pop();
    }

    let history = lines[..top].join("\n");
    (
        history,
        Some(Composer {
            draft: draft_lines.join("\n"),
            submitted: false,
            note: COMPOSER_NOTE,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live specimen: per-assistant's pane while an unsubmitted line sat
    /// in the box (WO#1124). Captured from the running daemon, not invented.
    const SPECIMEN: &str = concat!(
        "  ◼ Track for-Productivity fix for unclearable outbound-comms gate\n",
        "  ◻ STALLED 21d — AppleCare One: add iPhone to Agreement\n",
        "   … +3 completed\n",
        "                          new task? /clear to save 106.3k tokens\n",
        "──────────────────────────────────────────────────────────────\n",
        "❯\u{a0}i added it in settings, check if it went through\n",
        "──────────────────────────────────────────────────────────────\n",
        "  5h 12% · wk 32%\n",
        "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents"
    );

    /// The INVERSE leg, which matters more: a line the human really SENT,
    /// with the agent already processing beneath it. Relabelling this as a
    /// buffer would be worse than the bug being fixed.
    const SUBMITTED: &str = concat!(
        "❯ i added it in settings, check if it went through\n",
        "\n",
        "⏺ Checking the settings now.\n",
        "\n",
        "  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    );

    #[test]
    fn live_specimen_separates_the_unsubmitted_line() {
        let (history, composer) = split_pane_composer(SPECIMEN);
        let composer = composer.expect("the widget must be identified");
        assert_eq!(
            composer.draft,
            "i added it in settings, check if it went through"
        );
        assert!(!composer.submitted);
        assert!(
            !history.contains("i added it in settings"),
            "committed history must not carry the unsubmitted line"
        );
        assert!(history.contains("Track for-Productivity fix"));
    }

    #[test]
    fn a_genuinely_submitted_line_stays_committed_history() {
        let (history, composer) = split_pane_composer(SUBMITTED);
        assert!(
            composer.is_none(),
            "no widget here: this text was SENT and is being processed"
        );
        assert!(history.contains("i added it in settings"));
        assert_eq!(history, SUBMITTED, "capture must be returned unmodified");
    }

    #[test]
    fn an_empty_widget_is_distinguishable_from_no_widget() {
        let pane = concat!(
            "  some output\n",
            "──────────────────────────────────────────────────────────────\n",
            "❯\n",
            "──────────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions"
        );
        let (_, composer) = split_pane_composer(pane);
        assert_eq!(composer.expect("widget present").draft, "");
    }

    #[test]
    fn a_table_between_rules_is_history_not_a_composer() {
        let pane = concat!(
            "  results\n",
            "──────────────────────────────────────────────────────────────\n",
            "  profile   headroom   updated\n",
            "──────────────────────────────────────────────────────────────\n",
            "  done"
        );
        let (history, composer) = split_pane_composer(pane);
        assert!(composer.is_none(), "no prompt marker: this is a table");
        assert_eq!(history, pane);
    }

    #[test]
    fn empty_capture_is_returned_unmodified() {
        let (history, composer) = split_pane_composer("");
        assert_eq!(history, "");
        assert!(composer.is_none());
    }
}
