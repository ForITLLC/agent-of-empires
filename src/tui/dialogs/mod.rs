//! TUI dialog components

use crossterm::event::KeyCode;
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear};

use crate::tui::styles::Theme;

pub mod attach_project;
mod changelog;
mod cheats;
mod command_palette;
mod confirm;
mod context_menu;
mod custom_instruction;
mod delete_options;
mod group_delete_options;
mod hooks_install;
mod info;
mod intro;
mod new_session;
mod no_agents;
mod option_picker;
mod permission_response;
mod plugin_manager;
mod profile_picker;
mod project_session_picker;
mod projects;
mod rename;
mod repo_trust;
mod restart;
mod send_message;
mod serve;
mod skills_manager;
mod snooze_duration;
mod telemetry_consent;
mod tips;
mod tool_picker;
mod update_confirm;
mod worktree_name;

pub use attach_project::AttachProjectDialog;
pub use changelog::ChangelogDialog;
pub use command_palette::{
    builtin_commands, CommandPaletteDialog, PaletteAction, PaletteCommand, PaletteGroup,
};
pub use confirm::ConfirmDialog;
pub use context_menu::{ContextMenuAction, ContextMenuDialog};
pub use custom_instruction::CustomInstructionDialog;
pub use delete_options::{DeleteDialogConfig, DeleteOptions, UnifiedDeleteDialog};
pub use group_delete_options::{GroupDeleteOptions, GroupDeleteOptionsDialog};
pub use hooks_install::HooksInstallDialog;
pub use info::InfoDialog;
pub use intro::{IntroDialog, IntroOutcome};
pub(crate) use new_session::project_picker_label;
pub use new_session::{NewSessionData, NewSessionDialog};
pub use no_agents::{NoAgentsAction, NoAgentsDialog};
pub use option_picker::{GroupPickerDialog, SortPickerDialog};
pub use permission_response::{PermissionResponseChoice, PermissionResponseDialog};
pub use plugin_manager::PluginManagerDialog;
pub use profile_picker::{ProfileEntry, ProfilePickerAction, ProfilePickerDialog};
pub use project_session_picker::ProjectSessionPickerDialog;
pub use projects::ProjectsDialog;
pub use rename::{RenameData, RenameDialog, RenameMode};
pub use repo_trust::{RepoTrustAction, RepoTrustDialog};
pub use restart::{RestartData, RestartDialog};
pub use send_message::SendMessageDialog;
pub(crate) use serve::start_local_daemon_and_wait;
pub use serve::{ServeAction, ServeView};
pub use skills_manager::SkillsManagerDialog;
pub use snooze_duration::SnoozeDurationDialog;
pub use telemetry_consent::TelemetryConsentDialog;
pub use tips::{TipsDialog, TipsOutcome};
pub use tool_picker::ToolPickerDialog;
pub use update_confirm::UpdateConfirmDialog;
pub use worktree_name::{WorktreeNameData, WorktreeNameDialog};

pub enum DialogResult<T> {
    Continue,
    Cancel,
    Submit(T),
}

/// Insert pasted text into a single-line `Input`, stripping newlines so a paste cannot submit.
pub fn paste_into_input(input: &mut tui_input::Input, text: &str) {
    for ch in text.chars().filter(|c| *c != '\n' && *c != '\r') {
        input.handle(tui_input::InputRequest::InsertChar(ch));
    }
}

/// Center a dialog of given size within an area, clamping to fit.
pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

pub fn contains(area: Rect, col: u16, row: u16) -> bool {
    area.contains(Position::from((col, row)))
}

/// Index of the one-line-per-item row under `(col, row)` in `list`, if any.
pub fn row_index(list: Rect, col: u16, row: u16, len: usize) -> Option<usize> {
    let idx = contains(list, col, row).then(|| (row - list.y) as usize)?;
    (idx < len).then_some(idx)
}

/// Rounded, accent-bordered dialog block with a bold `theme.title` title.
pub fn dialog_block<'a>(title: impl Into<Line<'a>>, theme: &Theme) -> Block<'a> {
    toned_dialog_block(title, theme.accent, theme.title)
}

/// Rounded dialog block in an explicit tone: `border` frames it, `title_fg`
/// colors the bold title. Destructive dialogs pass `theme.error`.
pub fn toned_dialog_block<'a>(
    title: impl Into<Line<'a>>,
    border: Color,
    title_fg: Color,
) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(title)
        .title_style(Style::default().fg(title_fg).bold())
}

/// Cells a fitted dialog keeps clear on each side of the frame, so it still
/// reads as a box floating over the view when the frame is smaller than the
/// dialog's natural size.
const FIT_MARGIN: u16 = 1;

/// Smallest box `fit_dialog` hands back along either axis: a border plus one
/// cell of content. A frame smaller than this yields the whole frame.
const FIT_MIN: u16 = 3;

fn fit_extent(want: u16, have: u16) -> u16 {
    let max = have.saturating_sub(2 * FIT_MARGIN).max(FIT_MIN).min(have);
    want.min(max)
}

