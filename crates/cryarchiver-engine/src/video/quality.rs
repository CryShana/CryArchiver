use std::fs;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::outcome::{ErrorKind, JobError, KeepReason};
use crate::preset::Speed;
use crate::quality::memory::GroupStats;
use crate::quality::search::{self as crf_search, Bounds, Choice, Next, Probe, Target};
use crate::quality::vmaf::{self, FrameWindow, Reference, Score, Scoring};
use crate::toolchain::Toolchain;
use crate::video::analyze::{Analysis, DEFAULT_SEGMENTS, Detail};
use crate::video::encode::{Sample, encode_sample};
use crate::video::plan::{Encoder, VideoPlan};
use crate::workspace::Workspace;

pub const MAX_RETRIES: u32 = 1;

const TARGETS: [Target; 3] = [
	Target {
		mean: 95.0,
		floor: 91.0,
	},
	Target {
		mean: 93.0,
		floor: 89.0,
	},
	Target {
		mean: 93.0,
		floor: 88.0,
	},
];
const GRAIN_ALLOWANCE: Target = Target { mean: 2.0, floor: 2.5 };
const SLOW_ANALYSIS_SEGMENTS: usize = 8;
const SEARCH_SEGMENT_S: f64 = 4.0;
const WHOLE_FILE_BELOW_S: f64 = 12.0;
const VERIFY_WINDOW_S: f64 = 5.0;
const EDGE_SKIP: f64 = 0.05;
const FALLBACK_FPS: f64 = 25.0;
const VERIFY_LOG: &str = "vmaf-verify.json";
const SVT_BOUNDS: Bounds = Bounds {
	min: 10.0,
	max: 55.0,
	step: 0.25,
};
const WHOLE_STEP_BOUNDS: Bounds = Bounds {
	min: 10.0,
	max: 55.0,
	step: 1.0,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Strategy {
	Search { start: f64 },
	Fixed { crf: f64, verify: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Found {
	pub crf: f64,
	pub score: Score,
	pub sample_bytes: u64,
	pub slope: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Searched {
	Found(Found),
	Unreachable { crf: f64, score: Score },
}

pub struct Scorer<'a> {
	pub tools: &'a Toolchain,
	pub source: &'a Path,
	pub workspace: &'a Workspace,
	pub plan: &'a VideoPlan,
	pub threads: usize,
	pub cancel: &'a CancellationToken,
}

pub fn target(speed: Speed, film_grain: bool) -> Target {
	let base = match speed {
		Speed::Slow => TARGETS[0],
		Speed::Moderate => TARGETS[1],
		Speed::Fast => TARGETS[2],
	};
	if !film_grain {
		return base;
	}

	Target {
		mean: base.mean - GRAIN_ALLOWANCE.mean,
		floor: base.floor - GRAIN_ALLOWANCE.floor,
	}
}

pub fn analysis_segments(speed: Speed) -> usize {
	match speed {
		Speed::Slow => SLOW_ANALYSIS_SEGMENTS,
		Speed::Moderate | Speed::Fast => DEFAULT_SEGMENTS,
	}
}

pub fn bounds(encoder: &Encoder) -> Bounds {
	match encoder {
		Encoder::SvtAv1 { .. } => SVT_BOUNDS,
		Encoder::Aom { .. } | Encoder::Vp9Alpha { .. } => WHOLE_STEP_BOUNDS,
	}
}

pub fn strategy(speed: Speed, stats: Option<&GroupStats>, table_crf: f64, vmaf: bool) -> Strategy {
	let learned = stats.map(|stats| stats.setting);

	match speed {
		Speed::Fast => Strategy::Fixed {
			crf: table_crf,
			verify: false,
		},
		Speed::Moderate | Speed::Slow if !vmaf => Strategy::Fixed {
			crf: learned.unwrap_or(table_crf),
			verify: false,
		},
		Speed::Moderate => match learned {
			Some(crf) => Strategy::Fixed { crf, verify: true },
			None => Strategy::Search { start: table_crf },
		},
		Speed::Slow => Strategy::Search {
			start: learned.unwrap_or(table_crf),
		},
	}
}

pub fn search_samples(analysis: &Analysis, declared_s: Option<f64>, speed: Speed) -> Vec<Sample> {
	let count = match speed {
		Speed::Slow => 6,
		Speed::Moderate => 3,
		Speed::Fast => 0,
	};
	let Some(duration) = declared_s.filter(|duration| *duration > 0.0 && count > 0) else {
		return Vec::new();
	};

	if duration < WHOLE_FILE_BELOW_S {
		return vec![Sample {
			start_s: 0.0,
			length_s: duration,
		}];
	}

	let any_usable = analysis.detail.iter().any(|detail| detail.usable);
	let mut ranked: Vec<&Detail> = analysis
		.detail
		.iter()
		.filter(|detail| detail.usable || !any_usable)
		.collect();
	ranked.sort_by(|a, b| b.si.unwrap_or(0.0).total_cmp(&a.si.unwrap_or(0.0)));

	let mut samples: Vec<Sample> = ranked
		.into_iter()
		.take(count)
		.map(|detail| Sample {
			start_s: detail.start_s,
			length_s: SEARCH_SEGMENT_S.min(duration - detail.start_s),
		})
		.collect();
	samples.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
	samples
}

#[allow(
	clippy::cast_precision_loss,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]
pub fn verify_windows(frames: u64, decoded_s: f64, speed: Speed) -> Vec<FrameWindow> {
	let count: u64 = match speed {
		Speed::Slow => 5,
		Speed::Moderate => 3,
		Speed::Fast => 0,
	};
	if count == 0 || frames == 0 {
		return Vec::new();
	}

	let fps = if decoded_s > 0.0 {
		frames as f64 / decoded_s
	} else {
		FALLBACK_FPS
	};
	let length = ((VERIFY_WINDOW_S * fps).round() as u64).max(1);
	let edge = (frames as f64 * EDGE_SKIP) as u64;
	let span = frames - 2 * edge;
	if length * count >= span {
		return vec![FrameWindow {
			first: 0,
			last: frames - 1,
		}];
	}

	let stride = (span - length) / (count - 1);
	(0..count)
		.map(|index| {
			let first = edge + index * stride;
			FrameWindow {
				first,
				last: first + length - 1,
			}
		})
		.collect()
}

pub fn unreachable_reason(crf: f64, score: Score, target: Target) -> KeepReason {
	KeepReason::QualityUnreachable(format!(
		"quality target not reached at a smaller size (VMAF {:.1} / {:.1} at CRF {crf}, target {:.1} / {:.1})",
		score.mean, score.p5, target.mean, target.floor
	))
}

impl Scorer<'_> {
	pub async fn probe(&self, crf: f64, samples: &[Sample], round: usize) -> Result<(Score, u64), JobError> {
		let plan = VideoPlan {
			encoder: self.plan.encoder.with_crf(crf),
			..self.plan.clone()
		};
		let mut scores = Vec::new();
		let mut bytes = 0;

		for (index, sample) in samples.iter().enumerate() {
			let output = self.workspace.file(&format!("probe{round}-{index}.mkv"));
			bytes += encode_sample(&self.tools.ffmpeg, self.source, &output, &plan, sample, self.cancel).await?;

			let log = format!("vmaf-probe{round}-{index}.json");
			let scoring = self.scoring(&log);
			let cmd = vmaf::sample_command(
				&self.tools.ffmpeg,
				&output,
				&self.reference(),
				sample.start_s,
				sample.length_s,
				&scoring,
			);
			scores.extend(vmaf::frames(&cmd, &scoring, |_| {}, self.cancel).await?);
			let _ = fs::remove_file(&output);
		}

		Ok((pooled(&scores)?, bytes))
	}

	#[allow(clippy::cast_precision_loss)]
	pub async fn verify(
		&self,
		temp: &Path,
		windows: &[FrameWindow],
		total_frames: u64,
		mut on_fraction: impl FnMut(f64),
	) -> Result<Score, JobError> {
		let scoring = self.scoring(VERIFY_LOG);
		let cmd = vmaf::window_command(&self.tools.ffmpeg, temp, &self.reference(), windows, &scoring);

		let scores = vmaf::frames(
			&cmd,
			&scoring,
			|progress| {
				if let Some(frame) = progress.frame.filter(|_| total_frames > 0) {
					on_fraction((frame as f64 / total_frames as f64).clamp(0.0, 1.0));
				}
			},
			self.cancel,
		)
		.await?;

		pooled(&scores)
	}

	fn reference(&self) -> Reference<'_> {
		Reference {
			source: self.source,
			stream: self.plan.streams[0],
			decoder: self.plan.decoder,
			filters: &self.plan.filters,
		}
	}

	fn scoring<'b>(&'b self, log: &'b str) -> Scoring<'b> {
		Scoring {
			model: vmaf::model_for(self.plan.width, self.plan.height),
			threads: self.threads,
			dir: self.workspace.dir(),
			log,
		}
	}
}

