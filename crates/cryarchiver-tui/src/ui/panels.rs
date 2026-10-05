use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Sparkline, Widget};
use throbber_widgets_tui::{Set, Throbber, ThrobberState};

use super::widgets::{Chrome, Parts, block_gauge, bold, fg, gauge, pad_left, panel, put, put_line, to_u16, wrap};
use crate::app::{Hit, HitMap};
use crate::format::{bytes, fit, fit_middle, fps, fraction, percent, round_to_u32, speed, thousands, width};
use crate::theme::{Theme, label};
use crate::view::{ArchiveView, JobActivity, Overall, Parity, RunState, StateFilter, SystemStats, WorkKind, WorkerJob};

const SPINNER: Set = Set {
	full: "⠿",
	empty: " ",
	symbols: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
};
const PERCENT_CELLS: usize = 6;
const RATE_CELLS: usize = 14;
const MAX_GAUGE: usize = 24;
const MAX_CODEC: usize = 4;
const MIN_STATUS: usize = 7;
const MAX_STATUS: usize = 18;
/// The groups of the outcomes line, in the order they are drawn and their hits are assigned.
const OUTCOMES: [StateFilter; 3] = [StateFilter::Issues, StateFilter::Kept, StateFilter::Duplicates];

/// The Overall panel's lines and border at this terminal width. Workers and System are drawn into whatever height the
/// caller settles on.
pub fn height(width: u16, view: &ArchiveView, theme: &Theme, spinner: &ThrobberState) -> u16 {
	let cells = usize::from(overall_width(width)).saturating_sub(4);
	to_u16(overall_lines(&view.overall, cells, theme, spinner, None).lines.len()) + 2
}

fn overall_width(width: u16) -> u16 {
	to_u16(usize::from(width) * 30 / 100)
}

/// What the Overall panel shows besides the view: the spinner frame and the state filter the Library is narrowed to.
pub struct OverallState<'a> {
	pub spinner: &'a ThrobberState,
	pub active: Option<StateFilter>,
}

/// Returns whether the scanning spinner made it onto the screen.
pub fn render(
	buf: &mut Buffer,
	area: Rect,
	view: &ArchiveView,
	theme: &Theme,
	state: &OverallState,
	hits: &mut HitMap,
) -> bool {
	let overall_width = overall_width(area.width);
	let workers_width = to_u16((usize::from(area.width) * 43 + 50) / 100);
	let [overall, workers, system] = Layout::horizontal([
		Constraint::Length(overall_width),
		Constraint::Length(workers_width),
		Constraint::Fill(1),
	])
	.areas(area);

	render_workers(buf, workers, view, theme);
	render_system(buf, system, view, theme);
	render_overall(buf, overall, view, theme, state, hits)
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

fn render_overall(
	buf: &mut Buffer,
	area: Rect,
	view: &ArchiveView,
	theme: &Theme,
	state: &OverallState,
	hits: &mut HitMap,
) -> bool {
	let chrome = Chrome {
		title: "Overall",
		right: None,
		focused: false,
	};
	let body = content(panel(buf, area, theme, &chrome));
	let overall = &view.overall;
	let OverallLines {
		lines,
		outcomes_row,
		saved_row,
		spinning,
	} = overall_lines(overall, usize::from(body.width), theme, state.spinner, state.active);

	for (row, line) in (body.y..body.bottom()).zip(&lines) {
		put_line(buf, body.x, row, line, body.right());
	}

	let outcomes_y = body.y + to_u16(outcomes_row);
	if outcomes_y < body.bottom() {
		let mut x = body.x;
		for (group, state) in lines[outcomes_row].spans.chunks(3).zip(OUTCOMES) {
			let shown = to_u16(group[..2].iter().map(|span| width(&span.content)).sum());
			hits.push(
				Rect::new(x, outcomes_y, shown, 1).intersection(body),
				Hit::Outcome(state),
			);
			x += shown + to_u16(width(&group[2].content));
		}
	}

	let spark_x = body.x + to_u16(lines[saved_row].width()) + 2;
	let saved_row = body.y + to_u16(saved_row);
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

struct OverallLines<'a> {
	lines: Vec<Line<'a>>,
	outcomes_row: usize,
	saved_row: usize,
	spinning: bool,
}

fn overall_lines<'a>(
	overall: &Overall,
	cells: usize,
	theme: &Theme,
	spinner: &ThrobberState,
	active: Option<StateFilter>,
) -> OverallLines<'a> {
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
	let outcomes_row = lines.len();
	lines.push(outcomes_line(overall, cells, theme, active));
	if overall.originals_deleted > 0 {
		lines.push(deleted_line(overall, cells, theme));
	}

	let saved_row = lines.len();
	lines.push(saved_line(overall, cells, theme));
	if let Some(parity) = overall.parity {
		lines.extend(
			wrap(&parity_text(parity), cells)
				.into_iter()
				.map(|line| Line::styled(line, fg(theme.done))),
		);
	}

	OverallLines {
		lines,
		outcomes_row,
		saved_row,
		spinning,
	}
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

