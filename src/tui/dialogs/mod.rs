//! TUI dialog components

pub mod attach_project;
mod changelog;
mod cheats;
mod command_palette;
mod confirm;
mod context_menu;
mod custom_instruction;
mod delete_options;
mod group_delete_options;
mod group_picker;
mod hooks_install;
mod info;
mod intro;
mod new_session;
mod no_agents;
mod permission_response;
mod plugin_manager;
mod profile_picker;
mod project_session_picker;
mod projects;
mod rename;
mod repo_trust;
mod restart;
mod send_message;
#[cfg(feature = "serve")]
mod serve;
mod skills_manager;
mod snooze_duration;
pub mod sort_picker;
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
pub use group_picker::GroupPickerDialog;
pub use hooks_install::HooksInstallDialog;
pub use info::InfoDialog;
pub use intro::{IntroDialog, IntroOutcome};
pub(crate) use new_session::project_picker_label;
pub use new_session::{NewSessionData, NewSessionDialog};
pub use no_agents::{NoAgentsAction, NoAgentsDialog};
pub use permission_response::{PermissionResponseChoice, PermissionResponseDialog};
pub use plugin_manager::PluginManagerDialog;
pub use profile_picker::{ProfileEntry, ProfilePickerAction, ProfilePickerDialog};
pub use project_session_picker::ProjectSessionPickerDialog;
pub use projects::ProjectsDialog;
pub use rename::{RenameData, RenameDialog, RenameMode};
pub use repo_trust::{RepoTrustAction, RepoTrustDialog};
pub use restart::{RestartData, RestartDialog};
pub use send_message::SendMessageDialog;
#[cfg(feature = "serve")]
pub(crate) use serve::start_local_daemon_and_wait;
#[cfg(feature = "serve")]
pub use serve::{ServeAction, ServeView};
pub use skills_manager::SkillsManagerDialog;
pub use snooze_duration::SnoozeDurationDialog;
pub use sort_picker::SortPickerDialog;
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

/// Insert pasted text into a single-line `tui_input::Input`, stripping
/// newlines so a multi-line paste cannot act as a submit or scatter across
/// fields. Shared by every dialog/settings paste handler that targets an
/// `Input`; multi-line `TextArea` targets keep their own handling.
pub fn paste_into_input(input: &mut tui_input::Input, text: &str) {
    for ch in text.chars().filter(|c| *c != '\n' && *c != '\r') {
        input.handle(tui_input::InputRequest::InsertChar(ch));
    }
}

/// Center a dialog of given size within an area, clamping to fit.
pub fn centered_rect(
    area: ratatui::layout::Rect,
    width: u16,
    height: u16,
) -> ratatui::layout::Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    ratatui::layout::Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

/// Park the visible terminal cursor on the dialog's bottom-right corner.
///
/// A tmux client smaller than the window (a phone attached alongside a
/// desktop client under `window-size largest`) shows only the part of the
/// window containing the cursor, and tmux tracks only a VISIBLE cursor.
/// Dialogs center in the full frame, so without an in-dialog cursor they
/// can land entirely outside a small client's visible region. tmux pans
/// minimally, so anchoring the bottom-right corner pulls the whole dialog
/// into view on any client at least as large as the dialog.
pub fn anchor_client_view(frame: &mut ratatui::Frame, dialog_area: ratatui::layout::Rect) {
    if dialog_area.width == 0 || dialog_area.height == 0 {
        return;
    }
    frame.set_cursor_position(ratatui::layout::Position::new(
        dialog_area.right().saturating_sub(1),
        dialog_area.bottom().saturating_sub(1),
    ));
}

#[cfg(test)]
mod anchor_tests {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Confirm-class dialogs must leave the terminal cursor on their own
    /// bottom-right border corner so a smaller attached tmux client pans to
    /// the dialog instead of showing an empty corner of the frame (WO#1513:
    /// restart dialog invisible on a 90x44 phone client of a 235x85 window).
    #[test]
    fn confirm_dialogs_anchor_cursor_to_bottom_right_corner() {
        let cases: Vec<(&str, Box<dyn FnMut(&mut ratatui::Frame)>)> = vec![
            ("ConfirmDialog", {
                let theme = load_theme("empire");
                let mut d = super::ConfirmDialog::new("Stop Session", "Stop 'x'?", "stop_session");
                Box::new(move |f| d.render(f, f.area(), &theme))
            }),
            ("RestartDialog", {
                let theme = load_theme("empire");
                let mut d = super::RestartDialog::new(
                    "sess",
                    "default",
                    "claude",
                    "",
                    "",
                    vec!["default".into()],
                    vec!["claude".into()],
                );
                Box::new(move |f| d.render(f, f.area(), &theme))
            }),
        ];
        for (name, mut render) in cases {
            let backend = TestBackend::new(235, 85);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| render(f)).unwrap();
            let buf = terminal.backend().buffer().clone();
            // The bottom-right-most rounded corner char is the dialog's own.
            let mut corner = None;
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    if buf[(x, y)].symbol() == "╯" {
                        corner = Some((x, y));
                    }
                }
            }
            let corner = corner.unwrap_or_else(|| panic!("{name}: no dialog border rendered"));
            let cursor = terminal.get_cursor_position().unwrap();
            assert_eq!(
                (cursor.x, cursor.y),
                corner,
                "{name}: cursor must sit on the dialog's bottom-right corner"
            );
        }
    }
}
