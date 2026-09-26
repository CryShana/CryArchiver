use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::widgets::{bold, fg, put, to_u16};
use crate::format::{bytes, duration, fit_middle, round_to_u32, width};
use crate::theme::Theme;
use crate::view::{ArchiveView, RunState};

const BADGE: &str = " CryArchiver ";
const DEMO: &str = " DEMO ";
const DELETING: &str = " ⚠ DELETING ORIGINALS ";

pub fn render(buf: &mut Buffer, area: Rect, view: &ArchiveView, theme: &Theme) {
	let header = &view.header;
	let background = if header.deleting_originals {
		theme.danger
	} else {
		theme.panel
	};
	buf.set_style(area, Style::new().bg(background));
	let (y, right) = (area.y, area.right());

	let preset = format!(" {} · {} ", view.settings.speed.label(), header.codecs);
	let limit = header.performance_limit.map(|limit| {
		let applying = if limit.pending { " · applying…" } else { "" };
		format!("⚡ limit {} %{applying}", limit.cap)
	});
	let (run, run_color) = run_state(header.run_state, theme);

	let fixed = width(BADGE)
		+ if header.demo { width(DEMO) + 1 } else { 0 }
		+ if header.deleting_originals {
			width(DELETING) + 1
		} else {
			0
		} + width(&preset)
		+ limit.as_deref().map_or(0, |limit| width(limit) + 1)
		+ width(run)
		+ 5;
	let route_room = usize::from(area.width).saturating_sub(fixed);
	let route_width = (usize::from(area.width) * 3 / 10).min(route_room).max(10);

	let mut x = put(buf, area.x, y, BADGE, bold(theme.background).bg(theme.accent), right);
	if header.demo {
		x = put(buf, x + 1, y, DEMO, bold(theme.background).bg(theme.warn), right);
	}
	if header.deleting_originals {
		x = put(buf, x + 1, y, DELETING, bold(theme.bright).bg(theme.error), right);
	}

	let route = format!("{} → {}", header.source, header.destination);
	x = put(buf, x + 1, y, &fit_middle(&route, route_width), fg(theme.bright), right);
	x = put(buf, x + 1, y, &preset, bold(theme.accent), right);
	if let Some(limit) = &limit {
		x = put(buf, x + 1, y, limit, fg(theme.warn), right);
	}

	let status = Status { run, color: run_color };
	render_status(buf, Rect::new(x, y, right.saturating_sub(x), 1), view, &status, theme);
}

struct Status {
	run: &'static str,
	color: Color,
}

fn run_state(state: RunState, theme: &Theme) -> (&'static str, Color) {
	match state {
		RunState::Running => ("▶ running", theme.done),
		RunState::Paused => ("‖ paused", theme.warn),
		RunState::PausedLowDisk => ("‖ paused: low disk space", theme.error),
		RunState::Watching => ("◉ watching for new files", theme.accent),
		RunState::Stopping => ("■ stopping…", theme.warn),
		RunState::Finished => ("✓ complete", theme.done),
	}
}

/// The run state always shows; the ETA and then the saving join it only when they fit.
fn render_status(buf: &mut Buffer, area: Rect, view: &ArchiveView, status: &Status, theme: &Theme) {
	let overall = &view.overall;
	let saved = format!(
		"saved {} ({} %) · ",
		bytes(overall.saved_bytes),
		round_to_u32(overall.saved_fraction * 100.0)
	);
	let eta = overall
		.eta
		.filter(|_| !matches!(view.header.run_state, RunState::Finished | RunState::Watching))
		.map(|eta| format!("ETA {} · ", duration(eta)))
		.unwrap_or_default();

	let room = usize::from(area.width).saturating_sub(2);
	let run_width = width(status.run) + 1;
	let with_eta = run_width + width(&eta) <= room;
	let with_saved = with_eta && run_width + width(&eta) + width(&saved) <= room;

	let mut extras = String::new();
	if with_saved {
		extras.push_str(&saved);
	}
	if with_eta {
		extras.push_str(&eta);
	}

	let total = to_u16(width(&extras) + run_width);
	let start = area.right().saturating_sub(total);
	let x = put(buf, start, area.y, &extras, fg(theme.dim), area.right());
	put(
		buf,
		x,
		area.y,
		&format!("{} ", status.run),
		bold(status.color),
		area.right(),
	);
}
