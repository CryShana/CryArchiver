use std::ffi::OsStr;
use std::fmt::{self, Write};
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use console::{Style, Term};
use cryarchiver_engine::engine::{EncodeAgain, JobEvent, Stage};
use cryarchiver_engine::outcome::{JobOutcome, KeepReason};
use cryarchiver_engine::quality::vmaf::Score;
use cryarchiver_engine::text::{display_name, fit_middle};
use indicatif::{DecimalBytes, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

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
const SHORT_TAIL: usize = 3;
const VERBOSE_TAIL: usize = 20;

#[derive(Clone)]
pub struct Report {
	multi: MultiProgress,
	quiet: bool,
	verbose: bool,
	colour: bool,
}

impl Report {
	pub fn new(quiet: bool, verbose: bool) -> Self {
		let multi = MultiProgress::new();
		if quiet {
			multi.set_draw_target(ProgressDrawTarget::hidden());
		}

		Self {
			multi,
			quiet,
			verbose,
			colour: console::colors_enabled_stderr(),
		}
	}

	pub fn tail_lines(&self) -> usize {
		if self.verbose { VERBOSE_TAIL } else { SHORT_TAIL }
	}

	pub fn item(&self, source: &Path) -> Item {
		let columns = Term::stderr().size_checked().map(|(_, columns)| usize::from(columns));
		let (name_width, bar_width) = layout(columns);

		let bar = self
			.multi
			.add(ProgressBar::no_length().with_style(line_style(bar_width, self.colour)));
		let name = file_label(source);
		bar.set_prefix(fit_middle(&name, name_width));

		let status = Status::default();
		bar.set_message(status.label());
		if !self.quiet {
			bar.enable_steady_tick(TICK);
		}

		Item {
			bar,
			name,
			verbose: self.verbose,
			status: Mutex::new(status),
			shown: Mutex::new(Vec::new()),
		}
	}

	pub fn print(&self, line: &str, failed: bool) {
		if !self.quiet || failed {
			self.multi.suspend(|| {
				let _ = writeln!(io::stdout(), "{line}");
			});
		}
	}
}

/// One file's line while it runs.
pub struct Item {
	bar: ProgressBar,
	name: String,
	verbose: bool,
	status: Mutex<Status>,
	shown: Mutex<Vec<String>>,
}

impl Item {
	/// Shows `event` on the line, and returns a line to print above the bars for an event that earns one.
	pub fn apply(&self, event: &JobEvent) -> Option<String> {
		if let JobEvent::Warning(warning) = event {
			if !self.verbose {
				return None;
			}

			self.shown.lock().unwrap().push(warning.clone());
			return Some(format!("  ⚠ {}: {}", self.name, display_name(OsStr::new(warning))));
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

	/// Clears the line and hands back the warnings it already printed.
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
			JobEvent::Warning(_) => {}
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

#[derive(Debug, Default)]
pub struct Totals {
	encoded: usize,
	kept: usize,
	skipped: usize,
	failed: usize,
	saved: u64,
}

impl Totals {
	pub fn add(&mut self, outcome: &JobOutcome) {
		match outcome {
			JobOutcome::Encoded {
				src_bytes, out_bytes, ..
			} => {
				self.encoded += 1;
				self.saved += src_bytes.saturating_sub(*out_bytes);
			}
			JobOutcome::KeptOriginal { .. } => self.kept += 1,
			JobOutcome::Skipped { .. } => self.skipped += 1,
			JobOutcome::Failed(_) => self.failed += 1,
		}
	}

	pub fn line(&self) -> String {
		format!(
			"{} encoded · {} kept · {} skipped · {} failed · saved {}",
			self.encoded,
			self.kept,
			self.skipped,
			self.failed,
			DecimalBytes(self.saved)
		)
	}

	pub fn exit_code(&self) -> u8 {
		u8::from(self.failed > 0)
	}
}

#[cfg(test)]
pub fn outcome_line(source: &Path, outcome: &JobOutcome) -> String {
	outcome_line_with(source, outcome, SHORT_TAIL, &[])
}

/// The outcome's line, leaving out the warnings in `shown`, which were printed as they happened.
pub fn outcome_line_with(source: &Path, outcome: &JobOutcome, tail_lines: usize, shown: &[String]) -> String {
	let name = file_label(source);

	match outcome {
		JobOutcome::Encoded {
			output,
			src_bytes,
			out_bytes,
			warnings,
		} => {
			let saved = 100.0 - percent(*out_bytes, *src_bytes);
			let change = if saved < 0.0 {
				format!("+{:.0} %", -saved)
			} else {
				format!("−{saved:.0} %")
			};
			let mut line = format!(
				"✓ {name} → {}  {} → {} ({change})",
				file_label(output),
				DecimalBytes(*src_bytes),
				DecimalBytes(*out_bytes)
			);
			for warning in warnings.iter().filter(|warning| !shown.contains(warning)) {
				let _ = write!(line, "\n    ⚠ {}", display_name(OsStr::new(warning)));
			}
			line
		}
		JobOutcome::KeptOriginal { reason } => format!("= {name}  kept: {}", keep_reason(reason)),
		JobOutcome::Skipped { reason } => format!("· {name}  skipped: {reason}"),
		JobOutcome::Failed(err) => {
			let mut line = format!("✗ {name}  failed: {err}");
			for log in &err.log_tail[err.log_tail.len().saturating_sub(tail_lines)..] {
				let _ = write!(line, "\n    {}", display_name(OsStr::new(log)));
			}
			line
		}
	}
}

pub fn file_label(path: &Path) -> String {
	display_name(path.file_name().unwrap_or(path.as_os_str()))
}

fn keep_reason(reason: &KeepReason) -> String {
	match reason {
		KeepReason::NotSmaller { src_bytes, out_bytes } if out_bytes > src_bytes => {
			format!(
				"the encode came out larger than the source ({} → {})",
				DecimalBytes(*src_bytes),
				DecimalBytes(*out_bytes)
			)
		}
		KeepReason::NotSmaller { src_bytes, out_bytes } => {
			format!(
				"encoding saved too little ({} → {})",
				DecimalBytes(*src_bytes),
				DecimalBytes(*out_bytes)
			)
		}
		KeepReason::PredictedNotSmaller {
			src_bytes,
			predicted_bytes,
		} => {
			format!(
				"encoding would save too little ({} → predicted {})",
				DecimalBytes(*src_bytes),
				DecimalBytes(*predicted_bytes)
			)
		}
		KeepReason::AlreadyEfficient(why) | KeepReason::Unsupported(why) | KeepReason::QualityUnreachable(why) => {
			why.clone()
		}
		KeepReason::InvalidIcc(why) => format!("invalid ICC profile: {why}"),
		KeepReason::DecodeBomb { megapixels } => {
			format!("the header claims {megapixels} MP, above the 500 MP safety limit")
		}
	}
}

fn stage_label(stage: Stage) -> &'static str {
	match stage {
		Stage::Detect => "detecting",
		Stage::Probe => "probing",
		Stage::Plan => "planning",
		Stage::Queued => "queued",
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

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn permille(fraction: f64) -> u64 {
	(fraction.clamp(0.0, 1.0) * 1000.0).round() as u64
}

#[allow(clippy::cast_precision_loss)]
fn percent(part: u64, whole: u64) -> f64 {
	if whole == 0 {
		100.0
	} else {
		part as f64 / whole as f64 * 100.0
	}
}

#[cfg(test)]
mod tests {
	use std::path::PathBuf;
	use std::sync::Arc;

	use cryarchiver_engine::outcome::{ErrorKind, JobError};
	use cryarchiver_engine::quality::search::Target;
	use indicatif::TermLike;

	use super::*;

	#[test]
	fn describes_each_outcome_on_one_line() {
		let source = Path::new("music/02. 夜明け.flac");
		let encoded = JobOutcome::Encoded {
			output: PathBuf::from("music/02. 夜明け.cry.opus"),
			src_bytes: 20_000_000,
			out_bytes: 2_000_000,
			warnings: Vec::new(),
		};
		let kept = JobOutcome::KeptOriginal {
			reason: KeepReason::AlreadyEfficient("already Opus".into()),
		};
		let predicted = JobOutcome::KeptOriginal {
			reason: KeepReason::PredictedNotSmaller {
				src_bytes: 5_800_000,
				predicted_bytes: 7_100_000,
			},
		};
		let failed = JobOutcome::Failed(JobError::from(ErrorKind::SourceDamaged(
			"decodes to 4.0 s but declares 10.0 s".into(),
		)));

		assert_eq!(
			outcome_line(source, &encoded),
			"✓ 02. 夜明け.flac → 02. 夜明け.cry.opus  20.00 MB → 2.00 MB (−90 %)"
		);
		assert_eq!(
			outcome_line(
				source,
				&JobOutcome::Encoded {
					output: PathBuf::from("music/02. 夜明け.cry.opus"),
					src_bytes: 6_430_000,
					out_bytes: 32_090_000,
					warnings: Vec::new(),
				}
			),
			"✓ 02. 夜明け.flac → 02. 夜明け.cry.opus  6.43 MB → 32.09 MB (+399 %)"
		);
		assert_eq!(outcome_line(source, &kept), "= 02. 夜明け.flac  kept: already Opus");
		assert_eq!(
			outcome_line(source, &predicted),
			"= 02. 夜明け.flac  kept: encoding would save too little (5.80 MB → predicted 7.10 MB)"
		);
		assert!(outcome_line(source, &failed).starts_with("✗ 02. 夜明け.flac  failed: source damaged"));
	}

	#[test]
	fn verbose_failures_show_more_of_the_tool_output() {
		let err = JobError::with_log(
			ErrorKind::EncodeFailed("x".into()),
			(0..30).map(|line| format!("line {line}")).collect(),
		);
		let failed = JobOutcome::Failed(err);

		let short = outcome_line(Path::new("a.mp4"), &failed);
		let long = outcome_line_with(Path::new("a.mp4"), &failed, 20, &[]);

		assert_eq!(short.lines().count(), 4);
		assert_eq!(long.lines().count(), 21);
		assert!(long.ends_with("line 29"), "{long}");
		assert_eq!(Report::new(false, true).tail_lines(), 20);
		assert_eq!(Report::new(false, false).tail_lines(), 3);
	}

	#[test]
	fn image_keep_reasons_are_described() {
		let source = Path::new("photo.jpg");
		let line = |reason| outcome_line(source, &JobOutcome::KeptOriginal { reason });

		assert_eq!(
			line(KeepReason::QualityUnreachable(
				"quality target not reached at a smaller size".into()
			)),
			"= photo.jpg  kept: quality target not reached at a smaller size"
		);
		assert_eq!(
			line(KeepReason::InvalidIcc("declares 4294967295 bytes".into())),
			"= photo.jpg  kept: invalid ICC profile: declares 4294967295 bytes"
		);
		assert_eq!(
			line(KeepReason::DecodeBomb { megapixels: 10_000 }),
			"= photo.jpg  kept: the header claims 10000 MP, above the 500 MP safety limit"
		);
	}

	#[test]
	fn totals_and_exit_code() {
		let mut totals = Totals::default();
		totals.add(&JobOutcome::Encoded {
			output: PathBuf::from("a.cry.opus"),
			src_bytes: 3_000,
			out_bytes: 1_000,
			warnings: Vec::new(),
		});
		totals.add(&JobOutcome::Skipped {
			reason: "not media".into(),
		});

		assert_eq!(
			totals.line(),
			"1 encoded · 0 kept · 1 skipped · 0 failed · saved 2.00 kB"
		);
		assert_eq!(totals.exit_code(), 0);

		totals.add(&JobOutcome::Failed(JobError::from(ErrorKind::Cancelled)));
		assert_eq!(totals.exit_code(), 1);
	}

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
		assert_eq!(render_bar(Some(0.5), 7, 0, false), "━━━━───");
		assert_eq!(render_bar(Some(0.5), 8, 0, false), "━ 50% ──");

		assert_eq!(layout(None), (40, 24));
		assert_eq!(layout(Some(160)), (40, 24));
		assert_eq!(layout(Some(80)), (24, 16));
		assert_eq!(layout(Some(30)), (9, 8));
		for columns in [20, 40, 60, 99] {
			let (name, bar) = layout(Some(columns));
			assert!(
				name + bar + "  00:00 ".len() < columns.max(30),
				"{columns}: {name} {bar}"
			);
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

	#[test]
	fn a_line_shows_the_name_the_bar_the_time_and_the_status() {
		assert_eq!(
			drawn(Some(BAR_SCALE), 420),
			"a.mkv ━━━━━━━━━ 42% ────────── 00:00 encoding"
		);
		assert_eq!(drawn(None, 0), "a.mkv ━━━───────────────────── 00:00 encoding");
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

	#[test]
	fn a_repeated_encode_is_announced_and_labels_its_stages() {
		let mut status = Status::default();
		let again = EncodeAgain {
			attempt: 2,
			attempts: 2,
			measured: score(91.24, 84.0),
			target: Target {
				mean: 93.0,
				floor: 89.0,
			},
			crf: 24.0,
		};

		status.apply(&JobEvent::Stage(Stage::Encode));
		assert_eq!(status.label(), "encoding");
		status.apply(&JobEvent::Stage(Stage::Verify));
		assert_eq!(status.label(), "verifying");

		assert_eq!(
			status.apply(&JobEvent::EncodeAgain(again)).as_deref(),
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
		let again = JobEvent::EncodeAgain(EncodeAgain {
			attempt: 2,
			attempts: 2,
			measured: score(91.2, 84.0),
			target: Target {
				mean: 93.0,
				floor: 89.0,
			},
			crf: 24.0,
		});

		assert_eq!(quiet.apply(&warning), None);
		assert_eq!(
			verbose.apply(&warning),
			Some(format!(
				"  ⚠ a.mkv: {}",
				display_name(OsStr::new("bell\u{7} in the title"))
			))
		);
		assert!(
			quiet
				.apply(&again)
				.is_some_and(|line| line.starts_with("↻ a.mkv  encoding again (2/2): "))
		);
		assert_eq!(quiet.apply(&JobEvent::Progress(0.3)), None);

		assert!(quiet.finish().is_empty());
		assert_eq!(verbose.finish(), vec!["bell\u{7} in the title".to_owned()]);
	}

	#[test]
	fn warnings_shown_live_are_left_out_of_the_summary() {
		let encoded = JobOutcome::Encoded {
			output: PathBuf::from("a.cry.mkv"),
			src_bytes: 2_000,
			out_bytes: 1_000,
			warnings: vec!["first".into(), "second".into()],
		};

		let all = outcome_line_with(Path::new("a.mkv"), &encoded, SHORT_TAIL, &[]);
		let rest = outcome_line_with(Path::new("a.mkv"), &encoded, SHORT_TAIL, &["first".to_owned()]);

		assert_eq!(all.lines().count(), 3, "{all}");
		assert_eq!(rest.lines().count(), 2, "{rest}");
		assert!(rest.ends_with("⚠ second"), "{rest}");
	}

	#[test]
	fn a_larger_encode_is_called_larger() {
		let source = Path::new("small.mp4");
		let kept = |src_bytes, out_bytes| {
			outcome_line(
				source,
				&JobOutcome::KeptOriginal {
					reason: KeepReason::NotSmaller { src_bytes, out_bytes },
				},
			)
		};

		assert_eq!(
			kept(214_000, 282_000),
			"= small.mp4  kept: the encode came out larger than the source (214.00 kB → 282.00 kB)"
		);
		assert_eq!(
			kept(300_000, 282_000),
			"= small.mp4  kept: encoding saved too little (300.00 kB → 282.00 kB)"
		);
		assert_eq!(
			kept(282_000, 282_000),
			"= small.mp4  kept: encoding saved too little (282.00 kB → 282.00 kB)"
		);
	}

	const STAGES: [Stage; 12] = [
		Stage::Detect,
		Stage::Probe,
		Stage::Plan,
		Stage::Queued,
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
