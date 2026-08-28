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

/// Center a confirm-class dialog within the region the smallest attached
/// tmux client can see, so it stays usable on a phone-sized client.
///
/// A tmux client smaller than the window (a phone attached alongside a
/// desktop client under `window-size largest`) shows only a cut-down
/// region of the window, panned per-client to follow the visible cursor.
/// tmux's pan mapping is a three-zone clamp per axis (pin-left when the
/// cursor is within a client-width of the left edge, pin-right near the
/// right edge, else center on the cursor), so for a dialog centered in a
/// window much wider than the client there is NO cursor position that
/// brings the whole dialog into view; the dialog itself has to move.
///
/// Centering inside the top-left `min-client`-sized region works for
/// every client at least as large as the dialog: a client's view either
/// starts at the window origin (pin-left zone) and spans the region, or
/// is centered on the in-dialog cursor. When every attached client is at
/// least window-sized (the desktop-only case) the region is the whole
/// frame and this is exactly `centered_rect`.
pub fn client_fit_rect(
    area: ratatui::layout::Rect,
    width: u16,
    height: u16,
) -> ratatui::layout::Rect {
    centered_rect(
        shrink_to_min_client(area, min_attached_client()),
        width,
        height,
    )
}

/// Top-left sub-region of `area` no larger than the smallest attached
/// client's visible size. `min_client` is the raw client tty size; one row
/// is reserved for the tmux status line.
fn shrink_to_min_client(
    area: ratatui::layout::Rect,
    min_client: Option<(u16, u16)>,
) -> ratatui::layout::Rect {
    let Some((cw, ch)) = min_client else {
        return area;
    };
    ratatui::layout::Rect {
        x: area.x,
        y: area.y,
        width: area.width.min(cw),
        height: area.height.min(ch.saturating_sub(1)),
    }
}

/// Smallest attached client (width, height) of the tmux session hosting
/// this TUI, or None outside tmux / on any query failure. Cached briefly:
/// dialogs re-render on every event tick and the client set changes
/// rarely, so a short TTL keeps the exec off the hot path while still
/// tracking a phone attaching mid-dialog.
fn min_attached_client() -> Option<(u16, u16)> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    type ClientCache = Option<(Instant, Option<(u16, u16)>)>;
    static CACHE: Mutex<ClientCache> = Mutex::new(None);

    let mut cache = CACHE.lock().unwrap();
    if let Some((at, val)) = *cache {
        if at.elapsed() < Duration::from_secs(2) {
            return val;
        }
    }
    let val = query_min_attached_client();
    *cache = Some((Instant::now(), val));
    val
}

/// One `tmux list-clients` against the HOST server (the one whose pane we
/// render in, from $TMUX; not the server aoe manages sessions on).
fn query_min_attached_client() -> Option<(u16, u16)> {
    let tmux_env = std::env::var("TMUX").ok()?;
    let socket = tmux_env.split(',').next().filter(|s| !s.is_empty())?;
    let pane = std::env::var("TMUX_PANE").ok()?;
    let out = std::process::Command::new("tmux")
        .args([
            "-S",
            socket,
            "list-clients",
            "-t",
            &pane,
            "-F",
            "#{client_width} #{client_height}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut min: Option<(u16, u16)> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        let (Some(w), Some(h)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(w), Ok(h)) = (w.parse::<u16>(), h.parse::<u16>()) else {
            continue;
        };
        min = Some(match min {
            Some((mw, mh)) => (mw.min(w), mh.min(h)),
            None => (w, h),
        });
    }
    min
}

/// Park the visible terminal cursor on the dialog's bottom-right corner.
///
/// tmux pans a smaller client's view only to a VISIBLE cursor; dialogs
/// render no cursor of their own, so without this anchor a small client
/// never pans toward the dialog at all. Combined with [`client_fit_rect`]
/// the corner lands inside the pin-left zone for every client at least as
/// large as the dialog, which pans the whole dialog into view.
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
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    /// The dialog region shrinks to what the smallest attached client can
    /// see (one client row reserved for the tmux status line), and the
    /// dialog centers inside that region so every client at least as large
    /// as the dialog gets the whole dialog in its pin-left view.
    #[test]
    fn dialog_region_fits_smallest_client() {
        let frame = Rect::new(0, 0, 235, 85);
        // (min_client, expected shrunk region)
        let cases = [
            // no tmux / no clients: full frame, identical to centered_rect
            (None, frame),
            // phone client 90x44 tty: region is its 90x43 visible area
            (Some((90, 44)), Rect::new(0, 0, 90, 43)),
            // all clients at least window-sized: full frame
            (Some((235, 86)), frame),
            (Some((240, 90)), frame),
            // degenerate client: width clamps, height saturates to 0
            (Some((50, 1)), Rect::new(0, 0, 50, 0)),
        ];
        for (min_client, want) in cases {
            let got = super::shrink_to_min_client(frame, min_client);
            assert_eq!(got, want, "min_client={min_client:?}");
        }
        // The phone-shrunk region centers a 64x14 dialog fully inside the
        // client-visible area (WO#1513 geometry).
        let dialog = super::centered_rect(Rect::new(0, 0, 90, 43), 64, 14);
        assert_eq!(dialog, Rect::new(13, 14, 64, 14));
    }

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
