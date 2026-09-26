use std::ffi::OsStr;
use std::fmt::Write;
use std::io::{self, Write as _};
use std::path::Path;

use cryarchiver_engine::engine::{JobEvent, Stage};
use cryarchiver_engine::outcome::{JobOutcome, KeepReason};
use cryarchiver_engine::text::{display_name, fit_middle};
use indicatif::{DecimalBytes, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

const NAME_WIDTH: usize = 40;

pub struct Report {
	multi: MultiProgress,
	style: ProgressStyle,
	quiet: bool,
}

impl Report {
	pub fn new(quiet: bool) -> Self {
		let multi = MultiProgress::new();
		if quiet {
			multi.set_draw_target(ProgressDrawTarget::hidden());
		}

		let style = ProgressStyle::with_template("{prefix} {bar:24.cyan/blue} {msg}")
			.expect("valid template")
			.progress_chars("━━─");
		Self { multi, style, quiet }
	}

	pub fn bar(&self, source: &Path) -> ProgressBar {
		let bar = self.multi.add(ProgressBar::new(1000).with_style(self.style.clone()));
		bar.set_prefix(fit_middle(&file_label(source), NAME_WIDTH));
		bar.set_message(stage_label(Stage::Detect));
		bar
	}

	pub fn print(&self, line: &str, failed: bool) {
		if !self.quiet || failed {
			self.multi.suspend(|| {
				let _ = writeln!(io::stdout(), "{line}");
			});
		}
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

pub fn apply(bar: &ProgressBar, event: &JobEvent) {
	match event {
		JobEvent::Stage(stage) => bar.set_message(stage_label(*stage)),
		JobEvent::Progress(fraction) => bar.set_position(permille(*fraction)),
		JobEvent::Warning(_) => {}
	}
}

pub fn outcome_line(source: &Path, outcome: &JobOutcome) -> String {
	let name = file_label(source);

	match outcome {
		JobOutcome::Encoded {
			output,
			src_bytes,
			out_bytes,
			warnings,
		} => {
			let saved = 100.0 - percent(*out_bytes, *src_bytes);
			let mut line = format!(
				"✓ {name} → {}  {} → {} (−{saved:.0} %)",
				file_label(output),
				DecimalBytes(*src_bytes),
				DecimalBytes(*out_bytes)
			);
			for warning in warnings {
				let _ = write!(line, "\n    ⚠ {}", display_name(OsStr::new(warning)));
			}
			line
		}
		JobOutcome::KeptOriginal { reason } => format!("= {name}  kept: {}", keep_reason(reason)),
		JobOutcome::Skipped { reason } => format!("· {name}  skipped: {reason}"),
		JobOutcome::Failed(err) => {
			let mut line = format!("✗ {name}  failed: {err}");
			for log in &err.log_tail[err.log_tail.len().saturating_sub(3)..] {
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
		KeepReason::NotSmaller { src_bytes, out_bytes } => {
			format!(
				"encoding saved too little ({} → {})",
				DecimalBytes(*src_bytes),
				DecimalBytes(*out_bytes)
			)
		}
		KeepReason::AlreadyEfficient(why) | KeepReason::Unsupported(why) => why.clone(),
	}
}

fn stage_label(stage: Stage) -> &'static str {
	match stage {
		Stage::Detect => "detecting",
		Stage::Probe => "probing",
		Stage::Plan => "planning",
		Stage::Queued => "queued",
		Stage::Analyze => "analysing",
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

	use cryarchiver_engine::outcome::{ErrorKind, JobError};

	use super::*;

	#[test]
	fn describes_each_outcome_on_one_line() {
		let source = Path::new("music/02. 試験曲.flac");
		let encoded = JobOutcome::Encoded {
			output: PathBuf::from("music/02. 試験曲.cry.opus"),
			src_bytes: 20_000_000,
			out_bytes: 2_000_000,
			warnings: Vec::new(),
		};
		let kept = JobOutcome::KeptOriginal {
			reason: KeepReason::AlreadyEfficient("already Opus".into()),
		};
		let failed = JobOutcome::Failed(JobError::from(ErrorKind::SourceDamaged(
			"decodes to 4.0 s but declares 10.0 s".into(),
		)));

		assert_eq!(
			outcome_line(source, &encoded),
			"✓ 02. 試験曲.flac → 02. 試験曲.cry.opus  20.00 MB → 2.00 MB (−90 %)"
		);
		assert_eq!(outcome_line(source, &kept), "= 02. 試験曲.flac  kept: already Opus");
		assert!(outcome_line(source, &failed).starts_with("✗ 02. 試験曲.flac  failed: source damaged"));
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
	fn every_stage_has_a_label() {
		for stage in [
			Stage::Detect,
			Stage::Probe,
			Stage::Plan,
			Stage::Queued,
			Stage::Analyze,
			Stage::Encode,
			Stage::Mux,
			Stage::Metadata,
			Stage::Verify,
			Stage::Commit,
		] {
			assert!(!stage_label(stage).is_empty());
		}
	}
}
