use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use super::widgets::{bold, fg, gauge, put, put_line, to_u16};
use super::{MIN_HEIGHT, MIN_WIDTH};
use crate::format::{duration, percent, width};
use crate::theme::Theme;
use crate::view::{ArchiveView, RunState};

pub fn render(buf: &mut Buffer, area: Rect, view: &ArchiveView, theme: &Theme) {
	let overall = &view.overall;
	let run = if view.header.run_state == RunState::Paused {
		"‖"
	} else {
		"▶"
	};
	let eta = overall
		.eta
		.map(|eta| format!(" · ETA {}", duration(eta)))
		.unwrap_or_default();
	let progress = percent(overall.progress, overall.parity.is_some());
	let summary = format!("{run} {} · {} active{eta}", progress.trim(), overall.files_active);

	let top = too_small(buf, area, theme);
	centred(buf, area, top + 4, &summary, fg(theme.accent));

	let gauge_y = top + 6;
	if gauge_y < area.bottom() {
		let cells = usize::from(area.width / 2);
		let line = Line::from(gauge(cells, overall.progress, theme.accent, theme));
		put_line(buf, area.x + area.width / 4, gauge_y, &line, area.right());
	}
}

/// The three centred lines that say the terminal is too small; returns the row of the first.
pub fn too_small(buf: &mut Buffer, area: Rect, theme: &Theme) -> u16 {
	let lines = [
		("Terminal too small".to_owned(), bold(theme.bright)),
		(
			format!(
				"{}×{} · needs at least {MIN_WIDTH}×{MIN_HEIGHT}",
				area.width, area.height
			),
			fg(theme.text),
		),
		("Enlarge the window or reduce the font size".to_owned(), fg(theme.dim)),
	];

	let top = (area.y + area.height / 2).saturating_sub(3);
	for (y, (text, style)) in (top..).zip(lines) {
		centred(buf, area, y, &text, style);
	}
	top
}

fn centred(buf: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
	if y >= area.bottom() {
		return;
	}

	let x = area.x + area.width.saturating_sub(to_u16(width(text))) / 2;
	put(buf, x, y, text, style, area.right());
}
