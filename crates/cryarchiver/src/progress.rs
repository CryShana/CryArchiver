use std::ffi::OsStr;
use std::fmt::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use console::{Style, Term};
use cryarchiver_engine::engine::{EncodeAgain, JobEvent, Stage};
use cryarchiver_engine::quality::vmaf::Score;
use cryarchiver_engine::text::{display_name, fit_middle};
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};

use crate::report::file_label;

const NAME_WIDTH: usize = 40;
const BAR_WIDTH: usize = 24;
/// The narrowest bar, and the narrowest that still holds its percent.
const MIN_BAR_WIDTH: usize = 8;
const MIN_NAME_WIDTH: usize = 8;
/// Below this many columns the name and the bar shrink with the terminal.
const WIDE_TERMINAL: usize = 100;
const BAR_SCALE: u64 = 1000;
const TICK: Duration = Duration::from_millis(100);
const SWEEP_CELLS: usize = 3;

/// One file's line while it runs.
pub struct Item {
	bar: ProgressBar,
	name: String,
	verbose: bool,
	status: Mutex<Status>,
	shown: Mutex<Vec<String>>,
}

impl Item {
	pub fn new(multi: &MultiProgress, source: &Path, quiet: bool, verbose: bool, colour: bool) -> Self {
		let columns = Term::stderr().size_checked().map(|(_, columns)| usize::from(columns));
		let (name_width, bar_width) = layout(columns);

		let bar = multi.add(ProgressBar::no_length().with_style(line_style(bar_width, colour)));
		let name = file_label(source);
		bar.set_prefix(fit_middle(&name, name_width));

		let status = Status::default();
		bar.set_message(status.label());
		if !quiet {
			bar.enable_steady_tick(TICK);
		}

		Self {
			bar,
			name,
			verbose,
			status: Mutex::new(status),
			shown: Mutex::new(Vec::new()),
		}
	}

	/// Shows `event` on the line, and returns a line to print above the bars for an event that earns one.
	pub fn apply(&self, event: &JobEvent) -> Option<String> {
		if let JobEvent::Warning(warning) = event {
			let mut shown = self.shown.lock().unwrap();
			if !self.verbose || shown.contains(warning) {
				return None;
			}

			shown.push(warning.clone());
			return Some(format!("  ⚠ {}: {}", self.name, display_name(OsStr::new(warning))));
		}

		if let JobEvent::EncodeAgain(again) = event {
			self.shown.lock().unwrap().push(again.warning());
		}

		let mut status = self.status.lock().unwrap();
		let line = status.apply(event).map(|text| format!("↻ {}  {text}", self.name));
		self.draw(&status);
		line
	}

	pub fn wait(&self, text: String) {
		let mut status = self.status.lock().unwrap();
		status.wait(text);
		self.draw(&status);
	}

	/// Clears the line and hands back the warnings the line or the live output already told.
	pub fn finish(&self) -> Vec<String> {
		self.bar.finish_and_clear();
		std::mem::take(&mut *self.shown.lock().unwrap())
	}

	fn draw(&self, status: &Status) {
		match status.fraction() {
			Some(fraction) => {
				if self.bar.length() != Some(BAR_SCALE) {
					self.bar.set_length(BAR_SCALE);
				}
				self.bar.set_position(permille(fraction));
			}
			None => self.bar.unset_length(),
		}
		self.bar.set_message(status.label());
	}
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Probe {
	number: usize,
	most: usize,
	crf: f64,
}

/// What a file's line says: its stage, how far the stage has come when that is known, and the search's last result.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
	stage: Stage,
	fraction: Option<f64>,
	parts: Option<(usize, usize)>,
	probe: Option<Probe>,
	probed: Option<(f64, Score)>,
	again: Option<EncodeAgain>,
	waiting: Option<String>,
}

impl Default for Status {
	fn default() -> Self {
		Self {
			stage: Stage::Detect,
			fraction: None,
			parts: None,
			probe: None,
			probed: None,
			again: None,
			waiting: None,
		}
	}
}