/// Each group is its glyph, its label and the gap after it, so the clickable part is easy to find again. The group the
/// Library is filtered by is highlighted without changing a cell's width. When the words don't fit, the counts stand
/// alone, so every group stays on screen.
fn outcomes_line<'a>(overall: &Overall, cells: usize, theme: &Theme, active: Option<StateFilter>) -> Line<'a> {
	let groups = OUTCOMES.map(|state| match state {
		StateFilter::Issues => (
			state,
			"! ",
			bold(theme.error),
			overall.open_issues.to_string(),
			" issues",
		),
		StateFilter::Kept => (state, "= ", fg(theme.warn), thousands(overall.kept), " kept"),
		StateFilter::Duplicates => (state, "↪ ", fg(theme.search), thousands(overall.duplicates), " dup"),
	});
	let full_width: usize = groups
		.iter()
		.map(|(_, glyph, _, count, word)| width(glyph) + width(count) + width(word))
		.sum::<usize>()
		+ groups.len()
		- 1;
	let (words, gap) = if full_width <= cells {
		(true, " ")
	} else {
		(false, "  ")
	};

	let mut parts = Parts::new(cells);
	let last = groups.len() - 1;
	for (index, (state, glyph, glyph_style, count, word)) in groups.into_iter().enumerate() {
		let (glyph_style, label_style) = if active == Some(state) {
			let highlight = bold(theme.background).bg(theme.accent);
			(highlight, highlight)
		} else {
			(glyph_style, fg(theme.text))
		};
		let label = if words { count + word } else { count };
		let gap = if index == last { "" } else { gap };
		parts.group([
			(cow(glyph), glyph_style),
			(cow(label), label_style),
			(cow(gap), fg(theme.text)),
		]);
	}
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

	let rows = usize::from(body.height);
	let shown = if workers.jobs.len() > rows {
		rows.saturating_sub(1)
	} else {
		workers.jobs.len()
	};
	let jobs = &workers.jobs[..shown];

	let columns = columns(jobs, body.width);
	for ((index, job), row) in jobs.iter().enumerate().zip(body.y..body.bottom()) {
		worker_line(
			buf,
			Rect::new(body.x, row, body.width, 1),
			index + 1,
			job,
			columns,
			theme,
		);
	}

	let hidden = workers.jobs.len() - shown;
	if hidden > 0 {
		let row = body.y + to_u16(shown);
		put(
			buf,
			body.x,
			row,
			&format!("+{hidden} more"),
			fg(theme.dim),
			body.right(),
		);
	}
}

#[derive(Clone, Copy)]
struct Columns {
	codec: usize,
	status: usize,
}

