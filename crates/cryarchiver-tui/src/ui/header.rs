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
const SEPARATOR: &str = " · ";

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
	let pinned = if header.run_state == RunState::PausedLowDisk {
		width(&free_space(view)) + width(SEPARATOR) + 1
	} else {
		0
	};
	let squeeze_beside = |reserved: usize| {
		let room = usize::from(area.width).saturating_sub(width(run) + 2 + reserved);
		squeeze(view, &preset, room, usize::from(area.width) * 3 / 10)
	};
	let mut left = squeeze_beside(pinned);
	if left.badge == SHORT_BADGE {
		left = squeeze_beside(0);
	}

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

fn free_space(view: &ArchiveView) -> String {
	format!("{} free", bytes(view.system.disk_free))
}

struct Piece {
	text: String,
	color: Color,
}

/// The run state always shows; the ETA, then the saving, then the destination's free space join it while they fit.
/// While paused for disk space the order turns round: the free space stays beside the run state and the ETA goes
/// first. Each piece is drawn followed by its separator, so the lowest-ranked piece is always the leftmost.
fn render_status(buf: &mut Buffer, area: Rect, view: &ArchiveView, status: &Status, theme: &Theme) {
	let overall = &view.overall;
	let low_disk = view.header.run_state == RunState::PausedLowDisk;
	let eta = Piece {
		text: overall
			.eta
			.filter(|_| !matches!(view.header.run_state, RunState::Finished | RunState::Watching))
			.map(|eta| format!("ETA {}", duration(eta)))
			.unwrap_or_default(),
		color: theme.dim,
	};
	let saved = Piece {
		text: format!(
			"saved {} ({} %)",
			bytes(overall.saved_bytes),
			round_to_u32(overall.saved_fraction * 100.0)
		),
		color: theme.dim,
	};
	let free = Piece {
		text: free_space(view),
		color: if low_disk { theme.error } else { theme.dim },
	};
	let mut ranked = [eta, saved, free];
	if low_disk {
		ranked.reverse();
	}
	let pieces: Vec<&Piece> = ranked.iter().filter(|piece| !piece.text.is_empty()).collect();

	let room = usize::from(area.width).saturating_sub(2);
	let mut used = width(status.run) + 1;
	let mut shown = 0;
	for piece in &pieces {
		let cells = width(&piece.text) + width(SEPARATOR);
		if used + cells > room {
			break;
		}
		used += cells;
		shown += 1;
	}

	let mut x = area.right().saturating_sub(to_u16(used)).max(area.x);
	for piece in pieces[..shown].iter().rev() {
		x = put(buf, x, area.y, &piece.text, fg(piece.color), area.right());
		x = put(buf, x, area.y, SEPARATOR, fg(theme.dim), area.right());
	}
	put(
		buf,
		x,
		area.y,
		&format!("{} ", status.run),
		bold(status.color),
		area.right(),
	);
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;
	use crate::mock::MockSource;
	use crate::source::ArchiveSource;
	use crate::theme::ColorDepth;

	fn view(run_state: RunState) -> ArchiveView {
		let mut view = MockSource::new(7).view().clone();
		view.header.run_state = run_state;
		view.overall.eta = Some(Duration::from_hours(3));
		view.overall.saved_bytes = 223_000_000_000;
		view.overall.saved_fraction = 0.65;
		view.system.disk_free = 412_000_000_000;
		view
	}

	fn status(view: &ArchiveView, cells: u16, theme: &Theme) -> Buffer {
		let (run, color) = run_state(view.header.run_state, theme);
		let area = Rect::new(0, 0, cells, 1);
		let mut buf = Buffer::empty(area);
		render_status(&mut buf, area, view, &Status { run, color }, theme);
		buf
	}

	fn text(buf: &Buffer) -> String {
		(0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect()
	}

	#[test]
	fn free_space_goes_first_then_the_saving_then_the_eta() {
		let theme = Theme::dark(ColorDepth::TrueColor);
		let view = view(RunState::Running);

		let wide = text(&status(&view, 90, &theme));
		assert!(
			wide.trim_end()
				.ends_with("412 GB free · saved 223 GB (65 %) · ETA 3h 0m · ▶ running"),
			"{wide}"
		);

		let mut seen = Vec::new();
		for cells in 10..=90 {
			let shown = text(&status(&view, cells, &theme));
			let parts = (shown.contains("ETA"), shown.contains("saved"), shown.contains("free"));
			assert!(shown.contains("▶ running"), "{cells}: {shown}");
			assert!(!parts.1 || parts.0, "the saving needs the ETA at {cells}: {shown}");
			assert!(!parts.2 || parts.1, "free space needs the saving at {cells}: {shown}");
			if !seen.contains(&parts) {
				seen.push(parts);
			}
		}
		assert_eq!(
			seen,
			[
				(false, false, false),
				(true, false, false),
				(true, true, false),
				(true, true, true)
			]
		);
	}

	#[test]
	fn while_paused_for_disk_space_the_eta_goes_first_and_the_free_space_stays() {
		let theme = Theme::dark(ColorDepth::TrueColor);
		let view = view(RunState::PausedLowDisk);

		let wide = text(&status(&view, 110, &theme));
		assert!(
			wide.trim_end()
				.ends_with("ETA 3h 0m · saved 223 GB (65 %) · 412 GB free · ‖ paused: low disk space"),
			"{wide}"
		);

		let mut seen = Vec::new();
		for cells in 30..=110 {
			let shown = text(&status(&view, cells, &theme));
			let parts = (shown.contains("free"), shown.contains("saved"), shown.contains("ETA"));
			assert!(shown.contains("‖ paused: low disk space"), "{cells}: {shown}");
			assert!(
				!parts.1 || parts.0,
				"the saving needs the free space at {cells}: {shown}"
			);
			assert!(!parts.2 || parts.1, "the ETA needs the saving at {cells}: {shown}");
			if !seen.contains(&parts) {
				seen.push(parts);
			}
		}
		assert_eq!(
			seen,
			[
				(false, false, false),
				(true, false, false),
				(true, true, false),
				(true, true, true)
			]
		);
	}

	#[test]
	fn free_space_shows_as_an_error_while_paused_for_disk_space() {
		let theme = Theme::dark(ColorDepth::TrueColor);

		for (run_state, color) in [(RunState::PausedLowDisk, theme.error), (RunState::Running, theme.dim)] {
			let buf = status(&view(run_state), 120, &theme);
			let start = text(&buf).find("412").expect("the free space shows");
			let x = to_u16(text(&buf)[..start].chars().count());
			assert_eq!(buf[(x, 0)].fg, color, "{run_state:?}");
		}
	}
}