impl Status {
	/// Takes `event` in; a repeated encode also returns the line that announces it.
	#[allow(clippy::cast_precision_loss)]
	pub fn apply(&mut self, event: &JobEvent) -> Option<String> {
		match event {
			JobEvent::Stage(stage) => self.enter(*stage),
			JobEvent::Progress(fraction) => {
				if self.parts.is_none() && self.probe.is_none() {
					self.fraction = Some(*fraction);
				}
			}
			JobEvent::Parts { done, total } => {
				self.parts = Some((*done, *total));
				self.fraction = (*total > 0).then(|| *done as f64 / *total as f64);
			}
			JobEvent::Probe { number, most, crf } => {
				self.probe = Some(Probe {
					number: *number,
					most: *most,
					crf: *crf,
				});
				self.parts = None;
				self.fraction = Some(0.0);
			}
			JobEvent::Probed { crf, score } => self.probed = Some((*crf, *score)),
			JobEvent::EncodeAgain(again) => {
				self.again = Some(*again);
				return Some(again_text(again));
			}
			JobEvent::Warning(_) | JobEvent::Decision(_) => {}
		}
		None
	}

	pub fn wait(&mut self, text: String) {
		self.waiting = Some(text);
		self.fraction = None;
	}

	/// The stage's progress, or `None` while it has no measure yet.
	pub fn fraction(&self) -> Option<f64> {
		self.fraction
	}

	pub fn label(&self) -> String {
		if let Some(waiting) = &self.waiting {
			return waiting.clone();
		}

		match (self.stage, self.again) {
			(Stage::Encode, Some(again)) => {
				return format!(
					"encoding again ({}/{}) at CRF {}",
					again.attempt, again.attempts, again.crf
				);
			}
			(Stage::Verify, Some(again)) => return format!("verifying again ({}/{})", again.attempt, again.attempts),
			_ => {}
		}

		let mut label = stage_label(self.stage).to_owned();
		match (self.stage, self.probe, self.parts) {
			(Stage::Sample, Some(probe), parts) => {
				let _ = write!(
					label,
					": probe {} of up to {}, CRF {}",
					probe.number, probe.most, probe.crf
				);
				if let Some((done, total)) = parts {
					let _ = write!(label, ", sample {done}/{total}");
				}
				if let Some((crf, score)) = self.probed {
					let _ = write!(label, " · CRF {crf} scored VMAF {:.1} / {:.1}", score.mean, score.p5);
				}
			}
			(Stage::Sample, None, Some((done, total))) => {
				let _ = write!(label, ": sample {done}/{total}");
			}
			(Stage::Analyze, _, Some((done, total))) => {
				let _ = write!(label, " {done}/{total} segments");
			}
			_ => {}
		}
		label
	}

	/// A new stage starts without a measure; a repeated encode stays named until its check is done.
	fn enter(&mut self, stage: Stage) {
		let again = self.again.filter(|_| matches!(stage, Stage::Encode | Stage::Verify));
		*self = Self {
			stage,
			again,
			..Self::default()
		};
	}
}

fn again_text(again: &EncodeAgain) -> String {
	format!(
		"encoding again ({}/{}): the full check measured VMAF {:.1} / {:.1}, target {} / {}; now CRF {}",
		again.attempt,
		again.attempts,
		again.measured.mean,
		again.measured.p5,
		again.target.mean,
		again.target.floor,
		again.crf
	)
}

fn stage_label(stage: Stage) -> &'static str {
	match stage {
		Stage::Detect => "detecting",
		Stage::Probe => "probing",
		Stage::Plan => "planning",
		Stage::Queued => "queued",
		Stage::Disc => "reading disc",
		Stage::WaitingForMemory => "waiting for memory",
		Stage::Analyze => "analysing",
		Stage::Sample => "sampling",
		Stage::Encode => "encoding",
		Stage::Mux => "muxing",
		Stage::Metadata => "cover art",
		Stage::Verify => "verifying",
		Stage::Commit => "saving",
	}
}

/// The name, the bar, the time the file has taken and its status.
fn line_style(bar_width: usize, colour: bool) -> ProgressStyle {
	ProgressStyle::with_template("{prefix} {meter} {clock} {wide_msg}")
		.expect("valid template")
		.with_key("meter", move |state: &ProgressState, out: &mut dyn fmt::Write| {
			let phase = state.elapsed().as_millis() / TICK.as_millis();
			let phase = u64::try_from(phase).unwrap_or(u64::MAX);
			let _ = out.write_str(&render_bar(bar_fraction(state), bar_width, phase, colour));
		})
		.with_key("clock", |state: &ProgressState, out: &mut dyn fmt::Write| {
			let _ = out.write_str(&elapsed_text(state.elapsed()));
		})
}