pub async fn search(
	scorer: &Scorer<'_>,
	samples: &[Sample],
	start: f64,
	target: Target,
	prior_slope: f64,
	mut on_round: impl FnMut(usize),
) -> Result<Searched, JobError> {
	let bounds = bounds(&scorer.plan.encoder);
	let mut probes: Vec<Probe> = Vec::new();
	let mut sample_bytes: Vec<u64> = Vec::new();
	let mut crf = bounds.quantize(start);

	loop {
		on_round(probes.len());
		let (score, bytes) = scorer.probe(crf, samples, probes.len()).await?;
		probes.push(Probe { crf, score });
		sample_bytes.push(bytes);

		match crf_search::next(&probes, target, bounds, prior_slope) {
			Next::Probe(next) => crf = next,
			Next::Done(Choice::Chosen { index, slope }) => {
				return Ok(Searched::Found(Found {
					crf: probes[index].crf,
					score: probes[index].score,
					sample_bytes: sample_bytes[index],
					slope,
				}));
			}
			Next::Done(Choice::Unreachable { index }) => {
				return Ok(Searched::Unreachable {
					crf: probes[index].crf,
					score: probes[index].score,
				});
			}
		}
	}
}

fn pooled(scores: &[f64]) -> Result<Score, JobError> {
	vmaf::pool(scores).ok_or_else(|| ErrorKind::EncodeFailed("the quality check scored no frames".into()).into())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::video::analyze::{NoiseClass, ScanType};

	fn detail(start_s: f64, si: f64, usable: bool) -> Detail {
		Detail {
			start_s,
			length_s: 3.0,
			si: Some(si),
			usable,
		}
	}

	#[test]
	fn targets_follow_the_speed_and_allow_for_film_grain() {
		assert_eq!(
			target(Speed::Slow, false),
			Target {
				mean: 95.0,
				floor: 91.0
			}
		);
		assert_eq!(
			target(Speed::Slow, true),
			Target {
				mean: 93.0,
				floor: 88.5
			}
		);
		assert_eq!(
			target(Speed::Moderate, false),
			Target {
				mean: 93.0,
				floor: 89.0
			}
		);
		assert_eq!(
			target(Speed::Moderate, true),
			Target {
				mean: 91.0,
				floor: 86.5
			}
		);
		assert_eq!(
			target(Speed::Fast, false),
			Target {
				mean: 93.0,
				floor: 88.0
			}
		);
	}

	#[test]
	fn only_moderate_and_slow_measure_and_moderate_trusts_a_learned_crf() {
		let learned = GroupStats {
			setting: 31.5,
			slope: Some(0.4),
			bpp: None,
			observations: 3,
		};

		assert_eq!(
			strategy(Speed::Fast, Some(&learned), 35.0, true),
			Strategy::Fixed {
				crf: 35.0,
				verify: false
			}
		);
		assert_eq!(
			strategy(Speed::Moderate, None, 29.0, true),
			Strategy::Search { start: 29.0 }
		);
		assert_eq!(
			strategy(Speed::Moderate, Some(&learned), 29.0, true),
			Strategy::Fixed {
				crf: 31.5,
				verify: true
			}
		);
		assert_eq!(
			strategy(Speed::Slow, Some(&learned), 25.0, true),
			Strategy::Search { start: 31.5 }
		);
		assert_eq!(
			strategy(Speed::Slow, None, 25.0, false),
			Strategy::Fixed {
				crf: 25.0,
				verify: false
			}
		);
	}

	#[test]
	fn samples_are_the_most_detailed_usable_segments_in_time_order() {
		let analysis = Analysis {
			scan: ScanType::Progressive,
			noise: NoiseClass::Clean,
			detail: vec![
				detail(5.0, 40.0, true),
				detail(35.0, 90.0, true),
				detail(65.0, 70.0, true),
				detail(95.0, 200.0, false),
			],
		};
		let four = |start_s: f64| Sample { start_s, length_s: 4.0 };

		assert_eq!(
			search_samples(&analysis, Some(100.0), Speed::Moderate),
			vec![four(5.0), four(35.0), four(65.0)]
		);
		assert_eq!(
			search_samples(&analysis, Some(100.0), Speed::Slow),
			vec![four(5.0), four(35.0), four(65.0)]
		);
		assert!(search_samples(&analysis, Some(100.0), Speed::Fast).is_empty());
		assert!(search_samples(&analysis, None, Speed::Moderate).is_empty());
		assert_eq!(
			search_samples(&analysis, Some(8.5), Speed::Slow),
			vec![Sample {
				start_s: 0.0,
				length_s: 8.5
			}]
		);
	}

	#[test]
	fn low_detail_segments_are_sampled_only_when_no_segment_is_usable() {
		let analysis = |detail: Vec<Detail>| Analysis {
			scan: ScanType::Progressive,
			noise: NoiseClass::Clean,
			detail,
		};
		let four = |start_s: f64| Sample { start_s, length_s: 4.0 };
		let one_usable = analysis(vec![
			detail(5.0, 0.0, false),
			detail(35.0, 60.0, true),
			detail(65.0, 12.0, false),
			detail(95.0, 14.0, false),
		]);
		let none_usable = analysis(vec![
			detail(5.0, 0.0, false),
			detail(35.0, 9.0, false),
			detail(65.0, 12.0, false),
			detail(95.0, 14.0, false),
		]);

		assert_eq!(
			search_samples(&one_usable, Some(100.0), Speed::Moderate),
			vec![four(35.0)]
		);
		assert_eq!(search_samples(&one_usable, Some(100.0), Speed::Slow), vec![four(35.0)]);
		assert_eq!(
			search_samples(&none_usable, Some(100.0), Speed::Moderate),
			vec![four(35.0), four(65.0), four(95.0)]
		);
		assert_eq!(
			search_samples(&none_usable, Some(100.0), Speed::Slow),
			vec![four(5.0), four(35.0), four(65.0), four(95.0)]
		);
	}

	#[test]
	fn windows_skip_the_edges_and_short_outputs_are_scored_whole() {
		assert_eq!(
			verify_windows(1500, 60.0, Speed::Moderate),
			vec![
				FrameWindow { first: 75, last: 199 },
				FrameWindow { first: 687, last: 811 },
				FrameWindow {
					first: 1299,
					last: 1423
				},
			]
		);
		assert_eq!(verify_windows(1500, 60.0, Speed::Slow).len(), 5);
		assert_eq!(
			verify_windows(250, 10.0, Speed::Moderate),
			vec![FrameWindow { first: 0, last: 249 }]
		);
		assert!(verify_windows(1500, 60.0, Speed::Fast).is_empty());
	}

	#[test]
	fn whole_step_encoders_search_in_whole_steps() {
		assert!((bounds(&Encoder::Vp9Alpha { cpu_used: 2, crf: 33 }).step - 1.0).abs() < 1e-9);
		assert!((bounds(&Encoder::Aom { cpu_used: 4, crf: 30 }).step - 1.0).abs() < 1e-9);
		assert!(
			(bounds(&Encoder::SvtAv1 {
				preset: 5,
				crf: 29.0,
				film_grain: false
			})
			.step - 0.25)
				.abs() < 1e-9
		);
	}

	#[test]
	fn unreachable_searches_keep_the_original_with_the_scores() {
		let reason = unreachable_reason(
			10.0,
			Score {
				mean: 88.26,
				p5: 80.04,
				frames: 300,
			},
			Target {
				mean: 93.0,
				floor: 89.0,
			},
		);

		assert_eq!(
			reason,
			KeepReason::QualityUnreachable(
				"quality target not reached at a smaller size (VMAF 88.3 / 80.0 at CRF 10, target 93.0 / 89.0)".into()
			)
		);
	}
}
