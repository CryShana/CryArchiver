use std::ops::Range;
use std::time::Duration;

pub use cryarchiver_engine::text::{fit, fit_middle, width};

pub fn thousands(n: u64) -> String {
	let digits = n.to_string();
	let mut out = String::with_capacity(digits.len() + digits.len() / 3);
	for (i, digit) in digits.chars().enumerate() {
		if i > 0 && (digits.len() - i).is_multiple_of(3) {
			out.push(',');
		}
		out.push(digit);
	}
	out
}

pub fn short_count(n: u64) -> String {
	match n {
		0..1_000 => n.to_string(),
		1_000..10_000 => format!("{:.1}k", as_f64(n) / 1000.0),
		_ => format!("{}k", n / 1000),
	}
}

pub fn bytes(n: u64) -> String {
	const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

	let mut value = as_f64(n);
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

/// Five columns wide. Floored, so 99.96 % shows as 99.9 % and only a complete item reads 100 %.
pub fn percent(fraction: f64, complete: bool) -> String {
	if complete {
		return " 100%".to_owned();
	}

	let tenths = floor_to_u32(fraction.clamp(0.0, 1.0) * 1000.0 + 1e-9).min(999);
	format!("{:>4}%", format!("{}.{}", tenths / 10, tenths % 10))
}

pub fn saving(bytes_in: u64, bytes_out: u64) -> String {
	if bytes_out >= bytes_in || bytes_in == 0 {
		return "0%".to_owned();
	}

	format!("−{}%", round_to_u32((1.0 - fraction(bytes_out, bytes_in)) * 100.0))
}

pub fn duration(value: Duration) -> String {
	let seconds = value.as_secs();
	match seconds {
		0..60 => format!("{seconds}s"),
		60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
		_ => format!("{}h {}m", seconds / 3600, seconds / 60 % 60),
	}
}

pub fn speed(multiplier: f64) -> String {
	if multiplier >= 10.0 {
		format!("{multiplier:.0}×")
	} else {
		format!("{multiplier:.1}×")
	}
}

pub fn fps(value: f64) -> String {
	format!("{value:.0} fps")
}

pub fn fraction(part: u64, whole: u64) -> f64 {
	if whole == 0 {
		return 0.0;
	}

	(as_f64(part) / as_f64(whole)).clamp(0.0, 1.0)
}

pub fn filled_cells(cells: usize, fraction: f64) -> usize {
	round_to_usize(fraction.clamp(0.0, 1.0) * as_f64(cells as u64)).min(cells)
}

pub fn filled_eighths(cells: usize, fraction: f64) -> usize {
	floor_to_usize(fraction.clamp(0.0, 1.0) * as_f64(cells as u64) * 8.0).min(cells * 8)
}

/// Byte range of the first case-insensitive match of `needle` in `haystack`.
pub fn find_ignore_case(haystack: &str, needle: &str) -> Option<Range<usize>> {
	if needle.is_empty() {
		return None;
	}

	let wanted: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
	haystack.char_indices().find_map(|(start, _)| {
		let mut pending = wanted.as_slice();
		let mut end = start;
		for (offset, c) in haystack[start..].char_indices() {
			if pending.is_empty() {
				break;
			}

			for lower in c.to_lowercase() {
				match pending.split_first() {
					Some((&expected, rest)) if expected == lower => pending = rest,
					_ => return None,
				}
			}
			end = start + offset + c.len_utf8();
		}

		pending.is_empty().then_some(start..end)
	})
}

#[allow(clippy::cast_precision_loss)]
pub fn as_f64(n: u64) -> f64 {
	n as f64
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn round_to_u32(value: f64) -> u32 {
	value.round().max(0.0) as u32
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn round_to_u64(value: f64) -> u64 {
	value.round().max(0.0) as u64
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn floor_to_u32(value: f64) -> u32 {
	value.floor().max(0.0) as u32
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn round_to_usize(value: f64) -> usize {
	value.round().max(0.0) as usize
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn floor_to_usize(value: f64) -> usize {
	value.floor().max(0.0) as usize
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn counts_read_like_the_mockup() {
		assert_eq!(thousands(48_213), "48,213");
		assert_eq!(thousands(999), "999");
		assert_eq!(thousands(1_234_567), "1,234,567");
		assert_eq!(short_count(84), "84");
		assert_eq!(short_count(6_742), "6.7k");
		assert_eq!(short_count(31_200), "31k");
	}

	#[test]
	fn bytes_use_si_units_with_three_significant_digits() {
		assert_eq!(bytes(0), "0 B");
		assert_eq!(bytes(2_100_000), "2.10 MB");
		assert_eq!(bytes(56_800_000), "56.8 MB");
		assert_eq!(bytes(612_000_000_000), "612 GB");
		assert_eq!(bytes(1_200_000_000_000), "1.20 TB");
	}

	#[test]
	fn percent_is_floored_and_only_complete_reads_100() {
		assert_eq!(percent(0.412, false), "41.2%");
		assert_eq!(percent(0.075, false), " 7.5%");
		assert_eq!(percent(0.9999, false), "99.9%");
		assert_eq!(percent(1.0, false), "99.9%");
		assert_eq!(percent(1.0, true), " 100%");
	}

	#[test]
	fn saving_uses_a_real_minus_sign() {
		assert_eq!(saving(100, 42), "−58%");
		assert_eq!(saving(100, 100), "0%");
		assert_eq!(saving(0, 0), "0%");
	}

	#[test]
	fn durations_keep_two_units() {
		assert_eq!(duration(Duration::from_secs(7)), "7s");
		assert_eq!(duration(Duration::from_secs(190)), "3m 10s");
		assert_eq!(duration(Duration::from_mins(9 * 60 + 12)), "9h 12m");
	}

	#[test]
	fn case_insensitive_find_returns_the_original_byte_range() {
		assert_eq!(find_ignore_case("IMG_0001 2.MOV", "mov"), Some(11..14));
		assert_eq!(find_ignore_case("01.テスト.mp3", "テスト"), Some(3..12));
		assert_eq!(find_ignore_case("abc", "abcd"), None);
		assert_eq!(find_ignore_case("abc", ""), None);
	}
}
