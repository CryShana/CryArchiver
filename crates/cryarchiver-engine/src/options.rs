use crate::format::OutputFormat;

const DURATION_SLACK_S: f64 = 0.05;
const SIXTY: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trim {
	pub start_s: f64,
	pub end_s: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrimWindow {
	pub start_s: f64,
	pub length_s: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobOptions {
	pub trim: Option<Trim>,
	pub format: Option<OutputFormat>,
	pub strip: bool,
}

impl JobOptions {
	pub fn forced(&self) -> bool {
		self.trim.is_some() || self.format.is_some() || self.strip
	}
}

impl TrimWindow {
	pub fn shortfall(self, decoded_s: f64) -> Option<String> {
		(self.length_s - decoded_s > (self.length_s * 0.02).max(1.0)).then(|| {
			format!(
				"the trimmed range decodes to {decoded_s:.1} s but should last {:.1} s",
				self.length_s
			)
		})
	}
}

impl Trim {
	pub fn window(self, duration_s: Option<f64>) -> Result<TrimWindow, String> {
		if let Some(end_s) = self.end_s
			&& end_s <= self.start_s
		{
			return Err(format!(
				"--to ({end_s:.3} s) must be later than --ss ({:.3} s)",
				self.start_s
			));
		}

		let Some(duration_s) = duration_s else {
			return self
				.end_s
				.map(|end_s| TrimWindow {
					start_s: self.start_s,
					length_s: end_s - self.start_s,
				})
				.ok_or_else(|| "the file's duration is unknown, so --to is needed".to_owned());
		};

		if self.start_s >= duration_s {
			return Err(format!(
				"--ss ({:.3} s) is past the end of the file ({duration_s:.3} s)",
				self.start_s
			));
		}

		let end_s = self.end_s.unwrap_or(duration_s);
		if end_s > duration_s + DURATION_SLACK_S {
			return Err(format!(
				"--to ({end_s:.3} s) is past the end of the file ({duration_s:.3} s)"
			));
		}
		Ok(TrimWindow {
			start_s: self.start_s,
			length_s: end_s.min(duration_s) - self.start_s,
		})
	}
}

pub fn parse_time(text: &str) -> Result<f64, String> {
	let invalid = || {
		format!(
			"{text:?} is not a time: use seconds (90), minutes:seconds (1:30) or hours:minutes:seconds (00:01:30.500)"
		)
	};
	let parts: Vec<&str> = text.trim().split(':').collect();
	let Some((last, leading)) = parts.split_last().filter(|_| parts.len() <= 3) else {
		return Err(invalid());
	};

	let seconds = decimal(last).ok_or_else(invalid)?;
	if !leading.is_empty() && seconds >= SIXTY {
		return Err(invalid());
	}

	let mut total = seconds;
	let mut unit = SIXTY;
	for (position, part) in leading.iter().rev().enumerate() {
		let value: u32 = part
			.parse()
			.ok()
			.filter(|_| part.bytes().all(|byte| byte.is_ascii_digit()))
			.ok_or_else(invalid)?;
		if position == 0 && leading.len() == 2 && f64::from(value) >= SIXTY {
			return Err(invalid());
		}

		total += f64::from(value) * unit;
		unit *= SIXTY;
	}
	Ok(total)
}

pub fn kept_as_requested(src_bytes: u64, out_bytes: u64) -> String {
	format!(
		"the output is not smaller than the source ({src_bytes} → {out_bytes} bytes); it was written because it was requested"
	)
}

fn decimal(text: &str) -> Option<f64> {
	let digits = text.bytes().filter(u8::is_ascii_digit).count();
	let points = text.bytes().filter(|byte| *byte == b'.').count();
	(digits > 0 && points <= 1 && digits + points == text.len())
		.then(|| text.parse().ok())
		.flatten()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn trim(start_s: f64, end_s: Option<f64>) -> Trim {
		Trim { start_s, end_s }
	}

	#[test]
	fn times_are_seconds_minutes_or_hours() {
		assert_eq!(parse_time("90"), Ok(90.0));
		assert_eq!(parse_time("1:30"), Ok(90.0));
		assert_eq!(parse_time("00:01:30.500"), Ok(90.5));
		assert_eq!(parse_time("90:00"), Ok(5400.0));
		assert_eq!(parse_time(" 2.25 "), Ok(2.25));
		for bad in [
			"", "-5", "+5", "1:75", "1:60:00", "1:2:3:4", "a", "1.2.3", ":30", "1e3", ".",
		] {
			assert!(parse_time(bad).is_err(), "{bad}");
		}
	}

	#[test]
	fn windows_lie_inside_the_file() {
		assert_eq!(
			trim(10.0, Some(25.0)).window(Some(60.0)),
			Ok(TrimWindow {
				start_s: 10.0,
				length_s: 15.0
			})
		);
		assert_eq!(
			trim(10.0, None).window(Some(60.0)),
			Ok(TrimWindow {
				start_s: 10.0,
				length_s: 50.0
			})
		);
		assert_eq!(
			trim(10.0, Some(60.02)).window(Some(60.0)),
			Ok(TrimWindow {
				start_s: 10.0,
				length_s: 50.0
			})
		);
		assert_eq!(
			trim(5.0, Some(8.0)).window(None),
			Ok(TrimWindow {
				start_s: 5.0,
				length_s: 3.0
			})
		);
		assert!(
			trim(10.0, Some(10.0))
				.window(Some(60.0))
				.unwrap_err()
				.contains("later than")
		);
		assert!(
			trim(61.0, None)
				.window(Some(60.0))
				.unwrap_err()
				.contains("past the end")
		);
		assert!(
			trim(0.0, Some(61.0))
				.window(Some(60.0))
				.unwrap_err()
				.contains("past the end")
		);
		assert!(trim(5.0, None).window(None).unwrap_err().contains("--to is needed"));
	}

	#[test]
	fn a_window_that_decodes_short_is_damage() {
		let window = TrimWindow {
			start_s: 10.0,
			length_s: 30.0,
		};

		assert_eq!(window.shortfall(29.5), None);
		assert!(window.shortfall(20.0).unwrap().contains("20.0 s"));
	}

	#[test]
	fn only_transformations_force_an_output() {
		assert!(!JobOptions::default().forced());
		assert!(
			JobOptions {
				trim: Some(trim(1.0, None)),
				..JobOptions::default()
			}
			.forced()
		);
	}
}