/// The name's and the bar's widths for a terminal `columns` wide.
fn layout(columns: Option<usize>) -> (usize, usize) {
	match columns {
		Some(columns) if columns < WIDE_TERMINAL => (
			(columns * 3 / 10).max(MIN_NAME_WIDTH),
			(columns / 5).clamp(MIN_BAR_WIDTH, BAR_WIDTH),
		),
		_ => (NAME_WIDTH, BAR_WIDTH),
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
	Done,
	Todo,
	PercentDone,
	PercentTodo,
}

/// A bar `width` cells wide: filled to `fraction` with its percent in the middle, or without a fraction a short
/// block sweeping back and forth with `phase`.
fn render_bar(fraction: Option<f64>, width: usize, phase: u64, colour: bool) -> String {
	let cells = match fraction {
		Some(fraction) => meter_cells(fraction, width),
		None => sweep_cells(width, phase),
	};

	let mut out = String::new();
	for run in cells.chunk_by(|a, b| a.0 == b.0) {
		let text: String = run.iter().map(|&(_, glyph)| glyph).collect();
		let _ = write!(out, "{}", cell_style(run[0].0, colour).apply_to(text));
	}
	out
}

#[allow(
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss,
	clippy::cast_precision_loss
)]
fn meter_cells(fraction: f64, width: usize) -> Vec<(Cell, char)> {
	let fraction = fraction.clamp(0.0, 1.0);
	let filled = (fraction * width as f64).round() as usize;
	let percent: Vec<char> = format!(" {}% ", permille(fraction) / 10).chars().collect();
	let start = (width >= MIN_BAR_WIDTH).then(|| (width - percent.len()) / 2);

	(0..width)
		.map(|cell| {
			let done = cell < filled;
			let glyph = start
				.and_then(|start| cell.checked_sub(start))
				.and_then(|offset| percent.get(offset));
			match (glyph, done) {
				(Some(&glyph), true) => (Cell::PercentDone, glyph),
				(Some(&glyph), false) => (Cell::PercentTodo, glyph),
				(None, true) => (Cell::Done, '━'),
				(None, false) => (Cell::Todo, '─'),
			}
		})
		.collect()
}

#[allow(clippy::cast_possible_truncation)]
fn sweep_cells(width: usize, phase: u64) -> Vec<(Cell, char)> {
	let block = SWEEP_CELLS.min(width);
	let travel = width - block;
	let start = if travel == 0 {
		0
	} else {
		let step = (phase % (2 * travel as u64)) as usize;
		if step <= travel { step } else { 2 * travel - step }
	};

	(0..width)
		.map(|cell| {
			if (start..start + block).contains(&cell) {
				(Cell::Done, '━')
			} else {
				(Cell::Todo, '─')
			}
		})
		.collect()
}

fn cell_style(cell: Cell, colour: bool) -> Style {
	let style = match cell {
		Cell::Done | Cell::PercentTodo => Style::new().cyan(),
		Cell::Todo => Style::new().blue(),
		Cell::PercentDone => Style::new().black().on_cyan(),
	};
	style.force_styling(colour)
}

#[allow(clippy::cast_precision_loss)]
fn bar_fraction(state: &ProgressState) -> Option<f64> {
	state
		.len()
		.filter(|len| *len > 0)
		.map(|len| state.pos() as f64 / len as f64)
}

/// `03:12`, or `1:02:05` from an hour on.
fn elapsed_text(elapsed: Duration) -> String {
	let seconds = elapsed.as_secs();
	let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);

	if hours > 0 {
		format!("{hours}:{minutes:02}:{seconds:02}")
	} else {
		format!("{minutes:02}:{seconds:02}")
	}
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn permille(fraction: f64) -> u64 {
	(fraction.clamp(0.0, 1.0) * 1000.0).round() as u64
}

#[cfg(test)]
mod tests {
	use std::io;
	use std::sync::Arc;

	use cryarchiver_engine::quality::search::Target;
	use indicatif::{ProgressDrawTarget, TermLike};

	use super::*;
	use crate::report::Report;

	#[test]
	fn the_percent_sits_in_the_middle_of_the_bar() {
		let bar = render_bar(Some(0.42), 24, 0, false);

		assert_eq!(bar, "━━━━━━━━━ 42% ──────────");
		assert_eq!(bar.chars().count(), 24);
		assert_eq!(render_bar(Some(0.0), 24, 0, false), "────────── 0% ──────────");
		assert_eq!(render_bar(Some(1.0), 24, 0, false), "━━━━━━━━━ 100% ━━━━━━━━━");
		assert_eq!(
			render_bar(Some(0.9996), 24, 0, false),
			render_bar(Some(1.0), 24, 0, false)
		);
		assert!(render_bar(Some(0.999), 24, 0, false).contains(" 99% "));
	}

