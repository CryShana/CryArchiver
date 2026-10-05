//! The read model a front end mirrors (project layer spec §11.3). The project task sends [`Update`]s; a front end
//! folds them into its own view in its refresh step and never waits on the project. Every `String` here is
//! display-safe, escaped as `cryarchiver_engine::text::display_name` does.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::time::Duration;

use cryarchiver_engine::decision::{AudioTrack, CrfChoice, Decision, Scan, VideoDecision};
use cryarchiver_engine::text::display_name;

use crate::issues::{IssueId, Problem, Verbs};
use crate::schedule::Work;
use crate::settings::Settings;
use crate::tree::{FileId, FolderId, FolderStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
	/// Not started yet, or paused by the user: nothing new starts.
	Paused,
	Running,
	PausedLowDisk,
	/// Everything is archived and watch mode waits for new files.
	Watching,
	Stopping,
	/// Everything is archived and the completion check ran.
	Finished,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
	pub run_state: RunState,
	/// Part of the performance limit waits for running jobs; restarting them applies it now.
	pub limit_pending: bool,
	pub scan_complete: bool,
	pub deleting_originals: bool,
	/// The project deletes originals, but this session hasn't confirmed it yet: nothing is deleted until it does.
	pub deleting_paused: bool,
}

/// The state a file row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowState {
	Done,
	Encoding,
	Verifying,
	Queued,
	KeptOriginal,
	Copied,
	Duplicate,
	/// A retry delay, a canonical copy, the files a reference names, or the dedup hashes.
	Waiting,
	SourceDamaged,
	Failed,
	Warning,
	SourceGone,
	DeletePending,
	OriginalDeleted,
	Excluded,
}

