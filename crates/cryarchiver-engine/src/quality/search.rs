use crate::quality::vmaf::Score;

pub const MAX_PROBES: usize = 4;

const MAX_STEP: f64 = 8.0;
const CLOSE_ENOUGH: f64 = 0.5;
const DOWN_MARGIN: f64 = 0.5;
const MIN_SLOPE: f64 = 0.05;
const MAX_SLOPE: f64 = 5.0;
/// How far above the target a retry aims: half a point, as far as a check may miss and still be written.
const RETRY_MARGIN: f64 = 0.5;
const RETRY_SLOPE_TRUST: f64 = 0.75;
const MIN_RETRY_DROP: f64 = 2.0;
/// How far below its lowest probe a retry may follow the samples' curve, with no probe there to show its shape.
const MAX_EXTRAPOLATION: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
	pub mean: f64,
	pub floor: f64,
}

impl Target {
	pub fn margin(self, score: Score) -> f64 {
		(score.mean - self.mean).min(score.p5 - self.floor)
	}

	pub fn met(self, score: Score) -> bool {
		self.margin(score) >= 0.0
	}
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
	pub min: f64,
	pub max: f64,
	pub step: f64,
}

impl Bounds {
	pub fn quantize(self, crf: f64) -> f64 {
		((crf / self.step).floor() * self.step).clamp(self.min, self.max)
	}
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
	pub crf: f64,
	pub score: Score,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Choice {
	Chosen { index: usize, slope: f64 },
	Unreachable { index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Next {
	Probe(f64),
	Done(Choice),
}

pub fn next(probes: &[Probe], target: Target, bounds: Bounds, prior_slope: f64) -> Next {
	let margin = |index: usize| target.margin(probes[index].score);
	let crf = |index: usize| bounds.quantize(probes[index].crf);
	let passing = highest_pass(probes, target);
	let failing = lowest_miss(probes, target, passing);

	let slope = slope(probes, target, bounds, passing, failing)
		.unwrap_or(prior_slope)
		.clamp(MIN_SLOPE, MAX_SLOPE);
	let done = Next::Done(match passing {
		Some(index) => Choice::Chosen { index, slope },
		None => Choice::Unreachable {
			index: failing.unwrap_or(probes.len() - 1),
		},
	});

	if passing.is_some_and(|index| margin(index) <= CLOSE_ENOUGH) || probes.len() >= MAX_PROBES {
		return done;
	}

	let candidate = match (passing, failing) {
		(Some(low), Some(high)) => {
			let (low_crf, high_crf) = (crf(low), crf(high));
			if high_crf - low_crf <= bounds.step {
				return done;
			}

			let root = low_crf + margin(low) * (high_crf - low_crf) / (margin(low) - margin(high));
			let (lo, hi) = (low_crf + bounds.step, high_crf - bounds.step);
			bounds.quantize(root).clamp(lo.min(hi), lo.max(hi))
		}
		(Some(low), None) => bounds.quantize(crf(low) + (margin(low) / slope).min(MAX_STEP)),
		(None, Some(high)) => bounds.quantize(crf(high) - (-margin(high) / slope + DOWN_MARGIN).min(MAX_STEP)),
		(None, None) => return done,
	};

	if (0..probes.len()).any(|index| (crf(index) - candidate).abs() < bounds.step / 2.0) {
		return done;
	}

	Next::Probe(candidate)
}

/// The CRF a missed full check is encoded again at. The retry is the last full encode, so it aims `RETRY_MARGIN` above
/// the target on a slope it trusts only so far, and drops at least `MIN_RETRY_DROP`: the full check already landed
/// away from the samples once, and a CRF next to the one that missed would miss again.
pub fn retry_crf(crf: f64, score: Score, target: Target, slope: f64, bounds: Bounds) -> Option<f64> {
	let crf = bounds.quantize(crf);
	let deficit = -target.margin(score);
	if deficit <= 0.0 {
		return None;
	}

	let trusted = slope.clamp(MIN_SLOPE, MAX_SLOPE) * RETRY_SLOPE_TRUST;
	let drop = ((deficit + RETRY_MARGIN) / trusted).clamp(MIN_RETRY_DROP, MAX_STEP);
	let lowered = bounds.quantize(crf - drop);
	(lowered < crf).then_some(lowered)
}

/// The CRF a missed full check is encoded again at, aimed on the search's own probe curve: the probes below the CRF
/// that missed, each moved by how far the full check landed from its samples (`full` against `predicted`), with the
/// check itself on top. It is where that curve reaches the target plus `RETRY_MARGIN`, between the two points around
/// it, or, when no point reaches it, on the line through the lowest two, but never more than `MAX_EXTRAPOLATION` below
/// the lowest probe. It drops at least `MIN_RETRY_DROP` and at most `MAX_STEP`, like `retry_crf`. `None` for a check
/// that didn't miss or with no probe below the CRF that missed, where `retry_crf` aims on the slope instead.
pub fn curve_retry_crf(
	crf: f64,
	full: Score,
	predicted: Score,
	probes: &[Probe],
	target: Target,
	bounds: Bounds,
) -> Option<f64> {
	let crf = bounds.quantize(crf);
	if target.met(full) {
		return None;
	}

	let aim = Target {
		mean: target.mean + RETRY_MARGIN,
		floor: target.floor + RETRY_MARGIN,
	};
	let shifted = |score: Score| Score {
		mean: score.mean + full.mean - predicted.mean,
		p5: score.p5 + full.p5 - predicted.p5,
		..score
	};
	let mut points: Vec<(f64, f64)> = probes
		.iter()
		.map(|probe| (bounds.quantize(probe.crf), aim.margin(shifted(probe.score))))
		.filter(|(probe_crf, _)| *probe_crf < crf)
		.collect();
	points.sort_by(|a, b| b.0.total_cmp(&a.0));
	points.dedup_by(|a, b| (a.0 - b.0).abs() < f64::EPSILON);
	let lowest = points.last()?.0;
	points.insert(0, (crf, aim.margin(full)));

	let reached = points.windows(2).find_map(|pair| {
		let [(high_crf, high_margin), (low_crf, low_margin)] = [pair[0], pair[1]];
		(low_margin >= 0.0).then(|| low_crf + low_margin * (high_crf - low_crf) / (low_margin - high_margin))
	});
	let aimed = reached.unwrap_or_else(|| {
		let [(high_crf, high_margin), (low_crf, low_margin)] = [points[points.len() - 2], points[points.len() - 1]];
		let gain = (low_margin - high_margin) / (high_crf - low_crf);
		if gain > 0.0 {
			low_crf + low_margin / gain
		} else {
			f64::NEG_INFINITY
		}
	});

	let lowered = bounds.quantize(
		aimed
			.max(lowest - MAX_EXTRAPOLATION)
			.max(crf - MAX_STEP)
			.min(crf - MIN_RETRY_DROP),
	);
	(lowered < crf).then_some(lowered)
}

/// The CRF a passing score points to: the one used, raised by the margin beyond the search's own tolerance over the
/// slope, by one full step at most. Aiming at the tolerance rather than at the target itself keeps the next file of
/// the group from landing on the edge, where half of them would miss: written below the target, or encoded again.
pub fn implied_crf(crf: f64, score: Score, target: Target, slope: f64, bounds: Bounds) -> f64 {
	let spare = (target.margin(score) - CLOSE_ENOUGH).max(0.0);
	let step = (spare / slope.clamp(MIN_SLOPE, MAX_SLOPE)).min(MAX_STEP);
	(crf + step).clamp(bounds.min, bounds.max)
}

fn highest_pass(probes: &[Probe], target: Target) -> Option<usize> {
	probes
		.iter()
		.enumerate()
		.filter(|(_, probe)| target.met(probe.score))
		.max_by(|(_, a), (_, b)| a.crf.total_cmp(&b.crf))
		.map(|(index, _)| index)
}

fn lowest_miss(probes: &[Probe], target: Target, passing: Option<usize>) -> Option<usize> {
	let above = passing.map_or(f64::NEG_INFINITY, |index| probes[index].crf);

	probes
		.iter()
		.enumerate()
		.filter(|(_, probe)| !target.met(probe.score) && probe.crf > above)
		.min_by(|(_, a), (_, b)| a.crf.total_cmp(&b.crf))
		.map(|(index, _)| index)
}

fn slope(
	probes: &[Probe],
	target: Target,
	bounds: Bounds,
	passing: Option<usize>,
	failing: Option<usize>,
) -> Option<f64> {
	let (a, b) = match (passing, failing) {
		(Some(low), Some(high)) => (low, high),
		_ if probes.len() >= 2 => (probes.len() - 2, probes.len() - 1),
		_ => return None,
	};

	let run = bounds.quantize(probes[b].crf) - bounds.quantize(probes[a].crf);

	(run.abs() > f64::EPSILON).then(|| ((target.margin(probes[a].score) - target.margin(probes[b].score)) / run).abs())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::calibration::tables;

	const TARGET: Target = Target {
		mean: 95.0,
		floor: 90.0,
	};
	const SVT: Bounds = Bounds {
		min: 10.0,
		max: 55.0,
		step: 0.25,
	};

	fn linear(crf: f64) -> Score {
		let mean = 100.0 - 0.3 * crf;
		Score {
			mean,
			p5: mean - 4.0,
			frames: 100,
		}
	}

	fn hopeless(crf: f64) -> Score {
		let mean = 80.0 - 0.3 * crf;
		Score {
			mean,
			p5: mean - 4.0,
			frames: 100,
		}
	}

	fn run(start: f64, curve: impl Fn(f64) -> Score) -> (Choice, Vec<f64>) {
		let mut probes = Vec::new();
		let mut crf = SVT.quantize(start);
		loop {
			probes.push(Probe { crf, score: curve(crf) });
			match next(&probes, TARGET, SVT, tables().video.default_slope) {
				Next::Probe(next) => crf = next,
				Next::Done(choice) => return (choice, probes.iter().map(|probe| probe.crf).collect()),
			}
		}
	}

	#[test]
	fn a_start_above_the_target_steps_down_and_stops_on_a_close_pass() {
		let (choice, crfs) = run(29.0, linear);

		assert_eq!(crfs, [29.0, 21.0, 16.0]);
		assert!(
			matches!(choice, Choice::Chosen { index: 2, slope } if (slope - 0.3).abs() < 1e-9),
			"{choice:?}"
		);
	}

	#[test]
	fn a_start_below_the_target_steps_up_to_the_highest_close_pass() {
		let (choice, crfs) = run(10.0, linear);

		assert_eq!(crfs, [10.0, 14.0, 16.5]);
		assert!(matches!(choice, Choice::Chosen { index: 2, .. }), "{choice:?}");
	}

	#[test]
	fn an_unreachable_target_stops_at_the_lowest_crf() {
		let (choice, crfs) = run(29.0, hopeless);

		assert_eq!(crfs, [29.0, 21.0, 13.0, 10.0]);
		assert_eq!(choice, Choice::Unreachable { index: 3 });
	}

	#[test]
	fn a_bracket_is_split_at_the_secant_root() {
		let probes = [
			Probe {
				crf: 14.0,
				score: linear(14.0),
			},
			Probe {
				crf: 21.0,
				score: linear(21.0),
			},
		];

		let next = next(&probes, TARGET, SVT, tables().video.default_slope);

		assert!(
			matches!(next, Next::Probe(crf) if (crf - 16.5).abs() < 1e-9),
			"{next:?}"
		);
	}

	#[test]
	fn steps_are_clamped_and_never_repeat_a_probe() {
		let flat = |crf: f64| {
			let mean = 100.0 - 0.01 * crf;
			Score {
				mean,
				p5: mean - 4.0,
				frames: 100,
			}
		};
		let first = [Probe {
			crf: 29.0,
			score: flat(29.0),
		}];
		let at_the_top = [Probe {
			crf: 55.0,
			score: flat(55.0),
		}];

		assert!(
			matches!(next(&first, TARGET, SVT, tables().video.default_slope), Next::Probe(crf) if (crf - 37.0).abs() < 1e-9)
		);
		assert_eq!(
			next(&at_the_top, TARGET, SVT, tables().video.default_slope),
			Next::Done(Choice::Chosen {
				index: 0,
				slope: tables().video.default_slope
			})
		);
	}

	#[test]
	fn a_miss_below_a_pass_does_not_form_a_bracket() {
		let noisy = [
			Probe {
				crf: 20.0,
				score: Score {
					mean: 94.0,
					p5: 91.0,
					frames: 100,
				},
			},
			Probe {
				crf: 22.0,
				score: Score {
					mean: 96.0,
					p5: 92.0,
					frames: 100,
				},
			},
		];

		let next = next(&noisy, TARGET, SVT, tables().video.default_slope);

		assert!(matches!(next, Next::Probe(crf) if crf > 22.0), "{next:?}");
	}

	#[test]
	fn crfs_round_down_to_the_step_inside_the_bounds() {
		let whole = Bounds { step: 1.0, ..SVT };

		assert!((SVT.quantize(29.13) - 29.0).abs() < 1e-9);
		assert!((SVT.quantize(70.0) - 55.0).abs() < 1e-9);
		assert!((SVT.quantize(3.0) - 10.0).abs() < 1e-9);
		assert!((whole.quantize(26.9) - 26.0).abs() < 1e-9);
	}

	#[test]
	fn a_retry_lowers_the_crf_by_the_deficit_over_the_slope() {
		let missed = Score {
			mean: 93.5,
			p5: 89.0,
			frames: 300,
		};

		assert_eq!(retry_crf(30.0, missed, TARGET, 0.5, SVT), Some(24.5));
		assert_eq!(retry_crf(30.0, missed, TARGET, 0.25, SVT), Some(22.0));
		assert_eq!(retry_crf(30.0, linear(10.0), TARGET, 0.5, SVT), None);
		assert_eq!(retry_crf(10.0, missed, TARGET, 0.5, SVT), None);
	}

	#[test]
	fn a_retry_aims_above_the_target_and_never_lands_next_to_the_crf_that_missed() {
		let missed_by = |deficit: f64| Score {
			mean: TARGET.mean + 2.0,
			p5: TARGET.floor - deficit,
			frames: 300,
		};

		assert_eq!(retry_crf(30.0, missed_by(0.3), TARGET, 0.5, SVT), Some(27.75));
		assert_eq!(retry_crf(30.0, missed_by(0.1), TARGET, 3.0, SVT), Some(28.0));
		assert_eq!(retry_crf(30.0, missed_by(0.1), TARGET, 5.0, SVT), Some(28.0));
		assert_eq!(retry_crf(11.0, missed_by(0.3), TARGET, 0.5, SVT), Some(10.0));
	}

	#[test]
	fn a_retry_on_a_flat_curve_drops_one_full_step_at_most() {
		let missed_by_1 = Score {
			mean: TARGET.mean - 1.0,
			p5: TARGET.floor + 1.0,
			frames: 300,
		};

		assert_eq!(retry_crf(30.0, missed_by_1, TARGET, 0.05, SVT), Some(22.0));
		assert_eq!(retry_crf(30.0, missed_by_1, TARGET, 0.01, SVT), Some(22.0));
	}

	const GRAIN_TARGET: Target = Target {
		mean: 91.0,
		floor: 86.5,
	};

	/// Samples losing 0.35 mean and 0.4 p5 a CRF from 92.3 / 88.1 at CRF 36.75, as on a home DVD clip.
	fn samples(crf: f64) -> Score {
		Score {
			mean: 92.3 + 0.35 * (36.75 - crf),
			p5: 88.1 + 0.4 * (36.75 - crf),
			frames: 300,
		}
	}

	fn probed(crfs: &[f64], curve: impl Fn(f64) -> Score) -> Vec<Probe> {
		crfs.iter().map(|&crf| Probe { crf, score: curve(crf) }).collect()
	}

	fn score(mean: f64, p5: f64) -> Score {
		Score { mean, p5, frames: 300 }
	}

	#[test]
	fn a_retry_aims_on_the_probe_curve_moved_by_the_full_checks_offset() {
		let probes = probed(&[28.0, 36.0, 37.0, 36.75], samples);
		let full = score(92.0, 85.4);
		let passing = 33.75;

		let on_curve = curve_retry_crf(36.75, full, samples(36.75), &probes, GRAIN_TARGET, SVT).unwrap();
		let on_slope = retry_crf(36.75, full, GRAIN_TARGET, 0.4, SVT).unwrap();

		assert!((on_curve - 32.75).abs() < 1e-9, "{on_curve}");
		assert!(
			(0.0..=1.5).contains(&(passing - on_curve)),
			"{on_curve} against {passing}"
		);
		assert!((on_slope - 31.25).abs() < 1e-9, "the slope rule overshoots: {on_slope}");
	}

	/// 92 / 87 at CRF 30, 0.6 more a CRF lower.
	fn falling(crf: f64) -> Score {
		score(110.0 - 0.6 * crf, 105.0 - 0.6 * crf)
	}

	#[test]
	fn a_retry_takes_the_nearest_point_of_the_curve_that_reaches_the_aim() {
		let probes = probed(&[20.0, 26.0, 30.0], falling);
		let full = score(91.0, 86.0);

		let retried = curve_retry_crf(30.0, full, falling(30.0), &probes, TARGET, SVT);

		assert_eq!(retried, Some(22.5), "between the probes at 20 and 26");
	}

	#[test]
	fn a_retry_on_the_curve_drops_one_full_step_at_most() {
		let probes = probed(&[10.0, 30.0], falling);
		let far_below = score(86.0, 81.0);

		assert_eq!(
			curve_retry_crf(30.0, far_below, falling(30.0), &probes, TARGET, SVT),
			Some(22.0),
			"the curve reaches the aim near 12.7"
		);
	}

	#[test]
	fn a_retry_follows_the_curve_at_most_a_little_below_its_lowest_probe() {
		let far_below = score(86.0, 81.0);
		let flat = probed(&[26.0, 30.0], |_| falling(30.0));

		assert_eq!(
			curve_retry_crf(
				30.0,
				far_below,
				falling(30.0),
				&probed(&[26.0, 30.0], falling),
				TARGET,
				SVT
			),
			Some(24.0)
		);
		assert_eq!(
			curve_retry_crf(30.0, far_below, falling(30.0), &flat, TARGET, SVT),
			Some(24.0)
		);
	}

	#[test]
	fn a_retry_on_the_curve_still_drops_the_minimum_and_needs_a_probe_below() {
		let steep = |crf: f64| score(185.0 - 3.0 * crf, 181.0 - 3.0 * crf);
		let barely = score(94.8, 92.0);

		assert_eq!(
			curve_retry_crf(30.0, barely, steep(30.0), &probed(&[29.5, 30.0], steep), TARGET, SVT),
			Some(28.0)
		);
		assert_eq!(
			curve_retry_crf(30.0, barely, steep(30.0), &probed(&[30.0, 34.0], steep), TARGET, SVT),
			None,
			"a search that only went up falls back on the slope, as a learned setting does"
		);
		assert_eq!(
			curve_retry_crf(
				30.0,
				score(96.0, 91.0),
				steep(30.0),
				&probed(&[26.0, 30.0], steep),
				TARGET,
				SVT
			),
			None,
			"no miss"
		);
	}

	#[test]
	fn a_measured_margin_moves_the_crf_it_implies() {
		let passing_by = |margin: f64| Score {
			mean: TARGET.mean + margin,
			p5: TARGET.floor + margin,
			frames: 300,
		};

		assert!((implied_crf(20.0, passing_by(3.0), TARGET, 0.5, SVT) - 25.0).abs() < 1e-9);
		assert!((implied_crf(20.0, passing_by(10.0), TARGET, 0.5, SVT) - 28.0).abs() < 1e-9);
		assert!((implied_crf(20.0, passing_by(10.0), TARGET, 0.05, SVT) - 28.0).abs() < 1e-9);
		assert!((implied_crf(20.0, passing_by(0.3), TARGET, 0.5, SVT) - 20.0).abs() < 1e-9);
		assert!((implied_crf(20.0, passing_by(0.0), TARGET, 0.5, SVT) - 20.0).abs() < 1e-9);
		assert!((implied_crf(50.0, passing_by(10.0), TARGET, 0.5, SVT) - 55.0).abs() < 1e-9);
	}

	fn is_on_grid(crf: f64, bounds: Bounds) -> bool {
		let steps = (crf - bounds.min) / bounds.step;
		(steps - steps.round()).abs() < 1e-6
	}

	#[test]
	fn a_narrow_off_grid_bracket_does_not_panic() {
		let probes = [
			Probe {
				crf: 16.6,
				score: Score {
					mean: 95.6,
					p5: 90.6,
					frames: 100,
				},
			},
			Probe {
				crf: 17.0,
				score: Score {
					mean: 94.9,
					p5: 89.9,
					frames: 100,
				},
			},
		];

		match next(&probes, TARGET, SVT, tables().video.default_slope) {
			Next::Probe(crf) => assert!(is_on_grid(crf, SVT), "{crf} is off the grid"),
			Next::Done(_) => {}
		}
	}

	#[test]
	fn an_off_grid_bracket_on_both_sides_returns_a_grid_aligned_candidate() {
		let probes = [
			Probe {
				crf: 16.6,
				score: Score {
					mean: 95.6,
					p5: 90.6,
					frames: 100,
				},
			},
			Probe {
				crf: 17.4,
				score: Score {
					mean: 94.9,
					p5: 89.9,
					frames: 100,
				},
			},
		];

		let next = next(&probes, TARGET, SVT, tables().video.default_slope);

		assert!(
			matches!(next, Next::Probe(crf) if (crf - 17.0).abs() < 1e-9),
			"{next:?}"
		);
	}

	#[test]
	fn an_unaligned_single_probe_seed_steps_from_its_grid_crf() {
		let passing_by_2_1 = Score {
			mean: TARGET.mean + 2.1,
			p5: TARGET.floor + 2.1,
			frames: 100,
		};
		let probes = [Probe {
			crf: 29.2,
			score: passing_by_2_1,
		}];

		let next = next(&probes, TARGET, SVT, tables().video.default_slope);

		assert!(
			matches!(next, Next::Probe(crf) if (crf - 33.0).abs() < 1e-9 && is_on_grid(crf, SVT)),
			"{next:?}"
		);
	}
}
