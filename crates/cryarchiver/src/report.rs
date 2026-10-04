use std::ffi::OsStr;
use std::fmt::Write;
use std::io::{self, Write as _};
use std::path::Path;

use cryarchiver_engine::outcome::{JobOutcome, KeepReason};
use cryarchiver_engine::text::display_name;
use indicatif::{DecimalBytes, MultiProgress, ProgressDrawTarget};

use crate::progress::Item;

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
		Item::new(&self.multi, source, self.quiet, self.verbose, self.colour)
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

/// The outcome's line, leaving out the warnings in `shown`, which were printed as they happened.
pub fn outcome_line_with(source: &Path, outcome: &JobOutcome, tail_lines: usize, shown: &[String]) -> String {
	let name = file_label(source);

	match outcome {
		JobOutcome::Encoded {
			output,
			src_bytes,
			out_bytes,
			warnings,
			decision,
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
			if let Some(chosen) = decision.crf_and_score() {
				let _ = write!(line, ", {chosen}");
			}
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
		} if predicted_bytes > src_bytes => {
			format!(
				"encoding would come out larger than the source ({} → predicted {})",
				DecimalBytes(*src_bytes),
				DecimalBytes(*predicted_bytes)
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

	use cryarchiver_engine::decision::{Decision, Measured, VideoDecision};
	use cryarchiver_engine::outcome::{ErrorKind, JobError};

	use super::*;

	fn outcome_line(source: &Path, outcome: &JobOutcome) -> String {
		outcome_line_with(source, outcome, SHORT_TAIL, &[])
	}

	#[test]
	fn describes_each_outcome_on_one_line() {
		let source = Path::new("music/02. 夜明け.flac");
		let encoded = JobOutcome::Encoded {
			output: PathBuf::from("music/02. 夜明け.cry.opus"),
			src_bytes: 20_000_000,
			out_bytes: 2_000_000,
			warnings: Vec::new(),
			decision: Box::default(),
		};
		let kept = JobOutcome::KeptOriginal {
			reason: KeepReason::AlreadyEfficient("already Opus".into()),
		};
		let predicted = JobOutcome::KeptOriginal {
			reason: KeepReason::PredictedNotSmaller {
				src_bytes: 5_800_000,
				predicted_bytes: 5_500_000,
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
					decision: Box::default(),
				}
			),
			"✓ 02. 夜明け.flac → 02. 夜明け.cry.opus  6.43 MB → 32.09 MB (+399 %)"
		);
		assert_eq!(outcome_line(source, &kept), "= 02. 夜明け.flac  kept: already Opus");
		assert_eq!(
			outcome_line(source, &predicted),
			"= 02. 夜明け.flac  kept: encoding would save too little (5.80 MB → predicted 5.50 MB)"
		);
		assert!(outcome_line(source, &failed).starts_with("✗ 02. 夜明け.flac  failed: source damaged"));
	}

	#[test]
	fn a_predicted_larger_encode_is_called_larger() {
		let kept = |src_bytes, predicted_bytes| {
			outcome_line(
				Path::new("small.mp4"),
				&JobOutcome::KeptOriginal {
					reason: KeepReason::PredictedNotSmaller {
						src_bytes,
						predicted_bytes,
					},
				},
			)
		};

		assert_eq!(
			kept(5_800_000, 7_100_000),
			"= small.mp4  kept: encoding would come out larger than the source (5.80 MB → predicted 7.10 MB)"
		);
		assert_eq!(
			kept(5_800_000, 5_800_000),
			"= small.mp4  kept: encoding would save too little (5.80 MB → predicted 5.80 MB)"
		);
	}

	#[test]
	fn a_video_line_ends_with_the_chosen_crf_and_the_full_check() {
		let decision = Decision {
			video: Some(VideoDecision {
				crf: Some(36.75),
				checks: vec![Measured {
					crf: 36.75,
					mean: 93.12,
					p5: 87.21,
				}],
				..VideoDecision::default()
			}),
			..Decision::default()
		};
		let encoded = JobOutcome::Encoded {
			output: PathBuf::from("clip.cry.mkv"),
			src_bytes: 1_750_000_000,
			out_bytes: 152_000_000,
			warnings: vec!["a note".into()],
			decision: Box::new(decision),
		};

		assert_eq!(
			outcome_line(Path::new("clip.mp4"), &encoded),
			"✓ clip.mp4 → clip.cry.mkv  1.75 GB → 152.00 MB (−91 %), CRF 36.75, VMAF 93.1 / 87.2\n    ⚠ a note"
		);
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
			decision: Box::default(),
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
	fn warnings_shown_live_are_left_out_of_the_summary() {
		let encoded = JobOutcome::Encoded {
			output: PathBuf::from("a.cry.mkv"),
			src_bytes: 2_000,
			out_bytes: 1_000,
			warnings: vec!["first".into(), "second".into()],
			decision: Box::default(),
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
}
