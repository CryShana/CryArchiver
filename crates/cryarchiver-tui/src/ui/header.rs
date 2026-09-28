use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::widgets::{bold, fg, put, to_u16};
use crate::format::{bytes, duration, fit_middle, round_to_u32, width};
use crate::theme::Theme;
use crate::view::{ArchiveView, RunState};

const BADGE: &str = " CryArchiver ";
const SHORT_BADGE: &str = " CA ";
const DEMO: &str = " DEMO ";
const DELETING: &str = " ⚠ DELETING ORIGINALS ";
const DELETING_PAUSED: &str = " deleting paused: type DELETE to allow ";
const MIN_ROUTE: usize = 10;

/// The left side's texts after squeezing: the route shrinks to its minimum, then "applying…" becomes "…", then the
/// route goes, then the badge shortens.
struct Left {
	badge: &'static str,
	route_width: usize,
	limit: Option<String>,
}

impl Left {
	fn width(&self, view: &ArchiveView, preset: &str) -> usize {
		let header = &view.header;
		let chip = |on: bool, text: &str| if on { width(text) + 1 } else { 0 };
		let route = if self.route_width > 0 { self.route_width + 1 } else { 0 };

		width(self.badge)
			+ chip(header.demo, DEMO)
			+ chip(header.deleting_originals, DELETING)
			+ chip(header.deleting_originals && header.deleting_paused, DELETING_PAUSED)
			+ route + width(preset)
			+ 1 + self.limit.as_deref().map_or(0, |limit| width(limit) + 1)
	}
}

fn squeeze(view: &ArchiveView, preset: &str, room: usize, wanted_route: usize) -> Left {
	let limit_text = |short: bool| {
		view.limit().map(|limit| match (limit.pending, short) {
			(false, _) => format!("⚡ limit {} %", limit.cap),
			(true, false) => format!("⚡ limit {} % · applying…", limit.cap),
			(true, true) => format!("⚡ limit {} % …", limit.cap),
		})
	};

	let mut left = Left {
		badge: BADGE,
		route_width: 0,
		limit: limit_text(false),
	};
	let spare = |left: &Left| room.saturating_sub(left.width(view, preset));

	if spare(&left) > MIN_ROUTE {
		left.route_width = wanted_route.min(spare(&left) - 1).max(MIN_ROUTE);
		return left;
	}

	left.limit = limit_text(true);
	if spare(&left) > MIN_ROUTE {
		left.route_width = wanted_route.min(spare(&left) - 1).max(MIN_ROUTE);
		return left;
	}

	if spare(&left) == 0 {
		left.badge = SHORT_BADGE;
	}
	left
}

pub fn render(buf: &mut Buffer, area: Rect, view: &ArchiveView, theme: &Theme) {
	let header = &view.header;
	let background = if header.deleting_originals {
		theme.danger
	} else {
		theme.panel
	};
	buf.set_style(area, Style::new().bg(background));
	let (y, right) = (area.y, area.right());

	let profile = view.settings.profile_label();
	let preset = match &header.codecs {
		Some(codecs) => format!(" {profile} · {codecs} "),
		None => format!(" {profile} "),
	};
	let (run, run_color) = run_state(header.run_state, theme);
	let room = usize::from(area.width).saturating_sub(width(run) + 2);
	let left = squeeze(view, &preset, room, usize::from(area.width) * 3 / 10);

	let mut x = put(
		buf,
		area.x,
		y,
		left.badge,
		bold(theme.background).bg(theme.accent),
		right,
	);
	if header.demo {
		x = put(buf, x + 1, y, DEMO, bold(theme.background).bg(theme.warn), right);
	}
	if header.deleting_originals {
		x = put(buf, x + 1, y, DELETING, bold(theme.bright).bg(theme.error), right);
	}
	if header.deleting_originals && header.deleting_paused {
		x = put(
			buf,
			x + 1,
			y,
			DELETING_PAUSED,
			bold(theme.background).bg(theme.warn),
			right,
		);
	}

	if left.route_width > 0 {
		let route = format!("{} → {}", header.source, header.destination);
		x = put(
			buf,
			x + 1,
			y,
			&fit_middle(&route, left.route_width),
			fg(theme.bright),
			right,
		);
	}
	x = put(buf, x + 1, y, &preset, bold(theme.accent), right);
	if let Some(limit) = &left.limit {
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
	let start = area.right().saturating_sub(total).max(area.x);
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
