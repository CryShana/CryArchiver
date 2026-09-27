use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Sparkline, Widget};
use throbber_widgets_tui::{Set, Throbber, ThrobberState};

use super::widgets::{Chrome, Parts, block_gauge, bold, fg, gauge, pad_left, panel, put, put_line, to_u16, wrap};
use crate::format::{bytes, duration, fit, fit_middle, fps, fraction, percent, round_to_u32, speed, thousands};
use crate::theme::{Theme, label};
use crate::view::{ArchiveView, JobActivity, Overall, Parity, RunState, SystemStats, WorkKind, WorkerJob};

const SPINNER: Set = Set {
	full: "⠿",
	empty: " ",
	symbols: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
};
const PERCENT_CELLS: usize = 6;
const RATE_CELLS: usize = 14;
const MAX_GAUGE: usize = 24;

/// Returns whether the scanning spinner made it onto the screen.
pub fn render(buf: &mut Buffer, area: Rect, view: &ArchiveView, theme: &Theme, spinner: &ThrobberState) -> bool {
	let overall_width = to_u16(usize::from(area.width) * 30 / 100);
	let workers_width = to_u16((usize::from(area.width) * 43 + 50) / 100);
	let [overall, workers, system] = Layout::horizontal([
		Constraint::Length(overall_width),
		Constraint::Length(workers_width),
		Constraint::Fill(1),
	])
	.areas(area);

	render_workers(buf, workers, view, theme);
	render_system(buf, system, view, theme);
	render_overall(buf, overall, view, theme, spinner)
}

fn content(inner: Rect) -> Rect {
	Rect {
		x: inner.x + 1,
		width: inner.width.saturating_sub(2),
		..inner
	}
}

fn cow<'a>(text: impl Into<Cow<'a, str>>) -> Cow<'a, str> {
	text.into()
}

fn render_overall(buf: &mut Buffer, area: Rect, view: &ArchiveView, theme: &Theme, spinner: &ThrobberState) -> bool {
	let chrome = Chrome {
		title: "Overall",
		right: None,
		focused: false,
	};
	let body = content(panel(buf, area, theme, &chrome));
	let overall = &view.overall;
	let cells = usize::from(body.width);
	let gauge_color = if overall.parity.is_some() {
		theme.done
	} else {
		theme.accent
	};

	let headline = headline(overall, cells, theme, spinner);
	let spinning = headline
		.spans
		.last()
		.is_some_and(|span| span.content.ends_with("scanning"));
	let mut lines = vec![
		headline,
		Line::from(block_gauge(cells, overall.progress, gauge_color, theme)),
	];
	if overall.parity.is_none() {
		lines.push(counts_line(overall, cells, theme));
	}
	lines.push(outcomes_line(overall, cells, theme));
	if overall.originals_deleted > 0 {
		lines.push(deleted_line(overall, cells, theme));
	}

	let saved = saved_line(overall, cells, theme);
	let spark_x = body.x + to_u16(saved.width()) + 2;
	let saved_row = body.y + to_u16(lines.len());
	lines.push(saved);
	lines.extend(closing_lines(overall, cells, theme));

	for (row, line) in (body.y..body.bottom()).zip(&lines) {
		put_line(buf, body.x, row, line, body.right());
	}

	if saved_row < body.bottom() && spark_x + 4 < body.right() {
		let spark_width = (body.right() - spark_x).min(30);
		sparkline(
			buf,
			Rect::new(spark_x, saved_row, spark_width, 1),
			&overall.savings_history,
			theme.done,
		);
	}
	spinning
}

fn headline<'a>(overall: &Overall, cells: usize, theme: &Theme, spinner: &ThrobberState) -> Line<'a> {
	let complete = overall.parity.is_some();
	let plus = if overall.scan_complete { "" } else { "+" };
	let mut parts = Parts::new(cells);
	parts
		.push(
			percent(overall.progress, complete).trim().to_owned(),
			bold(theme.bright),
		)
		.push(
			format!(" of {}{plus} files", thousands(overall.files_total)),
			fg(theme.dim),
		);

	if !overall.scan_complete {
		let symbol = Throbber::default().throbber_set(SPINNER).to_symbol_span(spinner);
		parts.push(format!(" {}scanning", symbol.content), fg(theme.dim));
	}
	parts.line()
}