	#[test]
	fn the_percent_takes_the_fill_colour_as_the_bar_passes_it() {
		let paint = |style: Style, text: &str| style.force_styling(true).apply_to(text).to_string();

		let half = render_bar(Some(0.5), 10, 0, true);

		assert_eq!(
			half,
			[
				paint(Style::new().cyan(), "━━"),
				paint(Style::new().black().on_cyan(), " 50"),
				paint(Style::new().cyan(), "% "),
				paint(Style::new().blue(), "───"),
			]
			.concat()
		);
		assert!(render_bar(Some(1.0), 10, 0, true).contains(&paint(Style::new().black().on_cyan(), " 100% ")));
		assert!(render_bar(Some(0.0), 10, 0, true).contains(&paint(Style::new().cyan(), " 0% ")));
	}

	#[test]
	fn without_colour_the_bar_is_plain_text() {
		for fraction in [Some(0.0), Some(0.37), Some(1.0), None] {
			let bar = render_bar(fraction, 24, 5, false);

			assert!(!bar.contains('\x1b'), "{bar:?}");
			assert_eq!(bar.chars().count(), 24, "{bar:?}");
		}
	}

	#[test]
	fn a_narrow_bar_drops_the_percent_and_the_line_shrinks_to_the_terminal() {
		const SPACES_AND_LONGEST_CLOCK: usize = 3 + "1:02:05".len();
		const NARROWEST_FIT: usize = MIN_NAME_WIDTH + MIN_BAR_WIDTH + SPACES_AND_LONGEST_CLOCK;

		assert_eq!(render_bar(Some(0.5), 7, 0, false), "━━━━───");
		assert_eq!(render_bar(Some(0.5), 8, 0, false), "━ 50% ──");

		assert_eq!(layout(None), (40, 24));
		assert_eq!(layout(Some(160)), (40, 24));
		assert_eq!(layout(Some(80)), (24, 16));
		assert_eq!(layout(Some(30)), (9, 8));

		for columns in [NARROWEST_FIT, 30, 40, 60, 99] {
			let (name, bar) = layout(Some(columns));
			assert!(
				name + bar + SPACES_AND_LONGEST_CLOCK <= columns,
				"{columns}: {name} {bar}"
			);
		}
		for columns in [10, NARROWEST_FIT - 1] {
			assert_eq!(layout(Some(columns)), (MIN_NAME_WIDTH, MIN_BAR_WIDTH), "{columns}");
		}
	}

	#[test]
	fn a_stage_without_a_fraction_sweeps_back_and_forth() {
		let frames: Vec<String> = (0..48).map(|phase| render_bar(None, 10, phase, false)).collect();

		for frame in &frames {
			assert!(!frame.contains('%'), "{frame}");
			assert_eq!(frame.matches('━').count(), 3, "{frame}");
		}
		assert_eq!(frames[0], "━━━───────");
		assert_eq!(frames[7], "───────━━━");
		assert_eq!(frames[8], "──────━━━─");
		assert_eq!(frames[14], frames[0]);
		assert_eq!(render_bar(None, 2, 3, false), "━━");
	}

	#[derive(Debug, Clone, Default)]
	struct Screen(Arc<Mutex<String>>);

	impl TermLike for Screen {
		fn width(&self) -> u16 {
			100
		}

		fn move_cursor_up(&self, _: usize) -> io::Result<()> {
			Ok(())
		}

		fn move_cursor_down(&self, _: usize) -> io::Result<()> {
			Ok(())
		}

		fn move_cursor_right(&self, _: usize) -> io::Result<()> {
			Ok(())
		}

		fn move_cursor_left(&self, _: usize) -> io::Result<()> {
			Ok(())
		}

		fn write_line(&self, s: &str) -> io::Result<()> {
			let mut screen = self.0.lock().unwrap();
			screen.push_str(s);
			screen.push('\n');
			Ok(())
		}

		fn write_str(&self, s: &str) -> io::Result<()> {
			self.0.lock().unwrap().push_str(s);
			Ok(())
		}

		fn clear_line(&self) -> io::Result<()> {
			Ok(())
		}

		fn flush(&self) -> io::Result<()> {
			Ok(())
		}
	}

