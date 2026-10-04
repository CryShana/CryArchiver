use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::budget::{Lanes, in_lanes};
use crate::calibration::tables;
use crate::options::TrimWindow;
use crate::outcome::{ErrorKind, JobError, KeepReason};
use crate::preset::{Overrides, Speed};
use crate::quality::memory::{GroupStats, Offset};
use crate::quality::search::{self as crf_search, Bounds, Choice, Next, Probe, Target};
use crate::quality::vmaf::{self, Reference, Score, Scoring};
use crate::toolchain::Toolchain;
use crate::video::analyze::{Analysis, DEFAULT_SEGMENTS, Detail};
use crate::video::encode::{Sample, encode_sample, median_rate};
use crate::video::plan::{Encoder, VideoPlan, lane_threads};
use crate::workspace::Workspace;

pub const MAX_RETRIES: u32 = 1;
pub const MAX_SEARCH_RAISE: f64 = 3.0;
/// The samples lean on the hardest scenes, so the full check usually lands above them: a group's offset only ever
/// raises the search target.
pub const MAX_SEARCH_LOWER: f64 = 0.0;

const MAX_SEARCH_MEAN: f64 = 99.0;
const MOTION_SHARE: usize = 3;
/// Short windows are sampled over a quarter of their length, up to this much of it.
const SHORT_SPAN_S: f64 = 120.0;
const SHORT_COVERAGE: f64 = 0.25;
/// Below this spatial detail a segment is a flat picture (black, a title card) that any CRF encodes perfectly.
const FLAT_SI: f64 = 2.0;
const MAX_ANALYSIS_SEGMENTS: usize = 48;
const LONG_SPAN_S: f64 = 600.0;
const NEAR_MISS: f64 = 0.5;
/// A sample gets a lane of its own only with at least this many threads: one-thread lanes encode slower in all.
const SAMPLE_LANE_THREADS: usize = 2;
const NO_MARGIN: Target = Target { mean: 0.0, floor: 0.0 };

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
const SLOW_ANALYSIS_SEGMENTS: usize = 8;
const MODERATE_SAMPLING: Sampling = Sampling {
	base: 3,
	per_sample_s: 600.0,
	most: 12,
};
const SLOW_SAMPLING: Sampling = Sampling {
	base: 6,
	per_sample_s: 300.0,
	most: 24,
};
const SEARCH_SEGMENT_S: f64 = 4.0;
/// The full check scores about this many frames per second of the output, and never fewer than `MIN_SCORED_FRAMES` in
/// all.
const SCORED_PER_S: f64 = 11.0;
const MIN_SCORED_FRAMES: u64 = 300;
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
	/// No probe reached the search target; the one closest to it, with its samples' size.
	Unreachable {
		crf: f64,
		score: Score,
		sample_bytes: u64,
	},
}

/// A search's result, with how fast its samples encoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Search {
	pub searched: Searched,
	/// Encoding seconds per second of samples, from the first probe unless the aim brought one; `NaN` when that probe's
	/// samples ran side by side.
	pub rate: f64,
	/// Every probe, in the order it was run: the samples' quality curve.
	pub probes: Vec<Probe>,
	/// The bytes of each probe's samples, in the same order.
	pub sample_bytes: Vec<u64>,
}

/// What a search aims at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aim {
	/// The target the full check holds the encode to.
	pub nominal: Target,
	/// What the samples must reach before the margin: the nominal target moved by the group's learned offset.
	pub search: Target,
	/// The source seconds the full encode covers. The margin grows with the time encoding them will take.
	pub length_s: Option<f64>,
	/// An encoding rate (seconds per second of samples) measured earlier, so that the margin comes from it rather
	/// than from this search's first probe.
	pub rate: Option<f64>,
}

/// A probe's measurement over all the samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probed {
	pub score: Score,
	pub bytes: u64,
	/// The time the sample encodes took.
	pub encode_s: f64,
	/// The median sample's encoding seconds per second of source, so that one slow start-up doesn't set it; `NaN` when
	/// the samples ran side by side.
	pub rate: f64,
}

struct Sampling {
	base: usize,
	per_sample_s: f64,
	most: usize,
}

/// How far a search has come.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SearchStep {
	/// Probe `round`, counted from 0, starts at `crf`.
	Probe {
		round: usize,
		crf: f64,
	},
	/// `done` of the probe's `total` samples are scored.
	Samples {
		done: usize,
		total: usize,
	},
	Probed {
		crf: f64,
		score: Score,
	},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrainChoice {
	Grain,
	Plain,
}

#[derive(Clone, Copy)]
pub struct Scorer<'a> {
	pub tools: &'a Toolchain,
	pub source: &'a Path,
	pub workspace: &'a Workspace,
	pub plan: &'a VideoPlan,
	pub threads: usize,
	/// How many of a probe's samples may be encoded and scored at once.
	pub lanes: Lanes<'a>,
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

	let video = &tables().video;
	Target {
		mean: base.mean - video.grain_allowance_mean,
		floor: base.floor - video.grain_allowance_floor,
	}
}

/// The target the samples must reach so that the full check, which lands `offset` from them, reaches `target`. It
/// moves at most `MAX_SEARCH_RAISE` up and `MAX_SEARCH_LOWER` down.
pub fn search_target(target: Target, offset: Option<Offset>) -> Target {
	let Some(offset) = offset else {
		return target;
	};

	let shift = |gap: f64| (-gap).clamp(-MAX_SEARCH_LOWER, MAX_SEARCH_RAISE);
	let mean = (target.mean + shift(offset.mean)).min(MAX_SEARCH_MEAN);
	Target {
		mean,
		floor: (target.floor + shift(offset.p5)).min(mean),
	}
}