fn counts_line<'a>(overall: &Overall, cells: usize, theme: &Theme) -> Line<'a> {
	let mut parts = Parts::new(cells);
	parts
		.group([
			(cow("✓ "), fg(theme.done)),
			(cow(format!("{} done  ", thousands(overall.files_done))), fg(theme.text)),
		])
		.group([
			(cow("● "), fg(theme.accent)),
			(cow(format!("{} active  ", overall.files_active)), fg(theme.text)),
		])
		.group([
			(cow("· "), fg(theme.dim)),
			(
				cow(format!("{} queued", thousands(overall.files_queued))),
				fg(theme.text),
			),
		]);
	parts.line()
}

fn outcomes_line<'a>(overall: &Overall, cells: usize, theme: &Theme) -> Line<'a> {
	let mut parts = Parts::new(cells);
	parts
		.group([
			(cow("! "), bold(theme.error)),
			(cow(format!("{} issues  ", overall.open_issues)), fg(theme.text)),
		])
		.group([
			(cow("= "), fg(theme.warn)),
			(cow(format!("{} kept  ", thousands(overall.kept))), fg(theme.text)),
		])
		.group([
			(cow("↪ "), fg(theme.search)),
			(cow(format!("{} dup", thousands(overall.duplicates))), fg(theme.text)),
		]);
	parts.line()
}

fn deleted_line<'a>(overall: &Overall, cells: usize, theme: &Theme) -> Line<'a> {
	let mut parts = Parts::new(cells);
	parts.group([
		(cow("⊗ "), fg(theme.dim)),
		(
			cow(match overall.originals_deleted {
				1 => "1 original deleted".to_owned(),
				count => format!("{} originals deleted", thousands(count)),
			}),
			fg(theme.text),
		),
	]);
	parts.line()
}

fn saved_line<'a>(overall: &Overall, cells: usize, theme: &Theme) -> Line<'a> {
	let saved_percent = round_to_u32(overall.saved_fraction * 100.0);
	let mut parts = Parts::new(cells);
	parts.push(
		format!("Saved {} ({saved_percent} %)", bytes(overall.saved_bytes)),
		fg(theme.done),
	);
	parts.line()
}

fn parity_text(parity: Parity) -> String {
	let excluded = if parity.excluded > 0 {
		format!(" + {} excluded", thousands(parity.excluded))
	} else {
		String::new()
	};
	format!(
		"{} source files → {} in the archive ✓{excluded}",
		thousands(parity.source_files),
		thousands(parity.archive_files)
	)
}

fn closing_lines<'a>(overall: &Overall, cells: usize, theme: &Theme) -> Vec<Line<'a>> {
	if let Some(parity) = overall.parity {
		return wrap(&parity_text(parity), cells)
			.into_iter()
			.map(|line| Line::styled(line, fg(theme.done)))
			.collect();
	}

	let Some(eta) = overall.eta else {
		return Vec::new();
	};

	let mut line = Parts::new(cells);
	line.push(format!("ETA {}", duration(eta)), fg(theme.dim));
	if !overall.scan_complete {
		line.push(" · grows while scanning", fg(theme.dim));
	}
	vec![line.line()]
}

