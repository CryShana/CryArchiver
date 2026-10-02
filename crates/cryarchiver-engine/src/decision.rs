//! What the engine decided for one file: the record a project keeps with the file and its details show. Every field
//! is optional and records are read by name, so a record written by another build still reads.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::engine::{JobEvent, Stage};
use crate::options::TrimWindow;
use crate::quality::search::Target;
use crate::quality::vmaf::Score;
use crate::video::analyze::ScanType;
use crate::video::plan::Encoder;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Decision {
	pub video: Option<VideoDecision>,
	pub image: Option<ImageDecision>,
	pub audio: Option<AudioDecision>,
	pub warnings: Vec<String>,
	/// The source's size, or under a trim the window's share of it.
	pub source_bytes: Option<u64>,
	pub output_bytes: Option<u64>,
	/// The time each stage took, in the order the stages first ran; a stage that ran again adds to its time.
	pub stages: Vec<StageTime>,
	/// The full encodes made: more than one when the quality check sent the video back.
	pub attempts: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StageTime {
	pub stage: String,
	pub seconds: f64,
}

/// A VMAF mean and 5th percentile, measured or aimed at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Vmaf {
	pub mean: f64,
	pub p5: f64,
}

/// What an encode at `crf` scored: a search probe over its samples, or a full check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Measured {
	pub crf: f64,
	pub mean: f64,
	pub p5: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrfChoice {
	/// Searched on samples of the video.
	Searched,
	/// The CRF the video's group learned, used as it is and checked.
	Learned,
	/// The CRF the video's group learned, used without a quality measurement (no libvmaf).
	LearnedUnmeasured,
	/// Searched again, starting from the CRF the video's group learned.
	LearnedSearchedAgain,
	/// Set with `--crf`; the quality was not measured.
	Fixed,
	/// The speed's table CRF, without a quality measurement.
	Table,
	/// Nothing could be sampled: encoded at the starting CRF and checked.
	Unsearched,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scan {
	Progressive,
	Interlaced,
	Telecined,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Trim {
	pub start_s: f64,
	pub length_s: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reencode {
	pub reason: String,
	pub from_crf: f64,
	pub to_crf: f64,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoDecision {
	/// The encoder and its preset, as in `SVT-AV1 preset 5`.
	pub encoder: String,
	pub crf: Option<f64>,
	pub chosen: Option<CrfChoice>,
	/// The target the full check holds the encode to.
	pub target: Option<Vmaf>,
	/// How far the target was lowered for the encode's film grain.
	pub grain_allowance: Option<Vmaf>,
	/// The target comes from the quality settings rather than the speed.
	pub custom_target: bool,
	pub probes: Vec<Measured>,
	/// The full check of each encode, in order.
	pub checks: Vec<Measured>,
	pub reencode: Option<Reencode>,
	/// The last check missed the target by at most half a point, and the encode was written anyway.
	pub near_miss: bool,
	/// The film grain level planned for the video.
	pub grain: Option<u8>,
	/// The planned film grain was left out: the search found the video smaller without it, or its group learned so.
	pub grain_dropped: bool,
	pub scan: Option<Scan>,
	pub deinterlaced: bool,
	pub resized: Option<(u32, u32)>,
	pub trim: Option<Trim>,
	pub burned_subtitles: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageDecision {
	/// The output's kind, as in `AVIF` or `JPEG XL`.
	pub route: String,
	/// The encoder setting the output was made at, as in `quality 62`.
	pub setting: Option<String>,
	/// The output's SSIMULACRA2 score.
	pub score: Option<f64>,
	pub target: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioDecision {
	pub tracks: Vec<AudioTrack>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioTrack {
	/// `opus`, or `copy` for a track kept as it is.
	pub codec: String,
	pub bitrate_kbps: Option<u32>,
	pub channels: Option<u32>,
}

impl Decision {
	/// The chosen CRF and the last full check's score: `CRF 36.75, VMAF 93.1 / 87.2`.
	pub fn crf_and_score(&self) -> Option<String> {
		let video = self.video.as_ref()?;
		let crf = video.crf?;

		Some(match video.checks.last() {
			Some(check) => format!("CRF {crf}, VMAF {:.1} / {:.1}", check.mean, check.p5),
			None => format!("CRF {crf}"),
		})
	}
}

impl From<Target> for Vmaf {
	fn from(target: Target) -> Self {
		Self {
			mean: target.mean,
			p5: target.floor,
		}
	}
}

impl Measured {
	pub(crate) fn new(crf: f64, score: Score) -> Self {
		Self {
			crf,
			mean: score.mean,
			p5: score.p5,
		}
	}
}

impl From<ScanType> for Scan {
	fn from(scan: ScanType) -> Self {
		match scan {
			ScanType::Progressive => Self::Progressive,
			ScanType::Interlaced { .. } => Self::Interlaced,
			ScanType::Telecined => Self::Telecined,
		}
	}
}

impl From<TrimWindow> for Trim {
	fn from(window: TrimWindow) -> Self {
		Self {
			start_s: window.start_s,
			length_s: window.length_s,
		}
	}
}

pub(crate) fn encoder_text(encoder: &Encoder) -> String {
	match encoder {
		Encoder::SvtAv1 { preset, .. } => format!("SVT-AV1 preset {preset}"),
		Encoder::Aom { cpu_used, .. } => format!("libaom-av1 cpu-used {cpu_used}"),
		Encoder::Vp9Alpha { cpu_used, .. } => format!("VP9 cpu-used {cpu_used}"),
	}
}

fn stage_name(stage: Stage) -> &'static str {
	match stage {
		Stage::Detect => "detect",
		Stage::Probe => "probe",
		Stage::Plan => "plan",
		Stage::Queued => "queued",
		Stage::WaitingForMemory => "waiting for memory",
		Stage::Analyze => "analyse",
		Stage::Sample => "sample",
		Stage::Encode => "encode",
		Stage::Mux => "mux",
		Stage::Metadata => "metadata",
		Stage::Verify => "verify",
		Stage::Commit => "commit",
	}
}

/// Builds a job's record from its events and from what the engine notes on the way.
#[derive(Debug, Default)]
pub(crate) struct Recorder {
	state: Mutex<Recording>,
}

#[derive(Debug, Default)]
struct Recording {
	decision: Decision,
	stage: Option<(Stage, Instant)>,
}

impl Recording {
	fn enter(&mut self, stage: Option<Stage>, now: Instant) {
		if let Some((previous, since)) = self.stage.take() {
			let name = stage_name(previous);
			let seconds = now.saturating_duration_since(since).as_secs_f64();
			match self.decision.stages.iter_mut().find(|time| time.stage == name) {
				Some(time) => time.seconds += seconds,
				None => self.decision.stages.push(StageTime {
					stage: name.to_owned(),
					seconds,
				}),
			}
		}

		self.stage = stage.map(|stage| (stage, now));
	}
}

impl Recorder {
	fn lock(&self) -> MutexGuard<'_, Recording> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}

	pub(crate) fn observe(&self, event: &JobEvent) {
		match event {
			JobEvent::Stage(stage) => self.lock().enter(Some(*stage), Instant::now()),
			JobEvent::Probed { crf, score } => self.video(|video| video.probes.push(Measured::new(*crf, *score))),
			JobEvent::Warning(warning) => self.update(|decision| decision.warnings.push(warning.clone())),
			_ => {}
		}
	}

	pub(crate) fn update(&self, change: impl FnOnce(&mut Decision)) {
		change(&mut self.lock().decision);
	}

	pub(crate) fn video(&self, change: impl FnOnce(&mut VideoDecision)) {
		self.update(|decision| change(decision.video.get_or_insert_default()));
	}

	/// The record so far, the current stage timed up to now.
	pub(crate) fn snapshot(&self) -> Decision {
		let mut state = self.lock();
		let current = state.stage.map(|(stage, _)| stage);
		state.enter(current, Instant::now());
		state.decision.clone()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_headline_shows_the_crf_and_the_last_check() {
		let mut decision = Decision {
			video: Some(VideoDecision {
				crf: Some(36.75),
				..VideoDecision::default()
			}),
			..Decision::default()
		};
		assert_eq!(decision.crf_and_score().as_deref(), Some("CRF 36.75"));

		let video = decision.video.as_mut().unwrap();
		video.checks.push(Measured {
			crf: 38.0,
			mean: 90.04,
			p5: 85.0,
		});
		video.checks.push(Measured {
			crf: 36.75,
			mean: 93.06,
			p5: 87.24,
		});
		assert_eq!(decision.crf_and_score().as_deref(), Some("CRF 36.75, VMAF 93.1 / 87.2"));
		assert_eq!(Decision::default().crf_and_score(), None);
	}

	#[test]
	fn stages_add_up_their_time_and_probes_and_warnings_are_recorded() {
		let recorder = Recorder::default();

		recorder.observe(&JobEvent::Stage(Stage::Encode));
		recorder.observe(&JobEvent::Stage(Stage::Verify));
		recorder.observe(&JobEvent::Probed {
			crf: 30.0,
			score: Score {
				mean: 94.0,
				p5: 90.0,
				frames: 10,
			},
		});
		recorder.observe(&JobEvent::Stage(Stage::Encode));
		recorder.observe(&JobEvent::Warning("a note".into()));
		let decision = recorder.snapshot();

		let names: Vec<&str> = decision.stages.iter().map(|time| time.stage.as_str()).collect();
		assert_eq!(names, ["encode", "verify"]);
		assert_eq!(decision.warnings, ["a note"]);
		assert_eq!(
			decision.video.unwrap().probes,
			[Measured {
				crf: 30.0,
				mean: 94.0,
				p5: 90.0
			}]
		);
	}

	#[test]
	fn a_record_reads_back_without_the_fields_it_lacks() {
		#[derive(Serialize)]
		struct Older {
			attempts: u32,
			unknown_later: &'static str,
		}

		let bytes = serde_json::to_vec(&Older {
			attempts: 2,
			unknown_later: "x",
		})
		.unwrap();
		let decision: Decision = serde_json::from_slice(&bytes).unwrap();

		assert_eq!(decision.attempts, 2);
		assert_eq!(decision.video, None);
	}
}