/// The expected full encode time from which a learned CRF is searched again: where the tables start the margin
/// (`margin-from-s`).
pub fn long_encode_s() -> f64 {
	tables().video.margin_from_s
}

/// Whether a full encode of `length_s` at `rate` (encoding seconds per second of source) is expected to take at least
/// `from_s` (by default `long_encode_s`): long enough that a learned CRF is searched again, so that a second full
/// encode of it is less likely.
pub fn long_encode(rate: f64, length_s: f64, from_s: f64) -> bool {
	rate * length_s >= from_s
}

/// The margin for a full encode expected to take `expected_s`: none up to `margin-from-s`, then growing with each
/// doubling to the full margin at `full-margin-s`.
pub fn encode_margin(expected_s: f64) -> Target {
	let video = &tables().video;

	if expected_s.is_nan() || expected_s <= video.margin_from_s {
		return NO_MARGIN;
	}

	let share =
		((expected_s / video.margin_from_s).log2() / (video.full_margin_s / video.margin_from_s).log2()).min(1.0);
	Target {
		mean: video.full_margin_mean * share,
		floor: video.full_margin_floor * share,
	}
}

impl Aim {
	pub fn exact(target: Target) -> Self {
		Self {
			nominal: target,
			search: target,
			length_s: None,
			rate: None,
		}
	}

	/// Whether the search's first probe measures the encoding rate the margin comes from.
	pub fn times_its_samples(self) -> bool {
		self.length_s.is_some() && self.rate.is_none()
	}

	/// The search target with the margin for the full encode, at the rate given earlier or else `measured_rate`
	/// (encoding seconds per second of samples). It stays within `MAX_SEARCH_RAISE` of the nominal target and never
	/// falls below it.
	pub fn target(self, measured_rate: f64) -> Target {
		let Some(length_s) = self.length_s else {
			return self.search;
		};

		let margin = encode_margin(self.rate.unwrap_or(measured_rate) * length_s);
		let mean = (self.search.mean + margin.mean)
			.min(self.nominal.mean + MAX_SEARCH_RAISE)
			.min(MAX_SEARCH_MEAN)
			.max(self.search.mean)
			.max(self.nominal.mean);
		let floor = (self.search.floor + margin.floor)
			.min(self.nominal.floor + MAX_SEARCH_RAISE)
			.min(mean)
			.max(self.search.floor)
			.max(self.nominal.floor);
		Target { mean, floor }
	}
}

fn sampling(speed: Speed) -> Option<Sampling> {
	match speed {
		Speed::Slow => Some(SLOW_SAMPLING),
		Speed::Moderate => Some(MODERATE_SAMPLING),
		Speed::Fast => None,
	}
}

/// How many search samples a span gets: a quarter of a short window, then one more for each stretch of a long one.
#[allow(
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss,
	clippy::cast_precision_loss
)]
pub fn sample_count(speed: Speed, length_s: f64) -> usize {
	let Some(sampling) = sampling(speed) else {
		return 0;
	};

	let covered_s = SHORT_COVERAGE * length_s.clamp(0.0, SHORT_SPAN_S)
		+ length_s.max(0.0) / sampling.per_sample_s * SEARCH_SEGMENT_S;
	let wanted = (covered_s / SEARCH_SEGMENT_S).ceil();
	if wanted.is_nan() {
		return sampling.base;
	}

	(wanted.min(sampling.most as f64) as usize).clamp(sampling.base, sampling.most)
}

/// Twice as many segments as samples, so that the hardest parts of a long span can be found.
pub fn analysis_segments(speed: Speed, length_s: Option<f64>) -> usize {
	let base = match speed {
		Speed::Slow => SLOW_ANALYSIS_SEGMENTS,
		Speed::Moderate | Speed::Fast => DEFAULT_SEGMENTS,
	};

	let samples = length_s.map_or(0, |length_s| sample_count(speed, length_s));
	base.max(2 * samples).min(MAX_ANALYSIS_SEGMENTS)
}

pub fn bounds(encoder: &Encoder) -> Bounds {
	match encoder {
		Encoder::SvtAv1 { .. } => SVT_BOUNDS,
		Encoder::Aom { .. } | Encoder::Vp9Alpha { .. } => WHOLE_STEP_BOUNDS,
	}
}

/// At MODERATE a learned CRF is used as it is, except on a span of `LONG_SPAN_S` or more: a second full encode of a
/// long file costs far more than searching from the learned CRF.
pub fn strategy(
	speed: Speed,
	stats: Option<&GroupStats>,
	table_crf: f64,
	vmaf: bool,
	length_s: Option<f64>,
) -> Strategy {
	let learned = stats.map(|stats| stats.setting);
	let long = length_s.is_some_and(|length_s| length_s >= LONG_SPAN_S);

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
			Some(crf) if long => Strategy::Search { start: crf },
			Some(crf) => Strategy::Fixed { crf, verify: true },
			None => Strategy::Search { start: table_crf },
		},
		Speed::Slow => Strategy::Search {
			start: learned.unwrap_or(table_crf),
		},
	}
}