fn render_workers(buf: &mut Buffer, area: Rect, view: &ArchiveView, theme: &Theme) {
	let workers = &view.workers;
	let right = if workers.slots.is_empty() {
		format!(" {} active ", workers.jobs.len())
	} else {
		format!(" {} active · {} ", workers.jobs.len(), workers.slots)
	};
	let chrome = Chrome {
		title: "Workers",
		right: Some(&right),
		focused: false,
	};
	let body = content(panel(buf, area, theme, &chrome));

	if workers.jobs.is_empty() {
		let idle = match view.header.run_state {
			RunState::Running => "idle",
			RunState::Paused => "paused · no job running",
			RunState::PausedLowDisk => "paused until disk space frees",
			RunState::Watching => "idle · watching for new files",
			RunState::Stopping => "stopping…",
			RunState::Finished => "all work finished",
		};
		put(buf, body.x, body.y, idle, fg(theme.dim), body.right());
		return;
	}

	for ((index, job), row) in workers.jobs.iter().enumerate().zip(body.y..body.bottom()) {
		worker_line(buf, Rect::new(body.x, row, body.width, 1), index + 1, job, theme);
	}
}

fn worker_line(buf: &mut Buffer, area: Rect, number: usize, job: &WorkerJob, theme: &Theme) {
	let (y, right) = (area.y, area.right());
	let state_color = if job.over_limit {
		theme.warn
	} else {
		theme.state(job.state)
	};
	let name_share = if area.width >= 60 { 30 } else { 38 };
	let name_width = (usize::from(area.width) * name_share / 100).max(10);

	let mut x = put(buf, area.x, y, &format!("#{number} "), fg(theme.dim), right);
	x = put(buf, x, y, &fit(&job.codec, 5), bold(theme.accent), right);
	x = put(buf, x, y, &fit_middle(&job.name, name_width), fg(theme.text), right);
	let status = match (job.over_limit, job.work, &job.stage) {
		(true, _, _) => "limit",
		(false, WorkKind::Encode, Some(stage)) => stage.as_str(),
		(false, WorkKind::Encode, None) => label(job.state),
		(false, work, _) => work.label(),
	};
	x = put(buf, x + 1, y, &fit(status, 7), fg(state_color), right);

	let rest = usize::from(right.saturating_sub(x));
	match &job.activity {
		JobActivity::Progress {
			fraction,
			fps: rate,
			speed: multiplier,
		} => {
			let progress = Progress {
				fraction: *fraction,
				rate: rate.zip(*multiplier),
				color: state_color,
			};
			progress_cells(buf, Rect::new(x, y, to_u16(rest), 1), &progress, theme);
		}
		JobActivity::QualitySearch {
			probe,
			probes,
			crf,
			metric,
			score,
		} => {
			let text = format!("probe {probe}/{probes} · CRF {crf:.2} → {metric} {score:.1}");
			put(buf, x + 1, y, &fit(&text, rest.saturating_sub(1)), fg(theme.dim), right);
		}
	}
}

struct Progress {
	fraction: f64,
	rate: Option<(f64, f64)>,
	color: Color,
}

/// Every row reserves the same room for the rate, so the gauges of all workers line up.
fn progress_cells(buf: &mut Buffer, area: Rect, progress: &Progress, theme: &Theme) {
	let (y, right) = (area.y, area.right());
	let rest = usize::from(area.width);
	let with_rate = rest >= 1 + 6 + PERCENT_CELLS + RATE_CELLS;
	let gauge_cells = if with_rate {
		(rest - 1 - PERCENT_CELLS - RATE_CELLS).min(MAX_GAUGE)
	} else {
		rest.saturating_sub(1 + PERCENT_CELLS).min(MAX_GAUGE)
	};

	let mut x = area.x + 1;
	if gauge_cells >= 4 {
		put_line(
			buf,
			x,
			y,
			&Line::from(gauge(gauge_cells, progress.fraction, progress.color, theme)),
			right,
		);
		x += to_u16(gauge_cells);
	}

	x = put(
		buf,
		x,
		y,
		&format!(" {}", percent(progress.fraction, false)),
		fg(theme.text),
		right,
	);
	if let (true, Some((rate, multiplier))) = (with_rate, progress.rate) {
		put(
			buf,
			x + 1,
			y,
			&format!("{} {}", fps(rate), speed(multiplier)),
			fg(theme.dim),
			right,
		);
	}
}

