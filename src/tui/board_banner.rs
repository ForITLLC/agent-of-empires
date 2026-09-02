//! Board identity callout.
//!
//! A one-row, full-width strip at the very top of EVERY frame naming the
//! board (`[web] instance_label`) so an operator with several boards open —
//! a desk machine and a cloud VM — can tell at a glance which one a frame
//! belongs to. The app root paints it above the home list, previews,
//! settings and dialogs alike; the remote-home view paints the label of the
//! daemon it is attached to. No label → no strip, no row lost.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::tui::styles::{has_min_contrast, Theme};

/// Minimum text/bar contrast for the strip (WCAG AA body text).
const STRIP_CONTRAST_RATIO: f32 = 4.5;

/// Normalised label: trimmed; `None` when unset or blank.
pub fn label(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|s| !s.is_empty())
}

/// Reserve the strip row. Returns `(strip, remainder)`; `strip` is `None`
/// when there is no label or the frame is too short to give up a row.
pub fn split(area: Rect, label: Option<&str>) -> (Option<Rect>, Rect) {
    if label.is_none() || area.height < 2 {
        return (None, area);
    }
    let strip = Rect { height: 1, ..area };
    let rest = Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };
    (Some(strip), rest)
}

/// Paint the strip: the label, bold, on a solid accent bar spanning the
/// full width so it reads as chrome rather than content.
pub fn render(frame: &mut Frame, area: Rect, theme: &Theme, label: &str) {
    let bg = theme.accent;
    let fg = if has_min_contrast(theme.background, bg, STRIP_CONTRAST_RATIO) {
        theme.background
    } else {
        theme.text
    };
    let style = Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
    let line = Line::from(vec![Span::styled(format!(" {label} "), style)]);
    frame.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn label_normalises_blank_to_none() {
        assert_eq!(label(None), None);
        assert_eq!(label(Some("")), None);
        assert_eq!(label(Some("   \t")), None);
        assert_eq!(label(Some("  ForIT Cloud ")), Some("ForIT Cloud"));
    }

    #[test]
    fn split_reserves_the_top_row_only_with_a_label() {
        let area = Rect::new(0, 0, 40, 10);
        assert_eq!(split(area, None), (None, area));
        let (strip, rest) = split(area, Some("ForIT Cloud"));
        assert_eq!(strip, Some(Rect::new(0, 0, 40, 1)));
        assert_eq!(rest, Rect::new(0, 1, 40, 9));
        // Offset areas keep their origin.
        let (strip, rest) = split(Rect::new(3, 2, 20, 5), Some("Mini"));
        assert_eq!(strip, Some(Rect::new(3, 2, 20, 1)));
        assert_eq!(rest, Rect::new(3, 3, 20, 4));
        // Too short to give up a row: the frame is untouched.
        let tiny = Rect::new(0, 0, 40, 1);
        assert_eq!(split(tiny, Some("x")), (None, tiny));
    }

    fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn render_paints_the_label_on_a_full_width_bar_above_the_body() {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let mut terminal = Terminal::new(TestBackend::new(30, 3)).expect("terminal");
        terminal
            .draw(|f| {
                let (strip, rest) = split(f.area(), Some("ForIT Cloud"));
                render(f, strip.expect("strip"), &theme, "ForIT Cloud");
                f.render_widget(Paragraph::new("body"), rest);
            })
            .expect("draw");
        assert!(
            row(&terminal, 0).starts_with(" ForIT Cloud "),
            "{:?}",
            row(&terminal, 0)
        );
        let buf = terminal.backend().buffer();
        assert!(
            (0..30).all(|x| buf[(x, 0)].bg == theme.accent),
            "the bar spans the full width"
        );
        assert!(
            row(&terminal, 1).starts_with("body"),
            "{:?}",
            row(&terminal, 1)
        );
    }
}