/// The segments to probe: the hardest ones first (the most detailed, the busiest, the darkest and the noisiest), then
/// the most detailed usable ones, then the rest by detail. Held frames and flat pictures are probed only when there is
/// nothing else.
#[allow(clippy::cast_precision_loss)]
pub fn search_samples(analysis: &Analysis, span: Option<TrimWindow>, speed: Speed) -> Vec<Sample> {
	let wanted = span.map_or(0, |span| sample_count(speed, span.length_s));
	let Some(span) = span.filter(|span| span.length_s > 0.0 && wanted > 0) else {
		return Vec::new();
	};

	let whole = vec![Sample {
		start_s: span.start_s,
		length_s: span.length_s,
	}];
	if SEARCH_SEGMENT_S * wanted as f64 >= span.length_s {
		return whole;
	}

	let end_s = span.start_s + span.length_s;
	let chosen = hardest_first(&analysis.detail, wanted);
	let mut samples: Vec<Sample> = chosen
		.into_iter()
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

/// At least `wanted` segments, more when the hardest ones alone are more.
fn hardest_first(details: &[Detail], wanted: usize) -> Vec<&Detail> {
	let mut chosen = hardest_segments(details, wanted);
	for detail in ranked(details) {
		if chosen.len() >= wanted {
			break;
		}
		add_once(&mut chosen, detail);
	}
	chosen
}

/// The usable segments by detail, then the low-detail ones that still move, by detail; every segment when there are
/// neither.
fn ranked(details: &[Detail]) -> Vec<&Detail> {
	let by_detail = |list: &mut Vec<&Detail>| list.sort_by(|a, b| b.si.unwrap_or(0.0).total_cmp(&a.si.unwrap_or(0.0)));

	let mut ranked: Vec<&Detail> = details.iter().filter(|detail| detail.usable).collect();
	by_detail(&mut ranked);
	let mut low_detail: Vec<&Detail> = details
		.iter()
		.filter(|detail| !detail.usable && moving_picture(detail))
		.collect();
	by_detail(&mut low_detail);
	ranked.extend(low_detail);
	if ranked.is_empty() {
		ranked = details.iter().collect();
		by_detail(&mut ranked);
	}
	ranked
}

fn moving_picture(detail: &Detail) -> bool {
	!detail.still && detail.si.is_none_or(|si| si >= FLAT_SI)
}

/// The hardest segments of `wanted` samples: the most detailed, the `wanted / MOTION_SHARE` busiest, the darkest and
/// the noisiest. The busiest, darkest and noisiest may be segments that are not usable for detail (low detail, or
/// mostly black to blackdetect): near-black scenes with content are exactly what blackdetect calls black, and only
/// flat and held segments are left out.
pub fn hardest_segments(details: &[Detail], wanted: usize) -> Vec<&Detail> {
	let candidates: Vec<&Detail> = details.iter().filter(|detail| moving_picture(detail)).collect();
	let mut busiest: Vec<&Detail> = candidates
		.iter()
		.copied()
		.filter(|detail| detail.ti.is_some())
		.collect();
	busiest.sort_by(|a, b| b.ti.unwrap_or(0.0).total_cmp(&a.ti.unwrap_or(0.0)));
	let darkest = candidates
		.iter()
		.copied()
		.filter(|detail| detail.luma.is_some())
		.min_by(|a, b| a.luma.unwrap_or(0.0).total_cmp(&b.luma.unwrap_or(0.0)));
	let noisiest = candidates
		.iter()
		.copied()
		.filter(|detail| detail.noise.is_some())
		.max_by(|a, b| a.noise.unwrap_or(0.0).total_cmp(&b.noise.unwrap_or(0.0)));

	let mut chosen: Vec<&Detail> = Vec::new();
	let hardest = ranked(details)
		.first()
		.copied()
		.into_iter()
		.chain(busiest.into_iter().take(wanted.div_ceil(MOTION_SHARE)))
		.chain(darkest)
		.chain(noisiest);
	for detail in hardest {
		add_once(&mut chosen, detail);
	}
	chosen
}

fn add_once<'a>(chosen: &mut Vec<&'a Detail>, detail: &'a Detail) {
	if !chosen.iter().any(|picked| std::ptr::eq(*picked, detail)) {
		chosen.push(detail);
	}
}

/// Every how many frames the full check scores one, over an output of `frames` decoded over `decoded_s`: about
/// `SCORED_PER_S` a second, but at least `MIN_SCORED_FRAMES` in all (every frame of a shorter output). The step is
/// always odd: SVT-AV1's mini-GOP puts every other frame in its top temporal layer, the worst-coded one, and an even
/// key interval keeps those frames on the same parity, as do the two fields of a deinterlaced frame, so an even step
/// could score none of them.
#[allow(
	clippy::cast_precision_loss,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]