impl RowState {
	pub fn is_active(self) -> bool {
		matches!(self, Self::Encoding | Self::Verifying)
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileStatus {
	pub state: RowState,
	pub progress: Option<f64>,
	pub codec: Option<String>,
	pub bytes_in: u64,
	pub bytes_out: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileRow {
	pub id: FileId,
	pub name: String,
	pub status: FileStatus,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Parity {
	pub source_files: u64,
	pub archive_files: u64,
	pub excluded: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Overall {
	pub files_done: u64,
	pub files_active: u64,
	pub files_queued: u64,
	pub files_total: u64,
	pub scan_complete: bool,
	pub progress: f64,
	pub kept: u64,
	pub duplicates: u64,
	pub open_issues: u64,
	pub saved_bytes: u64,
	pub saved_fraction: f64,
	pub eta: Option<Duration>,
	pub parity: Option<Parity>,
	pub originals_deleted: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Worker {
	pub file: FileId,
	pub folder: FolderId,
	pub work: Work,
	pub name: String,
	pub state: RowState,
	/// The engine's current stage, in words.
	pub stage: Option<String>,
	pub fraction: f64,
	/// The pending performance limit wouldn't let this job run; restarting cancels it.
	pub over_limit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemView {
	pub cpu_percent: u8,
	pub ram_used: u64,
	pub ram_total: u64,
	pub disk_free: u64,
	pub low_priority: bool,
	pub toolchain: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueView {
	pub id: IssueId,
	pub problem: Problem,
	pub file: Option<FileId>,
	pub folder: Option<FolderId>,
	pub name: String,
	/// Relative to the source root, or to the destination root for files the project didn't write.
	pub location: String,
	pub reason: String,
	pub resolution: String,
	pub verbs: Verbs,
	/// Attempts with a failure log, numbered from 1.
	pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterEntry {
	Folder(FolderId),
	File { folder: FolderId, row: FileRow },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FilterView {
	pub query: String,
	/// Matches and their ancestors in display order, each with its depth below the root.
	pub rows: Vec<(u16, FilterEntry)>,
	pub matching_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathLink {
	pub file: FileId,
	pub folder: FolderId,
	pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Duplicates {
	None,
	PlaceholderOf(PathLink),
	CanonicalOf(Vec<PathLink>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Details {
	pub source_path: String,
	pub destination_path: String,
	pub source_info: String,
	pub state: RowState,
	pub stage: Option<String>,
	pub fraction: Option<f64>,
	pub bytes_in: u64,
	pub bytes_out: Option<u64>,
	pub plan: Vec<(String, String)>,
	/// What the engine decided for the file, as the project file keeps it.
	pub decision: Vec<DecisionLine>,
	/// What the project noted about the file this session, oldest first.
	pub notes: Vec<String>,
	pub duplicates: Duplicates,
	pub issue: Option<IssueView>,
}

/// A line of a file's decision record. A marked line stands out: the video was encoded again, or written slightly
/// below its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionLine {
	pub label: String,
	pub value: String,
	pub marked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
	Row(RowState),
	Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
	/// Seconds since midnight UTC.
	pub time: u32,
	pub kind: LogKind,
	pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NoticeLevel {
	Info,
	Warning,
	Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
	pub level: NoticeLevel,
	pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
	Status(Status),
	Settings(Settings),
	FolderAdded {
		id: FolderId,
		parent: FolderId,
		name: String,
	},
	Folder(FolderId, FolderStats),
	/// The answer to watching a folder: its rows, in order.
	Files {
		folder: FolderId,
		rows: Vec<FileRow>,
	},
	FileAdded {
		folder: FolderId,
		row: FileRow,
	},
	FileRemoved {
		folder: FolderId,
		file: FileId,
	},
	/// A status change of a file in a watched folder or in the current filter result.
	FileStatus {
		folder: FolderId,
		file: FileId,
		status: FileStatus,
	},
	Overall(Overall),
	Workers(Vec<Worker>),
	Issues(Vec<IssueView>),
	Filter(Option<FilterView>),
	Details(FileId, Box<Details>),
	IssueLog {
		issue: IssueId,
		attempt: u32,
		lines: Vec<String>,
	},
	Log(LogEntry),
	Notice(Notice),
	/// The answer to `Command::ShowPath`: the full source path.
	Path(String),
	System(SystemView),
	/// The project task has stopped; nothing follows.
	Stopped,
}

/// A size in decimal units, as people read them: `950 B`, `12.3 MB`, `1.24 GB`.
pub fn size_text(bytes: u64) -> String {
	const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];

	#[allow(clippy::cast_precision_loss)]
	let mut value = bytes as f64;
	let mut unit = 0;
	while value >= 1000.0 && unit < UNITS.len() - 1 {
		value /= 1000.0;
		unit += 1;
	}

	if unit == 0 || value >= 100.0 {
		format!("{value:.0} {}", UNITS[unit])
	} else if value >= 10.0 {
		format!("{value:.1} {}", UNITS[unit])
	} else {
		format!("{value:.2} {}", UNITS[unit])
	}
}

/// The label of a decision line that holds one of the engine's warnings, as `display_name` shows it.
pub const WARNING_LABEL: &str = "Warning";

/// A file's decision record in the words its details show it with.
pub fn decision_lines(decision: &Decision) -> Vec<DecisionLine> {
	let mut lines = Vec::new();

	if let Some(video) = &decision.video {
		video_lines(video, &mut lines);
	}

	if let Some(image) = &decision.image {
		let mut encoder = image.route.clone();
		if let Some(setting) = &image.setting {
			let _ = write!(encoder, ", {setting}");
		}
		lines.push(plain("Encoder", encoder));
		if let Some(score) = image.score {
			let target = image.target.map_or_else(String::new, |target| format!(" vs {target}"));
			lines.push(plain("Check", format!("SSIMULACRA2 {score:.1}{target}")));
		}
	}

	if let Some(audio) = &decision.audio
		&& !audio.tracks.is_empty()
	{
		let tracks: Vec<String> = audio.tracks.iter().map(track_text).collect();
		lines.push(plain("Audio", tracks.join(" · ")));
	}

	let took: Vec<String> = decision
		.stages
		.iter()
		.filter(|time| time.seconds >= 1.0)
		.map(|time| format!("{} {}", time.stage, seconds_text(time.seconds)))
		.collect();
	if !took.is_empty() {
		lines.push(plain("Took", took.join(", ")));
	}

	for warning in &decision.warnings {
		lines.push(plain(WARNING_LABEL, display_name(OsStr::new(warning))));
	}

	lines
}

fn video_lines(video: &VideoDecision, lines: &mut Vec<DecisionLine>) {
	if !video.encoder.is_empty() {
		let grain = match video.grain {
			Some(level) if video.grain_dropped => format!(" · film grain {level} left out"),
			Some(level) => format!(" · film grain {level}"),
			None => String::new(),
		};
		lines.push(plain("Encoder", format!("{}{grain}", video.encoder)));
	}

	if let Some(crf) = video.crf {
		let (chosen, other) = video.chosen_probes();
		let mut probes = match chosen {
			1 => "1 probe".to_owned(),
			count => format!("{count} probes"),
		};
		if other > 0 {
			let pass = if video.grain_dropped { "with" } else { "without" };
			let _ = write!(probes, "; {other} more {pass} film grain");
		}
		let mut how = match video.chosen {
			Some(CrfChoice::Searched) => format!("searched, {probes}"),
			Some(CrfChoice::LearnedSearchedAgain) => format!("learned, searched again, {probes}"),
			Some(CrfChoice::Learned) => "learned".to_owned(),
			Some(CrfChoice::LearnedUnmeasured) => "learned, not measured".to_owned(),
			Some(CrfChoice::Fixed) => "set with --crf, not measured".to_owned(),
			Some(CrfChoice::Table) => "the speed's default, not measured".to_owned(),
			Some(CrfChoice::Unsearched) => "nothing to sample, checked".to_owned(),
			None => String::new(),
		};
		if video.custom_target {
			how.push_str(if how.is_empty() {
				"custom target"
			} else {
				", custom target"
			});
		}

		let how = if how.is_empty() { how } else { format!(" ({how})") };
		lines.push(plain("CRF", format!("{crf}{how}")));
	}

	if let Some(check) = video.checks.last() {
		let mut text = format!("VMAF {:.1} / {:.1}", check.mean, check.p5);
		if let Some(target) = video.target {
			let _ = write!(text, " vs {} / {}", target.mean, target.p5);
		}
		if let Some(allowance) = video.grain_allowance {
			let _ = write!(text, ", grain allowance {} / {}", allowance.mean, allowance.p5);
		}
		lines.push(plain("Check", text));
	}

	lines.extend(again_line(video));

	if video.near_miss {
		lines.push(DecisionLine {
			label: "Target".into(),
			value: "near miss: written slightly below the target".into(),
			marked: true,
		});
	}

	let mut picture = Vec::new();
	match video.scan {
		Some(Scan::Interlaced) => picture.push("interlaced, deinterlaced".to_owned()),
		Some(Scan::Telecined) => picture.push("telecined, film frames restored".to_owned()),
		Some(Scan::Progressive) | None => {}
	}
	if let Some((width, height)) = video.resized {
		picture.push(format!("resized to {width}×{height}"));
	}
	if let Some(trim) = video.trim {
		picture.push(format!(
			"trimmed to {}–{}",
			clock_text(trim.start_s),
			clock_text(trim.start_s + trim.length_s)
		));
	}
	if video.burned_subtitles {
		picture.push("subtitles burned in".to_owned());
	}
	if !picture.is_empty() {
		lines.push(plain("Picture", picture.join(" · ")));
	}
}

/// The second encode: made, with what the first check measured, or skipped for its predicted size.
fn again_line(video: &VideoDecision) -> Option<DecisionLine> {
	let value = if let Some(again) = &video.reencode {
		let missed = video
			.checks
			.iter()
			.find(|check| (check.crf - again.from_crf).abs() < f64::EPSILON)
			.map_or_else(
				|| again.reason.clone(),
				|check| format!("measured {:.1} / {:.1}", check.mean, check.p5),
			);
		format!("CRF {} {missed}: encoded again at CRF {}", again.from_crf, again.to_crf)
	} else {
		let skipped = video.skipped_retry?;
		format!(
			"CRF {} missed: not encoded again at CRF {}, predicted at {}",
			skipped.from_crf,
			skipped.to_crf,
			size_text(skipped.predicted_bytes)
		)
	};

	Some(DecisionLine {
		label: "Again".into(),
		value,
		marked: true,
	})
}

fn track_text(track: &AudioTrack) -> String {
	if track.codec == "copy" {
		return "copied".to_owned();
	}

	let mut text = if track.codec == "opus" {
		"Opus".to_owned()
	} else {
		track.codec.clone()
	};
	if let Some(kbps) = track.bitrate_kbps {
		let _ = write!(text, " {kbps} kbps");
	}
	if let Some(channels) = track.channels {
		let _ = write!(text, ", {channels} ch");
	}
	text
}

fn plain(label: &str, value: String) -> DecisionLine {
	DecisionLine {
		label: label.to_owned(),
		value,
		marked: false,
	}
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn seconds_text(seconds: f64) -> String {
	let whole = seconds.max(0.0).round() as u64;
	match whole {
		0..60 => format!("{whole}s"),
		60..3600 => format!("{}m {}s", whole / 60, whole % 60),
		_ => format!("{}h {}m", whole / 3600, whole / 60 % 60),
	}
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clock_text(seconds: f64) -> String {
	let whole = seconds.max(0.0).floor() as u64;
	if whole >= 3600 {
		format!("{}:{:02}:{:02}", whole / 3600, whole / 60 % 60, whole % 60)
	} else {
		format!("{}:{:02}", whole / 60, whole % 60)
	}
}

#[cfg(test)]
mod tests {
	use cryarchiver_engine::decision::{AudioDecision, ImageDecision, Measured, Reencode, StageTime, Trim, Vmaf};

	use super::*;

	fn labelled(lines: &[DecisionLine]) -> Vec<(String, String, bool)> {
		lines
			.iter()
			.map(|line| (line.label.clone(), line.value.clone(), line.marked))
			.collect()
	}

	fn line(label: &str, value: &str, marked: bool) -> (String, String, bool) {
		(label.to_owned(), value.to_owned(), marked)
	}

	#[test]
	fn a_video_record_reads_as_its_crf_its_check_and_what_happened_to_it() {
		let decision = Decision {
			video: Some(VideoDecision {
				encoder: "SVT-AV1 preset 5".into(),
				crf: Some(36.75),
				chosen: Some(CrfChoice::Searched),
				target: Some(Vmaf { mean: 91.0, p5: 86.5 }),
				grain_allowance: Some(Vmaf { mean: 2.0, p5: 2.5 }),
				probes: vec![Measured::default(); 5],
				checks: vec![
					Measured {
						crf: 38.0,
						mean: 90.04,
						p5: 85.0,
						..Measured::default()
					},
					Measured {
						crf: 36.75,
						mean: 93.06,
						p5: 87.24,
						..Measured::default()
					},
				],
				reencode: Some(Reencode {
					reason: "the full check measured VMAF 90.0 / 85.0, below the target 91 / 86.5".into(),
					from_crf: 38.0,
					to_crf: 36.75,
				}),
				near_miss: true,
				grain: Some(6),
				scan: Some(Scan::Interlaced),
				deinterlaced: true,
				resized: Some((1280, 720)),
				trim: Some(Trim {
					start_s: 70.0,
					length_s: 30.5,
				}),
				..VideoDecision::default()
			}),
			stages: vec![
				StageTime {
					stage: "detect".into(),
					seconds: 0.2,
				},
				StageTime {
					stage: "encode".into(),
					seconds: 603.0,
				},
			],
			warnings: vec!["a \u{1b}[2J warning".into()],
			..Decision::default()
		};

		assert_eq!(
			labelled(&decision_lines(&decision)),
			[
				line("Encoder", "SVT-AV1 preset 5 · film grain 6", false),
				line("CRF", "36.75 (searched, 5 probes)", false),
				line("Check", "VMAF 93.1 / 87.2 vs 91 / 86.5, grain allowance 2 / 2.5", false),
				line("Again", "CRF 38 measured 90.0 / 85.0: encoded again at CRF 36.75", true),
				line("Target", "near miss: written slightly below the target", true),
				line(
					"Picture",
					"interlaced, deinterlaced · resized to 1280×720 · trimmed to 1:10–1:40",
					false
				),
				line("Took", "encode 10m 3s", false),
				line("Warning", &display_name(OsStr::new("a \u{1b}[2J warning")), false),
			]
		);
	}

	#[test]
	fn learned_fixed_and_dropped_grain_say_so() {
		let video = |chosen, custom_target, grain_dropped| Decision {
			video: Some(VideoDecision {
				encoder: "SVT-AV1 preset 5".into(),
				crf: Some(30.0),
				chosen: Some(chosen),
				custom_target,
				grain: Some(4),
				grain_dropped,
				..VideoDecision::default()
			}),
			..Decision::default()
		};

		let learned = decision_lines(&video(CrfChoice::Learned, true, true));
		assert_eq!(learned[0].value, "SVT-AV1 preset 5 · film grain 4 left out");
		assert_eq!(learned[1].value, "30 (learned, custom target)");
		assert_eq!(
			decision_lines(&video(CrfChoice::Fixed, false, false))[1].value,
			"30 (set with --crf, not measured)"
		);
		assert_eq!(decision_lines(&video(CrfChoice::Fixed, false, false)).len(), 2);
		assert_eq!(
			decision_lines(&video(CrfChoice::LearnedUnmeasured, false, false))[1].value,
			"30 (learned, not measured)"
		);

		let mut one_probe = video(CrfChoice::Searched, false, false);
		one_probe.video.as_mut().unwrap().probes.push(Measured::default());
		assert_eq!(decision_lines(&one_probe)[1].value, "30 (searched, 1 probe)");
	}

	#[test]
	fn a_retry_predicted_too_large_is_shown_as_not_encoded_again() {
		let decision = Decision {
			video: Some(VideoDecision {
				skipped_retry: Some(cryarchiver_engine::decision::SkippedRetry {
					from_crf: 35.5,
					to_crf: 29.5,
					predicted_bytes: 37_560_000,
				}),
				..VideoDecision::default()
			}),
			..Decision::default()
		};

		assert_eq!(
			labelled(&decision_lines(&decision)),
			[line(
				"Again",
				"CRF 35.5 missed: not encoded again at CRF 29.5, predicted at 37.6 MB",
				true
			)]
		);
	}

	#[test]
	fn a_search_in_two_passes_counts_the_probes_of_the_pass_it_kept() {
		let plain = Measured {
			without_grain: true,
			..Measured::default()
		};
		let two_passes = |grain_dropped| Decision {
			video: Some(VideoDecision {
				crf: Some(30.0),
				chosen: Some(CrfChoice::Searched),
				grain: Some(8),
				grain_dropped,
				probes: [vec![Measured::default(); 3], vec![plain; 2]].concat(),
				..VideoDecision::default()
			}),
			..Decision::default()
		};

		assert_eq!(
			decision_lines(&two_passes(false))[0].value,
			"30 (searched, 3 probes; 2 more without film grain)"
		);
		assert_eq!(
			decision_lines(&two_passes(true))[0].value,
			"30 (searched, 2 probes; 3 more with film grain)"
		);
	}

	#[test]
	fn image_and_audio_records_name_their_encoder_and_score() {
		let image = Decision {
			image: Some(ImageDecision {
				route: "AVIF".into(),
				setting: Some("quality 62".into()),
				score: Some(86.44),
				target: Some(85.0),
			}),
			..Decision::default()
		};
		let audio = Decision {
			audio: Some(AudioDecision {
				tracks: vec![
					AudioTrack {
						codec: "opus".into(),
						bitrate_kbps: Some(128),
						channels: Some(2),
					},
					AudioTrack {
						codec: "copy".into(),
						..AudioTrack::default()
					},
				],
			}),
			..Decision::default()
		};

		assert_eq!(
			labelled(&decision_lines(&image)),
			[
				line("Encoder", "AVIF, quality 62", false),
				line("Check", "SSIMULACRA2 86.4 vs 85", false)
			]
		);
		assert_eq!(
			labelled(&decision_lines(&audio)),
			[line("Audio", "Opus 128 kbps, 2 ch · copied", false)]
		);
		assert!(decision_lines(&Decision::default()).is_empty());
	}

	#[test]
	fn sizes_read_in_decimal_units() {
		assert_eq!(size_text(950), "950 B");
		assert_eq!(size_text(12_345_678), "12.3 MB");
		assert_eq!(size_text(1_240_000_000), "1.24 GB");
		assert_eq!(size_text(123_456_789), "123 MB");
	}
}
