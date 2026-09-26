use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::widgets::{bold, fg, put, to_u16};
use crate::format::{bytes, duration, fit_middle, round_to_u32, width};
use crate::theme::Theme;
use crate::view::{ArchiveView, RunState};

pub fn render(buf: &mut Buffer, area: Rect, view: &ArchiveView, theme: &Theme) {
	buf.set_style(area, Style::new().bg(theme.panel));
	let (y, right) = (area.y, area.right());
	let header = &view.header;

	let mut x = put(
		buf,
		area.x,
		y,
		" CryArchiver ",
		bold(theme.background).bg(theme.accent),
		right,
	);

	let route = format!("{} → {}", header.source, header.destination);
	let route_width = (usize::from(area.width) * 3 / 10).max(10);
	x = put(buf, x + 1, y, &fit_middle(&route, route_width), fg(theme.bright), right);

	let preset = format!(" {} · {} ", header.speed, header.codecs);
	x = put(buf, x + 1, y, &preset, bold(theme.accent), right);

	if let Some(limit) = header.performance_limit {
		x = put(buf, x + 1, y, &format!("⚡ limit {limit} %"), fg(theme.warn), right);
	}

	let status = status_block(view);
	let status_width = to_u16(width(&status));
	if x + status_width + 2 < right {
		let color = if header.run_state == RunState::Paused {
			theme.warn
		} else {
			theme.dim
		};
		put(buf, right - status_width, y, &status, fg(color), right);
	}
}

fn status_block(view: &ArchiveView) -> String {
	let overall = &view.overall;
	let saved_percent = round_to_u32(overall.saved_fraction * 100.0);
	let mut parts = vec![format!("saved {} ({saved_percent} %)", bytes(overall.saved_bytes))];

	if let Some(eta) = overall.eta.filter(|_| view.header.run_state != RunState::Finished) {
		parts.push(format!("ETA {}", duration(eta)));
	}

	parts.push(
		match view.header.run_state {
			RunState::Running => "▶ running",
			RunState::Paused => "‖ paused",
			RunState::Finished => "✓ finished",
		}
		.to_owned(),
	);

	format!("{} ", parts.join(" · "))
}
