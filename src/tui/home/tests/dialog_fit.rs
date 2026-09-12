//! Confirm-style dialogs are sized from the live frame on every render:
//! centred, clamped inside the frame behind a one-cell margin, never
//! partly off-screen. The sizes below are real tmux clients attached to one
//! session at the same time — a phone, a tablet in landscape, a tiny
//! window, and a desktop terminal.
use super::*;
use crate::tui::dialogs::RestartDialog;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

/// The rounded border box a dialog painted, located from its title: the top
/// border is the contiguous run `╭─… title …─╮`, and the left column runs
/// straight down to `╰`. `None` when any corner is missing, i.e. the box
/// is not fully on screen.
struct BorderBox {
    x: u16,
    y: u16,
    right: u16,
    bottom: u16,
}

fn cell(buf: &Buffer, x: u16, y: u16) -> char {
    buf[(x, y)].symbol().chars().next().unwrap_or(' ')
}

fn find_box(buf: &Buffer, title: &str) -> Option<BorderBox> {
    let title: Vec<char> = title.chars().collect();
    let (w, h) = (buf.area.width, buf.area.height);
    let (y, tx) = (0..h).find_map(|y| {
        let row: Vec<char> = (0..w).map(|x| cell(buf, x, y)).collect();
        row.windows(title.len())
            .position(|run| run == title.as_slice())
            .map(|x| (y, x as u16))
    })?;
    let x = (0..=tx).rev().find(|&x| cell(buf, x, y) == '╭')?;
    let right = (tx..w).find(|&x| cell(buf, x, y) == '╮')?;
    let bottom = (y..h).find(|&row| cell(buf, x, row) == '╰')?;
    (cell(buf, right, bottom) == '╯').then_some(BorderBox {
        x,
        y,
        right,
        bottom,
    })
}

fn dump(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| cell(buf, x, y))
                .collect::<String>()
                + "\n"
        })
        .collect()
}

/// Natural size of the Restart dialog (`RestartDialog::render`).
const NATURAL: (u16, u16) = (64, 14);

#[test]
#[serial]
fn restart_dialog_fits_and_centres_in_the_live_frame() {
    let theme = crate::tui::styles::load_theme("empire");
    for (w, h) in [(50u16, 41u16), (114, 22), (40, 18), (237, 67)] {
        let mut env = create_test_env_empty();
        env.view.restart_dialog = Some(RestartDialog::new(
            "S",
            "test",
            "claude",
            "",
            "",
            vec!["test".to_string()],
            vec!["claude".to_string()],
        ));
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let b = find_box(&buf, " Restart Session ").unwrap_or_else(|| {
            panic!(
                "{w}x{h}: dialog box is not fully on screen:\n{}",
                dump(&buf)
            )
        });
        let (bw, bh) = (b.right - b.x + 1, b.bottom - b.y + 1);
        let (left, right) = (b.x, w - 1 - b.right);
        let (top, bottom) = (b.y, h - 1 - b.bottom);
        assert!(
            left.abs_diff(right) <= 1 && top.abs_diff(bottom) <= 1,
            "{w}x{h}: dialog is not centred (margins l={left} r={right} t={top} b={bottom}):\n{}",
            dump(&buf)
        );
        // The natural size while it fits behind a one-cell margin;
        // otherwise the frame minus that margin, never edge-to-edge.
        let want_w = NATURAL.0.min(w - 2);
        let want_h = NATURAL.1.min(h - 2);
        assert_eq!(
            (bw, bh),
            (want_w, want_h),
            "{w}x{h}: dialog is {bw}x{bh}, expected {want_w}x{want_h}:\n{}",
            dump(&buf)
        );
        assert!(
            left >= 1 && right >= 1 && top >= 1 && bottom >= 1,
            "{w}x{h}: dialog touches the frame edge (l={left} r={right} t={top} b={bottom})"
        );
    }
}