/// Columns a dialog of natural width `width` gets in `area` — the rule
/// `fit_dialog` applies, for callers that wrap content before they know
/// their height.
pub fn fit_width(area: ratatui::layout::Rect, width: u16) -> u16 {
    fit_extent(width, area.width)
}

/// Fit a dialog of natural size `width` x `height` to `area` and centre it.
///
/// Unlike `centered_rect`, which clamps to the whole area, a dialog that
/// does not fit shrinks to the area minus `FIT_MARGIN` on each side (never
/// below `FIT_MIN`, never beyond the area), so it stays a box inside the
/// frame instead of running edge to edge. Callers pass the live frame area
/// on every render — nothing is sized when the dialog opens — so a terminal
/// (or tmux client) that shrinks or grows while the dialog is up re-fits it
/// on the next frame. All arithmetic saturates: the result always lies
/// inside `area`, even a degenerate one.
pub fn fit_dialog(area: ratatui::layout::Rect, width: u16, height: u16) -> ratatui::layout::Rect {
    let width = fit_extent(width, area.width);
    let height = fit_extent(height, area.height);
    ratatui::layout::Rect {
        x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
        y: area
            .y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    }
}

/// Clear a centered `width` x `height` area (fitted to `area` by [`fit_dialog`]),
/// draw `block` there, and return `(dialog, inner)`.
pub fn render_dialog_frame(
    frame: &mut Frame,
    area: Rect,
    width: u16,
    height: u16,
    block: Block,
) -> (Rect, Rect) {
    let dialog = fit_dialog(area, width, height);
    frame.render_widget(Clear, dialog);
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    (dialog, inner)
}

/// Footer hint such as `Enter select  Esc close`, keys in `theme.hint`.
pub fn hint_line(theme: &Theme, hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(hints.len() * 2);
    for (i, (key, label)) in hints.iter().enumerate() {
        let sep = if i + 1 < hints.len() { "  " } else { "" };
        spans.push(Span::styled(
            key.to_string(),
            Style::default().fg(theme.hint),
        ));
        spans.push(Span::raw(format!(" {label}{sep}")));
    }
    Line::from(spans)
}

/// Apply Up/k, Down/j, Home and End to a list cursor. Returns whether the key was a navigation key.
pub fn navigate_list(selected: &mut usize, len: usize, code: KeyCode) -> bool {
    match code {
        KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            if *selected + 1 < len {
                *selected += 1;
            }
        }
        KeyCode::Home => *selected = 0,
        KeyCode::End => *selected = len.saturating_sub(1),
        _ => return false,
    }
    true
}

/// Move `selected` to the hovered row; returns whether it changed.
pub fn hover_select(selected: &mut usize, hovered: Option<usize>) -> bool {
    match hovered {
        Some(idx) if idx != *selected => {
            *selected = idx;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod test_keys {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    pub fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    pub fn alt_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }
}

#[cfg(test)]
mod fit_tests {
    use super::{centered_rect, fit_dialog, fit_width};
    use ratatui::layout::Rect;

    #[test]
    fn natural_size_that_fits_is_centred_like_centered_rect() {
        for area in [
            Rect::new(0, 0, 237, 67),
            Rect::new(3, 2, 114, 22),
            Rect::new(0, 0, 66, 16),
        ] {
            assert_eq!(
                fit_dialog(area, 64, 14),
                centered_rect(area, 64, 14),
                "{area:?}"
            );
        }
    }

    #[test]
    fn frame_smaller_than_the_dialog_shrinks_it_behind_a_margin() {
        assert_eq!(
            fit_dialog(Rect::new(0, 0, 50, 41), 64, 14),
            Rect::new(1, 13, 48, 14)
        );
        assert_eq!(
            fit_dialog(Rect::new(5, 7, 40, 10), 64, 14),
            Rect::new(6, 8, 38, 8)
        );
        assert_eq!(fit_width(Rect::new(0, 0, 40, 10), 64), 38);
        assert_eq!(fit_width(Rect::new(0, 0, 40, 10), 30), 30);
    }

    #[test]
    fn degenerate_frames_never_panic_or_escape_the_area() {
        for (w, h) in [(0u16, 0u16), (1, 1), (2, 2), (3, 1), (4, 4), (5, 3)] {
            let area = Rect::new(0, 0, w, h);
            let r = fit_dialog(area, 64, 14);
            assert!(r.width <= w && r.height <= h, "{w}x{h}: {r:?}");
            assert!(
                r.right() <= area.right() && r.bottom() <= area.bottom(),
                "{w}x{h}: {r:?}"
            );
        }
        // Below the minimum box the whole frame is used, never a
        // rectangle wider than the frame.
        assert_eq!(
            fit_dialog(Rect::new(0, 0, 2, 2), 64, 14),
            Rect::new(0, 0, 2, 2)
        );
        assert_eq!(
            fit_dialog(Rect::new(0, 0, 4, 4), 64, 14),
            Rect::new(0, 0, 3, 3)
        );
    }
}