	fn drawn(length: Option<u64>, position: u64) -> String {
		let screen = Screen::default();
		let target = ProgressDrawTarget::term_like(Box::new(screen.clone()));
		let bar = ProgressBar::with_draw_target(length, target).with_style(line_style(24, false));
		bar.set_prefix("a.mkv");
		bar.set_message("encoding");
		bar.set_position(position);
		bar.abandon();

		let screen = screen.0.lock().unwrap();
		let last = screen.rfind("a.mkv").unwrap();
		screen[last..].trim_end().to_owned()
	}

	fn assert_clock_then_status(rest: &str, line: &str) {
		let clock = rest.strip_suffix(" encoding").unwrap_or_else(|| panic!("{line:?}"));

		assert_eq!(clock.len(), 5, "{line:?}");
		assert_eq!(clock.as_bytes()[2], b':', "{line:?}");
	}

	#[test]
	fn a_line_shows_the_name_the_bar_the_time_and_the_status() {
		let line = drawn(Some(BAR_SCALE), 420);
		let rest = line
			.strip_prefix("a.mkv ━━━━━━━━━ 42% ────────── ")
			.unwrap_or_else(|| panic!("{line:?}"));
		assert_clock_then_status(rest, &line);

		let line = drawn(None, 0);
		let rest = line.strip_prefix("a.mkv ").unwrap_or_else(|| panic!("{line:?}"));
		let (bar, rest) = rest.split_at(rest.char_indices().nth(24).unwrap().0);
		assert_eq!(bar.matches('━').count(), SWEEP_CELLS, "{line:?}");
		assert_eq!(bar.matches('─').count(), 24 - SWEEP_CELLS, "{line:?}");
		assert_clock_then_status(rest.trim_start(), &line);
	}

	#[test]
	fn elapsed_time_reads_minutes_and_seconds() {
		assert_eq!(elapsed_text(Duration::ZERO), "00:00");
		assert_eq!(elapsed_text(Duration::from_millis(192_900)), "03:12");
		assert_eq!(elapsed_text(Duration::from_secs(3599)), "59:59");
		assert_eq!(elapsed_text(Duration::from_secs(3725)), "1:02:05");
	}

	fn score(mean: f64, p5: f64) -> Score {
		Score { mean, p5, frames: 90 }
	}

	#[test]
	fn analysis_and_sampling_show_their_real_progress() {
		let mut status = Status::default();
		assert_eq!((status.label().as_str(), status.fraction()), ("detecting", None));

		status.apply(&JobEvent::Stage(Stage::Analyze));
		assert_eq!((status.label().as_str(), status.fraction()), ("analysing", None));
		status.apply(&JobEvent::Parts { done: 3, total: 12 });
		assert_eq!(
			(status.label().as_str(), status.fraction()),
			("analysing 3/12 segments", Some(0.25))
		);

		status.apply(&JobEvent::Stage(Stage::Sample));
		assert_eq!((status.label().as_str(), status.fraction()), ("sampling", None));
		status.apply(&JobEvent::Progress(0.0));
		status.apply(&JobEvent::Probe {
			number: 1,
			most: 8,
			crf: 30.0,
		});
		status.apply(&JobEvent::Parts { done: 0, total: 3 });
		assert_eq!(
			(status.label().as_str(), status.fraction()),
			("sampling: probe 1 of up to 8, CRF 30, sample 0/3", Some(0.0))
		);

		status.apply(&JobEvent::Parts { done: 2, total: 4 });
		status.apply(&JobEvent::Progress(0.125));
		assert_eq!(status.fraction(), Some(0.5), "the probe's samples drive the bar");

		status.apply(&JobEvent::Probed {
			crf: 30.0,
			score: score(94.12, 90.04),
		});
		status.apply(&JobEvent::Probe {
			number: 2,
			most: 8,
			crf: 34.25,
		});
		status.apply(&JobEvent::Parts { done: 1, total: 3 });
		assert_eq!(
			status.label(),
			"sampling: probe 2 of up to 8, CRF 34.25, sample 1/3 · CRF 30 scored VMAF 94.1 / 90.0"
		);

		status.apply(&JobEvent::Stage(Stage::Sample));
		status.apply(&JobEvent::Parts { done: 1, total: 3 });
		assert_eq!(status.label(), "sampling: sample 1/3");
	}

