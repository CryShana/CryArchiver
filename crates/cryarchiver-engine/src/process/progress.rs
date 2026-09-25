use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
	pub out_time: Option<Duration>,
	pub frame: Option<u64>,
	pub done: bool,
}

#[derive(Default)]
pub struct ProgressParser {
	current: Progress,
}

impl ProgressParser {
	pub fn line(&mut self, line: &str) -> Option<Progress> {
		let (key, value) = line.split_once('=')?;
		let value = value.trim();

		match key.trim() {
			"out_time_us" => self.current.out_time = value.parse().ok().map(Duration::from_micros),
			"frame" => self.current.frame = value.parse().ok(),
			"progress" => {
				self.current.done = value == "end";
				return Some(std::mem::take(&mut self.current));
			}
			_ => {}
		}
		None
	}
}

#[derive(Default)]
pub struct HighWater {
	out_time: Option<Duration>,
	frame: Option<u64>,
}

impl HighWater {
	pub fn advance(&mut self, progress: &Progress) -> bool {
		let advanced = progress.out_time > self.out_time || progress.frame > self.frame;
		self.out_time = self.out_time.max(progress.out_time);
		self.frame = self.frame.max(progress.frame);
		advanced
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn emits_one_progress_per_block() {
		let mut parser = ProgressParser::default();
		let block = [
			"frame=120",
			"fps=59.9",
			"out_time_us=2000000",
			"speed=1.5x",
			"progress=continue",
		];

		let emitted: Vec<Progress> = block.iter().filter_map(|line| parser.line(line)).collect();

		assert_eq!(
			emitted,
			vec![Progress {
				out_time: Some(Duration::from_secs(2)),
				frame: Some(120),
				done: false
			}]
		);
	}

	#[test]
	fn unknown_values_and_end_marker() {
		let mut parser = ProgressParser::default();

		assert_eq!(parser.line("out_time_us=N/A"), None);
		let end = parser.line("progress=end").unwrap();

		assert_eq!(end.out_time, None);
		assert!(end.done);
	}

	#[test]
	fn high_water_only_advances_on_a_new_maximum() {
		let mut high_water = HighWater::default();
		let first = Progress {
			out_time: Some(Duration::from_secs(1)),
			frame: Some(10),
			done: false,
		};

		assert!(high_water.advance(&first));
		assert!(!high_water.advance(&first));

		let regressed = Progress {
			out_time: None,
			frame: None,
			done: false,
		};
		assert!(!high_water.advance(&regressed));

		let later_time = Progress {
			out_time: Some(Duration::from_secs(2)),
			frame: Some(10),
			done: false,
		};
		assert!(high_water.advance(&later_time));

		let later_frame = Progress {
			out_time: Some(Duration::from_secs(2)),
			frame: Some(20),
			done: false,
		};
		assert!(high_water.advance(&later_frame));
	}
}
