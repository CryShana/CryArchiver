use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::options::TrimWindow;
use crate::outcome::{ErrorKind, JobError, KeepReason};
use crate::preset::{Overrides, Speed};
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
pub const CRF_RANGE: RangeInclusive<f64> = SVT_BOUNDS.min..=SVT_BOUNDS.max;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrainChoice {
	Grain,
	Plain,
}

pub struct Scorer<'a> {
	pub tools: &'a Toolchain,
	pub source: &'a Path,
	pub workspace: &'a Workspace,
	pub plan: &'a VideoPlan,
	pub threads: usize,
	pub cancel: &'a CancellationToken,
}

pub fn target(speed: Speed, film_grain: bool, overrides: &Overrides) -> Target {
	let custom = overrides.normalized().video.map(|video| Target {
		mean: video.mean,
		floor: video.floor,
	});
	let base = match (speed, custom) {
		(Speed::Fast, _) => TARGETS[2],
		(Speed::Slow | Speed::Moderate, Some(custom)) => custom,
		(Speed::Slow, None) => TARGETS[0],
		(Speed::Moderate, None) => TARGETS[1],
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

pub fn search_samples(analysis: &Analysis, span: Option<TrimWindow>, speed: Speed) -> Vec<Sample> {
	let count: u8 = match speed {
		Speed::Slow => 6,
		Speed::Moderate => 3,
		Speed::Fast => 0,
	};
	let Some(span) = span.filter(|span| span.length_s > 0.0 && count > 0) else {
		return Vec::new();
	};

	let whole = vec![Sample {
		start_s: span.start_s,
		length_s: span.length_s,
	}];
	if SEARCH_SEGMENT_S * f64::from(count) >= span.length_s {
		return whole;
	}

	let end_s = span.start_s + span.length_s;
	let any_usable = analysis.detail.iter().any(|detail| detail.usable);
	let mut ranked: Vec<&Detail> = analysis
		.detail
		.iter()
		.filter(|detail| detail.usable || !any_usable)
		.collect();
	ranked.sort_by(|a, b| b.si.unwrap_or(0.0).total_cmp(&a.si.unwrap_or(0.0)));

	let mut samples: Vec<Sample> = ranked
		.into_iter()
		.take(usize::from(count))
		.map(|detail| Sample {
			start_s: detail.start_s,
			length_s: SEARCH_SEGMENT_S.min(end_s - detail.start_s),
		})
		.collect();
	samples.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));

	let overlapping = samples
		.windows(2)
		.any(|pair| pair[0].start_s + pair[0].length_s > pair[1].start_s);
	if overlapping { whole } else { samples }
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

pub fn unreachable_text(crf: f64, score: Score, target: Target) -> String {
	format!(
		"quality target not reached at a smaller size ({})",
		measured(crf, score, target)
	)
}

/// The same measurement for an output written below its target because it was requested.
pub fn short_text(crf: f64, score: Score, target: Target) -> String {
	format!("quality target not reached ({})", measured(crf, score, target))
}

fn measured(crf: f64, score: Score, target: Target) -> String {
	format!(
		"VMAF {:.1} / {:.1} at CRF {crf}, target {:.1} / {:.1}",
		score.mean, score.p5, target.mean, target.floor
	)
}

pub fn unreachable_reason(crf: f64, score: Score, target: Target) -> KeepReason {
	KeepReason::QualityUnreachable(unreachable_text(crf, score, target))
}

/// The smaller of the two results that reach their targets (grain on a tie). When neither does, the one without grain
/// is kept: grain's denoiser only moves the picture further from the source.
pub fn grain_choice(grain: Searched, plain: Searched) -> GrainChoice {
	match (grain, plain) {
		(Searched::Found(grain), Searched::Found(plain)) if grain.sample_bytes > plain.sample_bytes => {
			GrainChoice::Plain
		}
		(Searched::Found(_), _) => GrainChoice::Grain,
		(Searched::Unreachable { .. }, _) => GrainChoice::Plain,
	}
}

/// Why film grain was left out.
#[allow(clippy::cast_precision_loss)]
pub fn dropped_grain_text(level: u8, grain: Searched, plain: Searched, grain_target: Target) -> String {
	let Searched::Found(plain) = plain else {
		return format!(
			"film grain {level} was left out: the quality target was reached neither with it nor without it"
		);
	};

	let with = match grain {
		Searched::Found(grain) => format!(
			"with it the samples needed CRF {} and came out {:.1}× the size",
			grain.crf,
			grain.sample_bytes as f64 / plain.sample_bytes.max(1) as f64
		),
		Searched::Unreachable { crf, score } => format!("with it {}", short_text(crf, score, grain_target)),
	};
	format!(
		"film grain {level} was left out: {with}; without it CRF {} measured VMAF {:.1} / {:.1}",
		plain.crf, plain.score.mean, plain.score.p5
	)
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
			window: self.plan.window,
			start: self.plan.starts.main,
			overlay: self.plan.overlay,
			subtitles: self.plan.subtitles.as_deref(),
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
	on_round: impl FnMut(usize),
) -> Result<Searched, JobError> {
	search_below(scorer, samples, start, target, prior_slope, None, on_round).await
}

/// `search`, given up as unreachable once a probe misses the target at `bytes` or more: every CRF that would pass is
/// lower and larger still, so nothing smaller than `bytes` can be found.
pub async fn search_below(
	scorer: &Scorer<'_>,
	samples: &[Sample],
	start: f64,
	target: Target,
	prior_slope: f64,
	bytes: Option<u64>,
	mut on_round: impl FnMut(usize),
) -> Result<Searched, JobError> {
	let bounds = bounds(&scorer.plan.encoder);
	let mut probes: Vec<Probe> = Vec::new();
	let mut sample_bytes: Vec<u64> = Vec::new();
	let mut crf = bounds.quantize(start);

	loop {
		on_round(probes.len());
		let (score, size) = scorer.probe(crf, samples, probes.len()).await?;
		probes.push(Probe { crf, score });
		sample_bytes.push(size);
		if !target.met(score) && bytes.is_some_and(|bytes| size >= bytes) {
			return Ok(Searched::Unreachable { crf, score });
		}

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
	use crate::preset::VmafOverride;
	use crate::quality::keys::{CacheKey, CodecFamily, DynamicRange, FpsBucket, Group, VideoGroup};
	use crate::video::analyze::{NoiseClass, ScanType, segments};
	use crate::video::plan::Resolution;

	fn detail(start_s: f64, si: f64, usable: bool) -> Detail {
		Detail {
			start_s,
			length_s: 3.0,
			si: Some(si),
			usable,
		}
	}

	#[allow(clippy::unnecessary_wraps)]
	fn whole(length_s: f64) -> Option<TrimWindow> {
		Some(TrimWindow { start_s: 0.0, length_s })
	}

	#[test]
	fn targets_follow_the_speed_and_allow_for_film_grain() {
		let preset = Overrides::default();

		assert_eq!(
			target(Speed::Slow, false, &preset),
			Target {
				mean: 95.0,
				floor: 91.0
			}
		);
		assert_eq!(
			target(Speed::Slow, true, &preset),
			Target {
				mean: 93.0,
				floor: 88.5
			}
		);
		assert_eq!(
			target(Speed::Moderate, false, &preset),
			Target {
				mean: 93.0,
				floor: 89.0
			}
		);
		assert_eq!(
			target(Speed::Moderate, true, &preset),
			Target {
				mean: 91.0,
				floor: 86.5
			}
		);
		assert_eq!(
			target(Speed::Fast, false, &preset),
			Target {
				mean: 93.0,
				floor: 88.0
			}
		);
	}

	fn overridden(mean: f64, floor: f64) -> Overrides {
		Overrides {
			video: Some(VmafOverride { mean, floor }),
			..Overrides::default()
		}
	}

	#[test]
	fn a_video_override_replaces_the_speed_targets_but_not_the_grain_allowance() {
		let custom = overridden(90.0, 85.0);

		assert_eq!(
			target(Speed::Moderate, false, &custom),
			Target {
				mean: 90.0,
				floor: 85.0
			}
		);
		assert_eq!(
			target(Speed::Moderate, true, &custom),
			Target {
				mean: 88.0,
				floor: 82.5
			}
		);
		assert_eq!(
			target(Speed::Slow, false, &custom),
			Target {
				mean: 90.0,
				floor: 85.0
			}
		);
		assert_eq!(
			target(Speed::Fast, false, &custom),
			target(Speed::Fast, false, &Overrides::default())
		);
	}

	#[test]
	fn the_target_reads_the_normalized_override() {
		assert_eq!(
			target(Speed::Moderate, false, &overridden(120.0, 60.0)),
			Target {
				mean: 99.0,
				floor: 70.0
			}
		);
		assert_eq!(
			target(Speed::Slow, false, &overridden(90.04, 85.06)),
			Target {
				mean: 90.0,
				floor: 85.1
			}
		);
		assert_eq!(
			target(Speed::Moderate, false, &overridden(f64::NAN, 85.0)),
			target(Speed::Moderate, false, &Overrides::default())
		);
	}

	#[test]
	fn overridden_targets_never_share_learned_values_with_the_preset() {
		let group = VideoGroup {
			codec: CodecFamily::H264,
			resolution: Resolution::Hd1080,
			fps: FpsBucket::UpTo31,
			range: DynamicRange::Sdr8,
			scan: ScanType::Progressive,
			grain: None,
			noisy: false,
			alpha: false,
		};
		let key = |overrides: &Overrides| {
			let target = target(Speed::Moderate, false, overrides);
			CacheKey {
				group: Group::Video(group),
				encoder: "svt-av1-4.2".to_owned(),
				preset: 5,
				target: target.mean,
				floor: Some(target.floor),
			}
			.to_string()
		};

		let preset = key(&Overrides::default());

		assert!(preset.ends_with("|t93.0/89.0"), "{preset}");
		assert_ne!(key(&overridden(90.0, 85.0)), preset);
		assert_ne!(key(&overridden(93.0, 85.0)), preset);
		assert_eq!(key(&overridden(93.0, 89.0)), preset);
	}

	#[test]
	fn only_moderate_and_slow_measure_and_moderate_trusts_a_learned_crf() {
		let learned = GroupStats {
			setting: 31.5,
			slope: Some(0.4),
			bpp: None,
			observations: 3,
			without_grain: false,
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
			noise_sigma: None,
			detail: vec![
				detail(5.0, 40.0, true),
				detail(35.0, 90.0, true),
				detail(65.0, 70.0, true),
				detail(95.0, 200.0, false),
			],
		};
		let four = |start_s: f64| Sample { start_s, length_s: 4.0 };

		assert_eq!(
			search_samples(&analysis, whole(100.0), Speed::Moderate),
			vec![four(5.0), four(35.0), four(65.0)]
		);
		assert_eq!(
			search_samples(&analysis, whole(100.0), Speed::Slow),
			vec![four(5.0), four(35.0), four(65.0)]
		);
		assert!(search_samples(&analysis, whole(100.0), Speed::Fast).is_empty());
		assert!(search_samples(&analysis, None, Speed::Moderate).is_empty());
		assert_eq!(
			search_samples(&analysis, whole(8.5), Speed::Slow),
			vec![Sample {
				start_s: 0.0,
				length_s: 8.5
			}]
		);
	}

	#[test]
	fn short_files_are_sampled_whole_instead_of_in_overlapping_samples() {
		let analysis = |duration: f64, count: usize| Analysis {
			scan: ScanType::Progressive,
			noise: NoiseClass::Clean,
			noise_sigma: None,
			detail: segments(Some(duration), count)
				.iter()
				.zip(0_u32..)
				.map(|(segment, index)| detail(segment.start_s, 50.0 + f64::from(index), true))
				.collect(),
		};
		let entire = |length_s: f64| vec![Sample { start_s: 0.0, length_s }];

		assert_eq!(
			search_samples(&analysis(12.0, 4), whole(12.0), Speed::Moderate),
			entire(12.0)
		);
		assert_eq!(
			search_samples(&analysis(16.0, 4), whole(16.0), Speed::Moderate),
			entire(16.0)
		);
		assert_eq!(
			search_samples(&analysis(24.0, 8), whole(24.0), Speed::Slow),
			entire(24.0)
		);
		assert_eq!(
			search_samples(&analysis(34.0, 8), whole(34.0), Speed::Slow),
			entire(34.0)
		);
		assert_eq!(
			search_samples(&analysis(17.0, 4), whole(17.0), Speed::Moderate).len(),
			3
		);
		assert_eq!(search_samples(&analysis(35.0, 8), whole(35.0), Speed::Slow).len(), 6);
	}

	#[test]
	fn samples_of_a_trim_stay_inside_it() {
		let analysis = Analysis {
			scan: ScanType::Progressive,
			noise: NoiseClass::Clean,
			noise_sigma: None,
			detail: vec![detail(40.0, 90.0, true), detail(57.0, 80.0, true)],
		};
		let trim = Some(TrimWindow {
			start_s: 38.0,
			length_s: 20.0,
		});
		let short = Some(TrimWindow {
			start_s: 38.0,
			length_s: 6.0,
		});

		assert_eq!(
			search_samples(&analysis, trim, Speed::Moderate),
			vec![
				Sample {
					start_s: 40.0,
					length_s: 4.0
				},
				Sample {
					start_s: 57.0,
					length_s: 1.0
				},
			]
		);
		assert_eq!(
			search_samples(&analysis, short, Speed::Moderate),
			vec![Sample {
				start_s: 38.0,
				length_s: 6.0
			}]
		);
	}

	#[test]
	fn low_detail_segments_are_sampled_only_when_no_segment_is_usable() {
		let analysis = |detail: Vec<Detail>| Analysis {
			scan: ScanType::Progressive,
			noise: NoiseClass::Clean,
			noise_sigma: None,
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
			search_samples(&one_usable, whole(100.0), Speed::Moderate),
			vec![four(35.0)]
		);
		assert_eq!(search_samples(&one_usable, whole(100.0), Speed::Slow), vec![four(35.0)]);
		assert_eq!(
			search_samples(&none_usable, whole(100.0), Speed::Moderate),
			vec![four(35.0), four(65.0), four(95.0)]
		);
		assert_eq!(
			search_samples(&none_usable, whole(100.0), Speed::Slow),
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
				film_grain: None
			})
			.step - 0.25)
				.abs() < 1e-9
		);
	}

	fn found(crf: f64, sample_bytes: u64) -> Searched {
		Searched::Found(Found {
			crf,
			score: Score {
				mean: 93.4,
				p5: 89.2,
				frames: 300,
			},
			sample_bytes,
			slope: 0.5,
		})
	}

	const MISSED: Searched = Searched::Unreachable {
		crf: 10.0,
		score: Score {
			mean: 90.1,
			p5: 84.0,
			frames: 300,
		},
	};

	#[test]
	fn when_both_reach_their_targets_the_smaller_wins() {
		assert_eq!(
			grain_choice(found(30.0, 101_000), found(29.0, 100_000)),
			GrainChoice::Plain
		);
		assert_eq!(
			grain_choice(found(30.0, 99_000), found(29.0, 100_000)),
			GrainChoice::Grain
		);
		assert_eq!(
			grain_choice(found(30.0, 100_000), found(29.0, 100_000)),
			GrainChoice::Grain
		);
		assert_eq!(
			grain_choice(found(12.0, 90_000), found(29.0, 100_000)),
			GrainChoice::Grain
		);
	}

	#[test]
	fn grain_that_cannot_reach_its_target_is_dropped() {
		assert_eq!(grain_choice(MISSED, found(29.0, 100_000)), GrainChoice::Plain);
		assert_eq!(grain_choice(MISSED, MISSED), GrainChoice::Plain);
		assert_eq!(grain_choice(found(12.0, 900_000), MISSED), GrainChoice::Grain);
	}

	#[test]
	fn dropped_grain_is_explained_with_both_searches() {
		let grain_target = target(Speed::Moderate, true, &Overrides::default());

		assert_eq!(
			dropped_grain_text(14, found(12.0, 600_000), found(29.0, 100_000), grain_target),
			"film grain 14 was left out: with it the samples needed CRF 12 and came out 6.0× the size; \
			 without it CRF 29 measured VMAF 93.4 / 89.2"
		);
		assert_eq!(
			dropped_grain_text(20, MISSED, found(29.0, 100_000), grain_target),
			"film grain 20 was left out: with it quality target not reached (VMAF 90.1 / 84.0 at CRF 10, target \
			 91.0 / 86.5); without it CRF 29 measured VMAF 93.4 / 89.2"
		);
		assert_eq!(
			dropped_grain_text(8, MISSED, MISSED, grain_target),
			"film grain 8 was left out: the quality target was reached neither with it nor without it"
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