	#[test]
	fn every_stage_starts_without_a_fraction_until_one_arrives() {
		let mut status = Status::default();

		for stage in STAGES {
			status.apply(&JobEvent::Progress(0.7));
			status.apply(&JobEvent::Stage(stage));
			assert_eq!(status.fraction(), None, "{stage:?}");
			assert_eq!(status.label(), stage_label(stage));

			status.apply(&JobEvent::Progress(0.4));
			assert_eq!(status.fraction(), Some(0.4), "{stage:?}");
		}

		status.wait("waiting 10 s for the disk".into());
		assert_eq!(
			(status.label().as_str(), status.fraction()),
			("waiting 10 s for the disk", None)
		);
		status.apply(&JobEvent::Stage(Stage::Detect));
		assert_eq!(status.label(), "detecting");
	}

	fn repeat() -> EncodeAgain {
		EncodeAgain {
			attempt: 2,
			attempts: 2,
			measured: score(91.24, 84.0),
			target: Target {
				mean: 93.0,
				floor: 89.0,
			},
			from_crf: 28.5,
			crf: 24.0,
		}
	}

	#[test]
	fn a_repeated_encode_is_announced_and_labels_its_stages() {
		let mut status = Status::default();

		status.apply(&JobEvent::Stage(Stage::Encode));
		assert_eq!(status.label(), "encoding");
		status.apply(&JobEvent::Stage(Stage::Verify));
		assert_eq!(status.label(), "verifying");

		assert_eq!(
			status.apply(&JobEvent::EncodeAgain(repeat())).as_deref(),
			Some("encoding again (2/2): the full check measured VMAF 91.2 / 84.0, target 93 / 89; now CRF 24")
		);
		status.apply(&JobEvent::Stage(Stage::Encode));
		assert_eq!(
			(status.label().as_str(), status.fraction()),
			("encoding again (2/2) at CRF 24", None)
		);
		status.apply(&JobEvent::Progress(0.5));
		assert_eq!(status.fraction(), Some(0.5));
		status.apply(&JobEvent::Stage(Stage::Verify));
		assert_eq!(status.label(), "verifying again (2/2)");

		status.apply(&JobEvent::Stage(Stage::Mux));
		assert_eq!(status.label(), "muxing");
		status.apply(&JobEvent::Stage(Stage::Verify));
		assert_eq!(status.label(), "verifying");
	}

	#[test]
	fn an_item_prints_the_repeat_and_verbose_warnings_once_by_their_display_name() {
		let quiet = Report::new(true, false).item(Path::new("clips/a.mkv"));
		let verbose = Report::new(true, true).item(Path::new("clips/a.mkv"));
		let warning = JobEvent::Warning("bell\u{7} in the title".into());
		let again = JobEvent::EncodeAgain(repeat());

		assert_eq!(quiet.apply(&warning), None);
		assert_eq!(
			verbose.apply(&warning),
			Some("  ⚠ a.mkv: bell\\x07 in the title".to_owned())
		);
		assert_eq!(verbose.apply(&warning), None, "a warning is told once");
		assert!(
			quiet
				.apply(&again)
				.is_some_and(|line| line.starts_with("↻ a.mkv  encoding again (2/2): "))
		);
		assert_eq!(quiet.apply(&JobEvent::Progress(0.3)), None);

		assert_eq!(quiet.finish(), vec![repeat().warning()]);
		assert_eq!(verbose.finish(), vec!["bell\u{7} in the title".to_owned()]);
	}

	#[test]
	fn the_engines_note_about_a_repeat_is_not_told_again_after_its_announcement() {
		let again = repeat();

		for verbose in [false, true] {
			let item = Report::new(true, verbose).item(Path::new("clips/a.mkv"));

			assert!(item.apply(&JobEvent::EncodeAgain(again)).is_some());
			assert_eq!(
				item.apply(&JobEvent::Warning(again.warning())),
				None,
				"verbose: {verbose}"
			);
			assert_eq!(item.finish(), vec![again.warning()], "verbose: {verbose}");
		}
	}

	const STAGES: [Stage; 13] = [
		Stage::Detect,
		Stage::Probe,
		Stage::Plan,
		Stage::Queued,
		Stage::Disc,
		Stage::WaitingForMemory,
		Stage::Analyze,
		Stage::Sample,
		Stage::Encode,
		Stage::Mux,
		Stage::Metadata,
		Stage::Verify,
		Stage::Commit,
	];

	#[test]
	fn every_stage_has_a_label() {
		for stage in STAGES {
			assert!(!stage_label(stage).is_empty());
		}
	}
}