fn columns(jobs: &[WorkerJob], cells: u16) -> Columns {
	let room = (usize::from(cells) / 5).clamp(MIN_STATUS, MAX_STATUS);
	Columns {
		codec: jobs
			.iter()
			.map(|job| width(&job.codec))
			.max()
			.unwrap_or(0)
			.min(MAX_CODEC),
		status: jobs
			.iter()
			.map(|job| width(status(job)))
			.max()
			.unwrap_or(0)
			.clamp(MIN_STATUS, room),
	}
}

fn status(job: &WorkerJob) -> &str {
	match (job.over_limit, job.work, &job.stage) {
		(true, _, _) => "limit",
		(false, WorkKind::Encode, Some(stage)) => stage.as_str(),
		(false, WorkKind::Encode, None) => label(job.state),
		(false, work, _) => work.label(),
	}
}

fn worker_line(buf: &mut Buffer, area: Rect, number: usize, job: &WorkerJob, columns: Columns, theme: &Theme) {
	let (y, right) = (area.y, area.right());
	let state_color = if job.over_limit {
		theme.warn
	} else {
		theme.state(job.state)
	};
	let name_share = if area.width >= 60 { 30 } else { 38 };
	let name_width = (usize::from(area.width) * name_share / 100).max(10);

	let mut x = put(buf, area.x, y, &format!("#{number} "), fg(theme.dim), right);
	if columns.codec > 0 {
		x = put(
			buf,
			x,
			y,
			&fit(&job.codec, columns.codec + 1),
			bold(theme.accent),
			right,
		);
	}
	x = put(buf, x, y, &fit_middle(&job.name, name_width), fg(theme.text), right);
	x = put(buf, x + 1, y, &fit(status(job), columns.status), fg(state_color), right);

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

#[cfg(test)]
mod tests {
	use super::*;
	use crate::mock::MockSource;
	use crate::source::ArchiveSource;
	use crate::theme::ColorDepth;
	use crate::view::{FileId, FileState, Library};

	fn job(codec: &str, stage: &str) -> WorkerJob {
		WorkerJob {
			file: FileId(1),
			folder: Library::new("root").root(),
			work: WorkKind::Encode,
			codec: codec.into(),
			name: "holiday-photo-1.jpg".into(),
			state: FileState::Encoding,
			stage: Some(stage.into()),
			activity: JobActivity::Progress {
				fraction: 0.5,
				fps: None,
				speed: None,
			},
			over_limit: false,
		}
	}

	fn rows(jobs: &[WorkerJob], width: u16) -> Vec<String> {
		let theme = Theme::dark(ColorDepth::TrueColor);
		let columns = columns(jobs, width);
		let mut buf = Buffer::empty(Rect::new(0, 0, width, to_u16(jobs.len())));
		for (index, job) in jobs.iter().enumerate() {
			worker_line(
				&mut buf,
				Rect::new(0, to_u16(index), width, 1),
				index + 1,
				job,
				columns,
				&theme,
			);
		}
		(0..buf.area.height)
			.map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
			.collect()
	}

	fn view() -> ArchiveView {
		let mut view = MockSource::new(7).view().clone();
		view.overall.eta = Some(std::time::Duration::from_hours(3));
		view.overall.originals_deleted = 0;
		view.overall.parity = None;
		view
	}

	fn panels(view: &ArchiveView, width: u16) -> Vec<String> {
		let theme = Theme::dark(ColorDepth::TrueColor);
		let spinner = ThrobberState::default();
		let area = Rect::new(0, 0, width, height(width, view, &theme, &spinner));
		let mut buf = Buffer::empty(area);
		let state = OverallState {
			spinner: &spinner,
			active: None,
		};
		render(&mut buf, area, view, &theme, &state, &mut HitMap::default());
		(0..area.height)
			.map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
			.collect()
	}

	#[test]
	fn every_outcome_group_stays_on_screen_and_only_the_words_give_way() {
		let view = view();
		let outcomes = |width| panels(&view, width).into_iter().find(|row| row.contains("= ")).unwrap();

		let wide = outcomes(120);
		assert!(
			wide.contains(" issues ") && wide.contains(" kept ") && wide.contains(" dup"),
			"{wide}"
		);
		assert!(wide.contains("↪ "), "{wide}");

		let narrow = outcomes(100);
		assert!(
			narrow.contains("! ") && narrow.contains("= ") && narrow.contains("↪ "),
			"{narrow}"
		);
		assert!(!narrow.contains("kept"), "{narrow}");
	}

	#[test]
	fn the_panels_leave_the_eta_and_the_disk_to_the_header() {
		let rows = panels(&view(), 100);

		assert_eq!(rows.len(), 7, "{rows:#?}");
		assert!(
			rows.iter().all(|row| !row.contains("ETA") && !row.contains("free")),
			"{rows:#?}"
		);
		assert!(
			rows[5].contains("Saved"),
			"the last row inside the border is used: {rows:#?}"
		);
		assert!(rows[5].contains("ffmpeg"), "{rows:#?}");
	}

	#[test]
	fn the_top_grows_only_for_lines_that_would_not_fit() {
		let theme = Theme::dark(ColorDepth::TrueColor);
		let spinner = ThrobberState::default();
		let mut view = view();
		assert_eq!(height(100, &view, &theme, &spinner), 7);

		view.workers.jobs = (0..8).map(|_| job("AVIF", "encoding")).collect();
		assert_eq!(
			height(100, &view, &theme, &spinner),
			7,
			"the worker count never moves the top"
		);
		view.workers.jobs.clear();
		assert_eq!(height(100, &view, &theme, &spinner), 7);

		view.overall.originals_deleted = 12;
		assert_eq!(height(100, &view, &theme, &spinner), 8);
		assert!(panels(&view, 100)[6].contains("Saved"));

		view.overall.originals_deleted = 0;
		view.overall.parity = Some(Parity {
			source_files: 44_463,
			archive_files: 44_461,
			excluded: 2,
		});
		assert_eq!(height(100, &view, &theme, &spinner), 9);
		assert_eq!(height(160, &view, &theme, &spinner), 8);
		assert!(panels(&view, 100)[7].contains("excluded"));
	}

	#[test]
	fn hidden_workers_are_counted_on_the_last_row() {
		let mut view = view();
		view.workers.jobs = (0..8).map(|_| job("AVIF", "encoding")).collect();

		let rows = panels(&view, 120);
		assert!(rows[4].contains("#4 AVIF"), "{rows:#?}");
		assert!(rows[5].contains("+4 more"), "{rows:#?}");
		assert!(!rows.iter().any(|row| row.contains("#5 ")), "{rows:#?}");

		view.workers.jobs.truncate(5);
		let rows = panels(&view, 120);
		assert!(rows[5].contains("#5 AVIF"), "{rows:#?}");
		assert!(!rows.iter().any(|row| row.contains("more")), "{rows:#?}");
	}

	#[test]
	fn stages_show_whole_and_gauges_line_up_without_a_codec() {
		let rows = rows(&[job("", "verifying"), job("", "encoding")], 80);

		assert!(rows[0].starts_with("#1 holiday-photo-1.jpg"), "{rows:?}");
		assert!(rows[0].contains("verifying"), "{rows:?}");
		assert!(rows[1].contains("encoding"), "{rows:?}");
		assert_eq!(rows[0].find('━'), rows[1].find('━'), "{rows:?}");
	}

	#[test]
	fn long_stages_leave_room_for_the_gauge_on_narrow_panels() {
		let rows = rows(&[job("AVIF", "waiting for memory")], 40);

		assert!(rows[0].starts_with("#1 AVIF "), "{rows:?}");
		assert!(rows[0].contains("50.0%"), "{rows:?}");
	}
}
