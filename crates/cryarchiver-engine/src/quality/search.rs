use crate::quality::vmaf::Score;

pub const MAX_PROBES: usize = 4;
pub const DEFAULT_SLOPE: f64 = 0.5;

const MAX_STEP: f64 = 8.0;
const CLOSE_ENOUGH: f64 = 0.5;
const DOWN_MARGIN: f64 = 0.5;
const MIN_SLOPE: f64 = 0.05;
const MAX_SLOPE: f64 = 5.0;

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
	let passing = highest_pass(probes, target);
	let failing = lowest_miss(probes, target, passing);

	let slope = slope(probes, target, passing, failing)
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
			let (low_crf, high_crf) = (probes[low].crf, probes[high].crf);
			if high_crf - low_crf <= bounds.step {
				return done;
			}

			let root = low_crf + margin(low) * (high_crf - low_crf) / (margin(low) - margin(high));
			bounds
				.quantize(root)
				.clamp(low_crf + bounds.step, high_crf - bounds.step)
		}
		(Some(low), None) => bounds.quantize(probes[low].crf + (margin(low) / slope).min(MAX_STEP)),
		(None, Some(high)) => bounds.quantize(probes[high].crf - (-margin(high) / slope + DOWN_MARGIN).min(MAX_STEP)),
		(None, None) => return done,
	};

	if probes
		.iter()
		.any(|probe| (probe.crf - candidate).abs() < bounds.step / 2.0)
	{
		return done;
	}

	Next::Probe(candidate)
}

pub fn retry_crf(crf: f64, score: Score, target: Target, slope: f64, bounds: Bounds) -> Option<f64> {
	let deficit = -target.margin(score);
	if deficit <= 0.0 {
		return None;
	}

	let lowered = bounds.quantize(crf - (deficit / slope.clamp(MIN_SLOPE, MAX_SLOPE) + DOWN_MARGIN));
	(lowered < crf).then_some(lowered)
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

fn slope(probes: &[Probe], target: Target, passing: Option<usize>, failing: Option<usize>) -> Option<f64> {
	let (a, b) = match (passing, failing) {
		(Some(low), Some(high)) => (low, high),
		_ if probes.len() >= 2 => (probes.len() - 2, probes.len() - 1),
		_ => return None,
	};

	let run = probes[b].crf - probes[a].crf;

	(run.abs() > f64::EPSILON).then(|| ((target.margin(probes[a].score) - target.margin(probes[b].score)) / run).abs())
}

#[cfg(test)]
mod tests {
	use super::*;

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
			match next(&probes, TARGET, SVT, DEFAULT_SLOPE) {
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

		let next = next(&probes, TARGET, SVT, DEFAULT_SLOPE);

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

		assert!(matches!(next(&first, TARGET, SVT, DEFAULT_SLOPE), Next::Probe(crf) if (crf - 37.0).abs() < 1e-9));
		assert_eq!(
			next(&at_the_top, TARGET, SVT, DEFAULT_SLOPE),
			Next::Done(Choice::Chosen {
				index: 0,
				slope: DEFAULT_SLOPE
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

		let next = next(&noisy, TARGET, SVT, DEFAULT_SLOPE);

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

		assert_eq!(retry_crf(30.0, missed, TARGET, 0.5, SVT), Some(26.5));
		assert_eq!(retry_crf(30.0, missed, TARGET, 0.25, SVT), Some(23.5));
		assert_eq!(retry_crf(30.0, linear(10.0), TARGET, 0.5, SVT), None);
		assert_eq!(retry_crf(10.0, missed, TARGET, 0.5, SVT), None);
	}
}