pub fn subsample(frames: u64, decoded_s: f64) -> u64 {
	let fps = if decoded_s > 0.0 {
		frames as f64 / decoded_s
	} else {
		FALLBACK_FPS
	};
	let by_rate = (fps / SCORED_PER_S).round().max(1.0) as u64 | 1;
	let most = (frames / MIN_SCORED_FRAMES).max(1);

	by_rate.min((most - 1) | 1)
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

/// A full check, the first or the last, that missed by so little, on the mean and on the p5 alike, that the output is
/// written as it is instead of being encoded again.
pub fn near_miss(score: Score, target: Target) -> bool {
	score.mean >= target.mean - NEAR_MISS && score.p5 >= target.floor - NEAR_MISS
}

/// Under a bitrate cap the cap is named instead: a capped encode is never encoded again, whatever it misses by.
pub fn near_miss_text(crf: f64, score: Score, target: Target, cap_kbps: Option<u32>) -> String {
	let written = format!(
		"written slightly below the quality target: VMAF {:.1} / {:.1} at CRF {crf}, target {:.1} / {:.1} (within \
		 {NEAR_MISS:.1}",
		score.mean, score.p5, target.mean, target.floor
	);
	match cap_kbps {
		Some(_) => with_cap_note(&format!("{written})"), cap_kbps),
		None => format!("{written}, so not encoded again)"),
	}
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

pub fn with_cap_note(reason: &str, cap_kbps: Option<u32>) -> String {
	match cap_kbps {
		Some(kbps) => format!("{reason} under the {} Mbit/s cap", f64::from(kbps) / 1000.0),
		None => reason.to_owned(),
	}
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

/// Why film grain was left out; a target missed under a bitrate cap names the cap.
#[allow(clippy::cast_precision_loss)]
pub fn dropped_grain_text(
	level: u8,
	grain: Searched,
	plain: Searched,
	grain_target: Target,
	cap_kbps: Option<u32>,
) -> String {
	let Searched::Found(plain) = plain else {
		return format!(
			"film grain {level} was left out: {}",
			with_cap_note(
				"the quality target was reached neither with it nor without it",
				cap_kbps
			)
		);
	};

	let with = match grain {
		Searched::Found(grain) => format!(
			"with it the samples needed CRF {} and came out {:.1}× the size",
			grain.crf,
			grain.sample_bytes as f64 / plain.sample_bytes.max(1) as f64
		),
		Searched::Unreachable { crf, score, .. } => format!(
			"with it {}",
			with_cap_note(&short_text(crf, score, grain_target), cap_kbps)
		),
	};
	format!(
		"film grain {level} was left out: {with}; without it CRF {} measured VMAF {:.1} / {:.1}",
		plain.crf, plain.score.mean, plain.score.p5
	)
}

impl Scorer<'_> {
	/// Encodes and scores `samples` at `crf`, telling `on_samples` how many are done, from none to all. Samples run
	/// side by side when the lanes allow it, each on its share of the threads; their times then don't show how fast
	/// one sample encodes alone, so the rate is `NaN`.
	pub async fn probe(
		&self,
		crf: f64,
		samples: &[Sample],
		round: usize,
		mut on_samples: impl FnMut(usize),
	) -> Result<Probed, JobError> {
		let granted = self.lanes.grant(samples.len(), SAMPLE_LANE_THREADS);
		let lane = granted.lane_threads();
		let plan = VideoPlan {
			encoder: self.plan.encoder.with_crf(crf),
			threads: lane.map_or(self.plan.threads, |threads| Some(lane_threads(threads))),
			..self.plan.clone()
		};
		on_samples(0);

		let (plan, done) = (&plan, &Mutex::new((0, on_samples)));
		let measured = in_lanes(samples.len(), granted.lanes, move |index| async move {
			let sample = &samples[index];
			let output = self.workspace.file(&format!("probe{round}-{index}.mkv"));
			let started = Instant::now();
			let bytes = encode_sample(&self.tools.ffmpeg, self.source, &output, plan, sample, self.cancel).await?;
			let elapsed_s = started.elapsed().as_secs_f64();

			let log = format!("vmaf-probe{round}-{index}.json");
			let scoring = self.lane_scoring(&log, lane);
			let cmd = vmaf::sample_command(
				&self.tools.ffmpeg,
				&output,
				&self.reference(),
				sample.start_s,
				sample.length_s,
				&scoring,
			);
			let scores = vmaf::frames(&cmd, &scoring, |_| {}, self.cancel).await?;
			let _ = fs::remove_file(&output);

			let (count, on_samples) = &mut *done.lock().unwrap_or_else(PoisonError::into_inner);
			*count += 1;
			on_samples(*count);
			Ok::<_, JobError>((bytes, elapsed_s, scores))
		})
		.await?;

		let rates = samples
			.iter()
			.zip(&measured)
			.filter(|(sample, _)| sample.length_s > 0.0 && lane.is_none())
			.map(|(sample, (_, elapsed_s, _))| elapsed_s / sample.length_s)
			.collect();
		let scores: Vec<f64> = measured
			.iter()
			.flat_map(|(_, _, scores)| scores.iter().copied())
			.collect();
		Ok(Probed {
			score: pooled(&scores)?,
			bytes: measured.iter().map(|(bytes, _, _)| bytes).sum(),
			encode_s: measured.iter().map(|(_, elapsed_s, _)| elapsed_s).sum(),
			rate: median_rate(rates),
		})
	}

	/// The full check of `temp`, `total_frames` decoded over `decoded_s`: the whole output against the same span of the
	/// source, both decoded through, so that trims, TS and PS starts, inverse telecine and burned subtitles line up as in
	/// the encode, scored on every `subsample`th frame.
	pub async fn verify(
		&self,
		temp: &Path,
		total_frames: u64,
		decoded_s: f64,
		on_fraction: impl FnMut(f64),
	) -> Result<Score, JobError> {
		let scores = self
			.whole_scores(temp, subsample(total_frames, decoded_s), total_frames, on_fraction)
			.await?;
		pooled(&scores)
	}

	/// The VMAF of every `every`th frame of `temp`, `total_frames` long, in order.
	#[allow(clippy::cast_precision_loss)]
	pub async fn whole_scores(
		&self,
		temp: &Path,
		every: u64,
		total_frames: u64,
		mut on_fraction: impl FnMut(f64),
	) -> Result<Vec<f64>, JobError> {
		let scoring = self.scoring(VERIFY_LOG);
		let cmd = vmaf::whole_command(&self.tools.ffmpeg, temp, &self.reference(), every, &scoring);

		vmaf::frames(
			&cmd,
			&scoring,
			|progress| {
				if let Some(frame) = progress.frame.filter(|_| total_frames > 0) {
					on_fraction((frame as f64 / total_frames as f64).clamp(0.0, 1.0));
				}
			},
			self.cancel,
		)
		.await
	}

	fn reference(&self) -> Reference<'_> {
		Reference {
			source: self.source,
			stream: self.plan.streams[0],
			decoder: self.plan.decoder,
			shape: &self.plan.shape,
			filters: &self.plan.filters,
			window: self.plan.window,
			start: self.plan.starts.main,
			overlay: self.plan.overlay,
			subtitles: self.plan.subtitles.as_deref(),
		}
	}

	fn scoring<'b>(&'b self, log: &'b str) -> Scoring<'b> {
		self.lane_scoring(log, None)
	}

	/// Scoring on a lane's threads, or else on the job's.
	fn lane_scoring<'b>(&'b self, log: &'b str, lane: Option<usize>) -> Scoring<'b> {
		Scoring {
			model: vmaf::model_for(self.plan.width, self.plan.height),
			threads: lane.unwrap_or(self.threads),
			budget: lane.or(self.plan.threads.map(|threads| threads.tool)),
			dir: self.workspace.dir(),
			log,
		}
	}
}

pub async fn search(
	scorer: &Scorer<'_>,
	samples: &[Sample],
	start: f64,
	aim: Aim,
	prior_slope: f64,
	on_step: impl FnMut(SearchStep),
) -> Result<Search, JobError> {
	search_below(scorer, samples, start, aim, prior_slope, None, on_step).await
}

/// `search`, given up as unreachable once a probe misses the target at `bytes` or more: every CRF that would pass is
/// lower and larger still, so nothing smaller than `bytes` can be found. The target takes its margin from how long the
/// first probe's encodes took, unless the aim brings a rate; that probe's samples then run one at a time on the job's
/// caps, so that each one's time is what it takes alone.
pub async fn search_below(
	scorer: &Scorer<'_>,
	samples: &[Sample],
	start: f64,
	aim: Aim,
	prior_slope: f64,
	bytes: Option<u64>,
	mut on_step: impl FnMut(SearchStep),
) -> Result<Search, JobError> {
	let bounds = bounds(&scorer.plan.encoder);
	let mut target = aim.search;
	let mut rate = f64::NAN;
	let mut probes: Vec<Probe> = Vec::new();
	let mut sample_bytes: Vec<u64> = Vec::new();
	let mut crf = bounds.quantize(start);

	let searched = loop {
		on_step(SearchStep::Probe {
			round: probes.len(),
			crf,
		});
		let on_samples = |done| {
			on_step(SearchStep::Samples {
				done,
				total: samples.len(),
			});
		};
		let timing = if probes.is_empty() && aim.times_its_samples() {
			Scorer {
				lanes: Lanes::SERIAL,
				..*scorer
			}
		} else {
			*scorer
		};
		let measured = timing.probe(crf, samples, probes.len(), on_samples).await?;
		on_step(SearchStep::Probed {
			crf,
			score: measured.score,
		});

		if probes.is_empty() {
			rate = aim.rate.unwrap_or(measured.rate);
			target = aim.target(rate);
		}

		probes.push(Probe {
			crf,
			score: measured.score,
		});
		sample_bytes.push(measured.bytes);
		if !target.met(measured.score) && bytes.is_some_and(|bytes| measured.bytes >= bytes) {
			break Searched::Unreachable {
				crf,
				score: measured.score,
				sample_bytes: measured.bytes,
			};
		}

		match crf_search::next(&probes, target, bounds, prior_slope) {
			Next::Probe(next) => crf = next,
			Next::Done(Choice::Chosen { index, slope }) => {
				break Searched::Found(Found {
					crf: probes[index].crf,
					score: probes[index].score,
					sample_bytes: sample_bytes[index],
					slope,
				});
			}
			Next::Done(Choice::Unreachable { index }) => {
				break Searched::Unreachable {
					crf: probes[index].crf,
					score: probes[index].score,
					sample_bytes: sample_bytes[index],
				};
			}
		}
	};
	Ok(Search {
		searched,
		rate,
		probes,
		sample_bytes,
	})
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
			ti: None,
			luma: None,
			noise: None,
			usable,
			still: false,
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
			unmeasured: false,
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
			offset: None,
			observations: 3,
			without_grain: false,
		};

		let short = Some(599.0);

		assert_eq!(
			strategy(Speed::Fast, Some(&learned), 35.0, true, short),
			Strategy::Fixed {
				crf: 35.0,
				verify: false
			}
		);
		assert_eq!(
			strategy(Speed::Moderate, None, 29.0, true, short),
			Strategy::Search { start: 29.0 }
		);
		for length_s in [short, None] {
			assert_eq!(
				strategy(Speed::Moderate, Some(&learned), 29.0, true, length_s),
				Strategy::Fixed {
					crf: 31.5,
					verify: true
				}
			);
		}
		assert_eq!(
			strategy(Speed::Slow, Some(&learned), 25.0, true, short),
			Strategy::Search { start: 31.5 }
		);
		assert_eq!(
			strategy(Speed::Slow, None, 25.0, false, short),
			Strategy::Fixed {
				crf: 25.0,
				verify: false
			}
		);
	}

	#[test]
	fn a_learned_crf_is_searched_again_once_the_full_encode_is_expected_to_take_a_quarter_hour() {
		assert!(!long_encode(1.0, 300.0, 900.0));
		assert!(!long_encode(2.9, 300.0, 900.0));
		assert!(long_encode(3.0, 300.0, 900.0));
		assert!(long_encode(12.0, 120.0, 900.0));
		assert!(!long_encode(f64::NAN, 300.0, 900.0));
		assert!(long_encode(0.01, 120.0, 1.0));
	}

	#[test]
	fn a_learned_crf_is_searched_again_where_the_tables_start_the_margin() {
		assert!((long_encode_s() - tables().video.margin_from_s).abs() < 1e-9);
		assert!(!long_encode(0.999, long_encode_s(), long_encode_s()));
		assert!(long_encode(1.0, long_encode_s(), long_encode_s()));
	}

	#[test]
	fn a_long_file_at_moderate_searches_from_its_learned_crf() {
		let learned = GroupStats {
			setting: 31.5,
			slope: Some(0.4),
			bpp: None,
			offset: None,
			observations: 3,
			without_grain: false,
		};

		assert_eq!(
			strategy(Speed::Moderate, Some(&learned), 29.0, true, Some(600.0)),
			Strategy::Search { start: 31.5 }
		);
		assert_eq!(
			strategy(Speed::Moderate, Some(&learned), 29.0, false, Some(7200.0)),
			Strategy::Fixed {
				crf: 31.5,
				verify: false
			}
		);
		assert_eq!(
			strategy(Speed::Fast, Some(&learned), 35.0, true, Some(7200.0)),
			Strategy::Fixed {
				crf: 35.0,
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
		assert_eq!(
			search_samples(&analysis, whole(100.0), Speed::Moderate),
			vec![four(5.0), four(35.0), four(65.0), four(95.0)]
		);
		assert_eq!(
			search_samples(&analysis, whole(100.0), Speed::Slow),
			vec![four(5.0), four(35.0), four(65.0), four(95.0)]
		);
		assert_eq!(
			search_samples(
				&analysis_of(vec![
					detail(2.0, 40.0, true),
					detail(12.0, 200.0, false),
					detail(22.0, 60.0, true),
					detail(32.0, 50.0, true),
				]),
				whole(40.0),
				Speed::Moderate
			),
			vec![four(2.0), four(22.0), four(32.0)]
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

	fn analysis_of(detail: Vec<Detail>) -> Analysis {
		Analysis {
			scan: ScanType::Progressive,
			noise: NoiseClass::Clean,
			noise_sigma: None,
			detail,
		}
	}

	fn four(start_s: f64) -> Sample {
		Sample { start_s, length_s: 4.0 }
	}

	#[test]
	fn low_detail_segments_fill_the_samples_up_and_flat_ones_only_when_nothing_else_is_left() {
		let one_usable = analysis_of(vec![
			detail(5.0, 0.0, false),
			detail(35.0, 60.0, true),
			detail(65.0, 12.0, false),
			detail(95.0, 14.0, false),
		]);
		let none_usable = analysis_of(vec![
			detail(5.0, 0.0, false),
			detail(35.0, 9.0, false),
			detail(65.0, 12.0, false),
			detail(95.0, 14.0, false),
		]);
		let flat = analysis_of(vec![detail(5.0, 0.0, false), detail(35.0, 1.0, false)]);
		let held = analysis_of(vec![
			detail(5.0, 70.0, true),
			Detail {
				still: true,
				..detail(35.0, 50.0, false)
			},
			detail(65.0, 40.0, true),
		]);

		for speed in [Speed::Moderate, Speed::Slow] {
			assert_eq!(
				search_samples(&one_usable, whole(100.0), speed),
				vec![four(35.0), four(65.0), four(95.0)]
			);
			assert_eq!(
				search_samples(&none_usable, whole(100.0), speed),
				vec![four(35.0), four(65.0), four(95.0)]
			);
			assert_eq!(search_samples(&flat, whole(100.0), speed), vec![four(5.0), four(35.0)]);
			assert_eq!(search_samples(&held, whole(100.0), speed), vec![four(5.0), four(65.0)]);
		}
	}

	#[test]
	fn the_busiest_segment_gets_one_of_the_search_samples() {
		let moving = |start_s, si, ti| Detail {
			ti: Some(ti),
			..detail(start_s, si, true)
		};
		let analysis = analysis_of(vec![
			moving(2.0, 90.0, 5.0),
			moving(12.0, 80.0, 2.0),
			moving(22.0, 20.0, 40.0),
			moving(32.0, 70.0, 1.0),
		]);

		let starts: Vec<f64> = search_samples(&analysis, whole(40.0), Speed::Moderate)
			.iter()
			.map(|sample| sample.start_s)
			.collect();

		assert_eq!(starts, [2.0, 12.0, 22.0]);
	}

	#[test]
	fn the_darkest_and_the_noisiest_segments_are_always_sampled() {
		let analysis = analysis_of(vec![
			detail(2.0, 90.0, true),
			Detail {
				luma: Some(14.0),
				..detail(12.0, 6.0, false)
			},
			detail(22.0, 80.0, true),
			Detail {
				noise: Some(2.4),
				..detail(32.0, 30.0, true)
			},
			Detail {
				luma: Some(90.0),
				noise: Some(0.4),
				..detail(42.0, 70.0, true)
			},
			Detail {
				luma: Some(3.0),
				..detail(52.0, 1.0, false)
			},
		]);

		let starts: Vec<f64> = search_samples(&analysis, whole(60.0), Speed::Moderate)
			.iter()
			.map(|sample| sample.start_s)
			.collect();

		assert_eq!(
			starts,
			[2.0, 12.0, 22.0, 32.0],
			"the flat black one at 52 s is left out"
		);
	}

	#[test]
	fn a_short_window_whose_other_segments_are_dark_still_gets_several_samples() {
		let analysis = analysis_of(
			(0..8_u32)
				.map(|index| {
					let start_s = 2.0 + 7.0 * f64::from(index);
					if index == 3 {
						detail(start_s, 40.0, true)
					} else {
						Detail {
							luma: Some(10.0 + f64::from(index)),
							..detail(start_s, 5.0 + f64::from(index), false)
						}
					}
				})
				.collect(),
		);

		let samples = search_samples(&analysis, whole(60.0), Speed::Moderate);

		assert_eq!(samples.len(), 4, "{samples:?}");
		assert!(
			samples.contains(&four(23.0)) && samples.contains(&four(2.0)),
			"{samples:?}"
		);
	}

	#[test]
	fn samples_cover_a_quarter_of_a_short_window_and_grow_with_the_length() {
		let counts =
			|speed: Speed| [60.0, 100.0, 120.0, 600.0, 3600.0, 7200.0].map(|length| sample_count(speed, length));

		assert_eq!(counts(Speed::Moderate), [4, 7, 8, 9, 12, 12]);
		assert_eq!(counts(Speed::Slow), [6, 7, 8, 10, 20, 24]);
		assert_eq!(counts(Speed::Fast), [0; 6]);
		assert_eq!(sample_count(Speed::Moderate, 20.0), 3);
		assert_eq!(analysis_segments(Speed::Moderate, None), 4);
		assert_eq!(analysis_segments(Speed::Slow, None), 8);
		assert_eq!(analysis_segments(Speed::Moderate, Some(20.0)), 6);
		assert_eq!(analysis_segments(Speed::Moderate, Some(3600.0)), 24);
		assert_eq!(analysis_segments(Speed::Slow, Some(7200.0)), 48);
		assert_eq!(analysis_segments(Speed::Fast, Some(7200.0)), 4);
	}

	#[test]
	fn the_search_aims_above_the_target_by_the_learned_shortfall_within_limits() {
		let target = Target {
			mean: 93.0,
			floor: 89.0,
		};
		let aimed = |mean, p5| search_target(target, Some(Offset { mean, p5 }));
		let near = |a: Target, b: Target| (a.mean - b.mean).abs() < 1e-9 && (a.floor - b.floor).abs() < 1e-9;

		assert_eq!(search_target(target, None), target);
		assert!(near(
			aimed(-0.4, -2.7),
			Target {
				mean: 93.4,
				floor: 91.7
			}
		));
		assert!(near(
			aimed(-9.0, -9.0),
			Target {
				mean: 96.0,
				floor: 92.0
			}
		));
		assert_eq!(aimed(2.0, 0.5), target);
		assert!(near(
			aimed(1.0, -2.0),
			Target {
				mean: 93.0,
				floor: 91.0
			}
		));
		assert_eq!(
			search_target(
				Target {
					mean: 98.0,
					floor: 97.0
				},
				Some(Offset { mean: -3.0, p5: -3.0 })
			),
			Target {
				mean: 99.0,
				floor: 99.0
			}
		);
	}

	#[test]
	fn the_margin_grows_with_the_time_the_full_encode_will_take() {
		let nominal = Target {
			mean: 93.0,
			floor: 89.0,
		};
		let aim = |search: Target, length_s: f64| Aim {
			nominal,
			search,
			length_s: Some(length_s),
			rate: None,
		};
		let near = |a: Target, mean: f64, floor: f64| (a.mean - mean).abs() < 1e-9 && (a.floor - floor).abs() < 1e-9;
		let at_realtime = |aim: Aim| aim.target(1.0);

		assert_eq!(at_realtime(aim(nominal, 600.0)), nominal);
		assert_eq!(aim(nominal, 1800.0).target(0.5), nominal);
		assert!(near(at_realtime(aim(nominal, 1800.0)), 93.25, 89.375));
		assert!(near(at_realtime(aim(nominal, 3600.0)), 93.5, 89.75));
		assert!(near(aim(nominal, 3600.0).target(4.0), 94.0, 90.5));
		assert!(near(aim(nominal, 3600.0).target(40.0), 94.0, 90.5));
		assert!(near(
			Aim {
				rate: Some(4.0),
				..aim(nominal, 3600.0)
			}
			.target(0.1),
			94.0,
			90.5
		));
		assert_eq!(
			Aim {
				search: Target {
					mean: 92.0,
					floor: 88.0
				},
				..aim(nominal, 600.0)
			}
			.target(1.0),
			nominal,
			"never below the nominal target"
		);
		assert!(near(
			aim(
				Target {
					mean: 95.5,
					floor: 91.5
				},
				14_400.0
			)
			.target(1.0),
			96.0,
			92.0
		));
		assert_eq!(Aim::exact(nominal).target(1000.0), nominal);
		assert_eq!(aim(nominal, 3600.0).target(f64::NAN), nominal);
	}

	#[test]
	fn the_check_scores_about_eleven_frames_a_second_at_least_three_hundred_on_an_odd_step() {
		let every = |fps: f64, seconds: f64| {
			#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
			let frames = (fps * seconds) as u64;
			subsample(frames, seconds)
		};
		let rates = [23.976, 24.0, 25.0, 29.97, 50.0, 59.94, 60.0];

		assert_eq!(rates.map(|fps| every(fps, 3600.0)), [3, 3, 3, 3, 5, 5, 5]);
		assert_eq!(rates.map(|fps| every(fps, 60.0)), [3, 3, 3, 3, 5, 5, 5]);
		assert_eq!(rates.map(|fps| every(fps, 30.0)), [1, 1, 1, 1, 5, 5, 5]);
		assert_eq!(rates.map(|fps| every(fps, 20.0)), [1, 1, 1, 1, 3, 3, 3]);
		assert_eq!(rates.map(|fps| every(fps, 10.0)), [1, 1, 1, 1, 1, 1, 1]);
		assert_eq!(rates.map(|fps| every(fps, 3.0)), [1; 7]);
		assert_eq!([15.0, 12.0, 8.0, 0.5].map(|fps| every(fps, 3600.0)), [1, 1, 1, 1]);
		assert_eq!(every(48.0, 3600.0), 5);
		assert_eq!(subsample(0, 0.0), 1);
		assert_eq!(subsample(1500, f64::NAN), 3);
		assert_eq!(subsample(90_000, 0.0), 3);

		for fps in [10.0, 23.976, 24.0, 25.0, 30.0, 48.0, 50.0, 60.0, 100.0, 120.0, 144.0] {
			for seconds in [1.0, 5.0, 12.0, 14.0, 19.0, 26.0, 40.0, 61.0, 600.0, 7200.0] {
				let step = every(fps, seconds);
				#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
				let frames = (fps * seconds) as u64;

				assert_eq!(step % 2, 1, "{fps} fps, {seconds} s: {step}");
				assert!(
					frames.div_ceil(step) >= frames.min(MIN_SCORED_FRAMES),
					"{fps} fps, {seconds} s: {step}"
				);
			}
		}
	}

	#[test]
	fn a_miss_of_at_most_half_a_point_on_both_scores_is_near() {
		let target = Target {
			mean: 91.0,
			floor: 86.5,
		};
		let scored = |mean: f64, p5: f64| Score { mean, p5, frames: 300 };

		assert!(near_miss(scored(92.0, 86.2), target));
		assert!(near_miss(scored(90.5, 86.0), target));
		assert!(!near_miss(scored(92.0, 85.9), target));
		assert!(!near_miss(scored(90.4, 87.0), target));
		assert!(!near_miss(scored(f64::NAN, 87.0), target));
		assert_eq!(
			near_miss_text(28.5, scored(92.04, 86.21), target, None),
			"written slightly below the quality target: VMAF 92.0 / 86.2 at CRF 28.5, target 91.0 / 86.5 (within 0.5, so \
			 not encoded again)"
		);
		assert_eq!(
			near_miss_text(28.5, scored(92.04, 86.21), target, Some(12_500)),
			"written slightly below the quality target: VMAF 92.0 / 86.2 at CRF 28.5, target 91.0 / 86.5 (within 0.5) \
			 under the 12.5 Mbit/s cap"
		);
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
		sample_bytes: 900_000,
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
			dropped_grain_text(14, found(12.0, 600_000), found(29.0, 100_000), grain_target, None),
			"film grain 14 was left out: with it the samples needed CRF 12 and came out 6.0× the size; \
			 without it CRF 29 measured VMAF 93.4 / 89.2"
		);
		assert_eq!(
			dropped_grain_text(20, MISSED, found(29.0, 100_000), grain_target, None),
			"film grain 20 was left out: with it quality target not reached (VMAF 90.1 / 84.0 at CRF 10, target \
			 91.0 / 86.5); without it CRF 29 measured VMAF 93.4 / 89.2"
		);
		assert_eq!(
			dropped_grain_text(8, MISSED, MISSED, grain_target, None),
			"film grain 8 was left out: the quality target was reached neither with it nor without it"
		);
	}

	#[test]
	fn grain_dropped_under_a_bitrate_cap_names_the_cap() {
		let grain_target = target(Speed::Moderate, true, &Overrides::default());
		let cap = Some(12_500);

		assert_eq!(
			dropped_grain_text(20, MISSED, found(29.0, 100_000), grain_target, cap),
			"film grain 20 was left out: with it quality target not reached (VMAF 90.1 / 84.0 at CRF 10, target \
			 91.0 / 86.5) under the 12.5 Mbit/s cap; without it CRF 29 measured VMAF 93.4 / 89.2"
		);
		assert_eq!(
			dropped_grain_text(8, MISSED, MISSED, grain_target, cap),
			"film grain 8 was left out: the quality target was reached neither with it nor without it under the \
			 12.5 Mbit/s cap"
		);
		assert_eq!(
			dropped_grain_text(14, found(12.0, 600_000), found(29.0, 100_000), grain_target, cap),
			"film grain 14 was left out: with it the samples needed CRF 12 and came out 6.0× the size; \
			 without it CRF 29 measured VMAF 93.4 / 89.2"
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