fn render_system(buf: &mut Buffer, area: Rect, view: &ArchiveView, theme: &Theme) {
	let chrome = Chrome {
		title: "System",
		right: None,
		focused: false,
	};
	let body = content(panel(buf, area, theme, &chrome));
	let system = &view.system;
	let limit = view.settings.performance_limit;
	let cells = body.width.saturating_sub(11).max(6);
	let (x, right) = (body.x, body.right());
	let rows: Vec<u16> = (body.y..body.bottom()).collect();

	if let Some(&row) = rows.first() {
		cpu_line(buf, Rect::new(x, row, body.width, 1), system, limit, cells, theme);
	}

	if let Some(&row) = rows.get(1) {
		sparkline(buf, Rect::new(x + 4, row, cells, 1), &system.cpu_history, theme.accent);
	}

	if let Some(&row) = rows.get(2) {
		put(buf, x, row, "RAM", fg(theme.dim), right);
		let used = fraction(system.ram_used, system.ram_total);
		buf.set_span(
			x + 4,
			row,
			&block_gauge(usize::from(cells), used, theme.verify, theme),
			cells,
		);
		let ram = format!(
			"{}/{}G",
			system.ram_used / 1_000_000_000,
			system.ram_total / 1_000_000_000
		);
		put(buf, x + 5 + cells, row, &ram, fg(theme.text), right);
	}

	let text_lines = [
		(
			format!("Disk {} · {} free", system.disk_label, bytes(system.disk_free)),
			fg(theme.text),
		),
		limit_line(limit, system, theme),
		(system.toolchain.clone(), fg(theme.dim)),
	];
	for (&row, (text, style)) in rows.iter().skip(3).zip(text_lines) {
		put_line(buf, x, row, &segments(&text, usize::from(body.width), style), right);
	}
}

/// Splits on ` · ` and keeps the leading segments that fit, so a line is shortened by whole facts.
fn segments(text: &str, cells: usize, style: Style) -> Line<'static> {
	let mut parts = Parts::new(cells);
	for (index, segment) in text.split(" · ").enumerate() {
		let piece = if index == 0 {
			segment.to_owned()
		} else {
			format!(" · {segment}")
		};
		parts.push(piece, style);
	}
	parts.line()
}

fn cpu_line(buf: &mut Buffer, area: Rect, system: &SystemStats, limit: Option<u8>, cells: u16, theme: &Theme) {
	let (x, y, right) = (area.x, area.y, area.right());
	let over_limit = limit.is_some_and(|limit| system.cpu_percent > limit);
	let color = if over_limit { theme.warn } else { theme.accent };
	let load = f64::from(system.cpu_percent) / 100.0;

	put(buf, x, y, "CPU", fg(theme.dim), right);
	buf.set_span(x + 4, y, &block_gauge(usize::from(cells), load, color, theme), cells);

	if let Some(limit) = limit {
		let marker_cell = (usize::from(cells) * usize::from(limit) / 100).min(usize::from(cells) - 1);
		let marker = x + 4 + to_u16(marker_cell);
		buf.set_string(marker, y, "┃", Style::new().fg(theme.error).bg(theme.panel));
	}

	let text = pad_left(&format!("{}%", system.cpu_percent), 4);
	put(buf, x + 5 + cells, y, &text, fg(theme.text), right);
}

fn limit_line(limit: Option<u8>, system: &SystemStats, theme: &Theme) -> (String, Style) {
	let priority = if system.low_priority { "low" } else { "normal" };
	match limit {
		Some(limit) => (format!("⚡ limit {limit} % · priority {priority}"), fg(theme.warn)),
		None => (format!("full speed · priority {priority}"), fg(theme.dim)),
	}
}

fn sparkline(buf: &mut Buffer, area: Rect, history: &[u64], color: Color) {
	let shown = &history[history.len().saturating_sub(usize::from(area.width))..];
	Sparkline::default().data(shown).style(fg(color)).render(area, buf);
}
