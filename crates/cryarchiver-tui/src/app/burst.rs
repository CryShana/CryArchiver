use std::time::{Duration, Instant};

/// Keys closer together than this came from a paste, not from fingers. Key repeat stays well above it.
pub const BURST_GAP: Duration = Duration::from_millis(5);

/// Marks each key press that arrived within [`BURST_GAP`] of its neighbour. The Windows console delivers a paste as
/// plain key presses, and this is how the TUI tells it apart from typing. `previous` is the last key press before
/// these, from an earlier batch.
pub fn burst_flags(times: &[Instant], previous: Option<Instant>) -> Vec<bool> {
	let close = |earlier: Instant, later: Instant| later.saturating_duration_since(earlier) < BURST_GAP;

	(0..times.len())
		.map(|index| {
			let before = if index == 0 { previous } else { Some(times[index - 1]) };
			let after_close = times.get(index + 1).is_some_and(|&next| close(times[index], next));
			let before_close = before.is_some_and(|earlier| close(earlier, times[index]));
			before_close || after_close
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(start: Instant, millis: &[u64]) -> Vec<Instant> {
		millis.iter().map(|&ms| start + Duration::from_millis(ms)).collect()
	}

	#[test]
	fn typing_is_never_a_burst() {
		let start = Instant::now();

		assert_eq!(burst_flags(&at(start, &[0, 60, 130, 200]), None), [false; 4]);
	}

	#[test]
	fn a_paste_is_a_burst_from_its_first_key() {
		let start = Instant::now();

		assert_eq!(burst_flags(&at(start, &[0, 1, 2, 3]), None), [true; 4]);
	}

	#[test]
	fn a_paste_after_typing_marks_only_the_pasted_keys() {
		let start = Instant::now();

		assert_eq!(
			burst_flags(&at(start, &[0, 100, 101, 102, 300]), None),
			[false, true, true, true, false]
		);
	}

	#[test]
	fn a_burst_continues_across_batches() {
		let start = Instant::now();
		let previous = Some(start);

		assert_eq!(burst_flags(&at(start, &[2]), previous), [true]);
		assert_eq!(burst_flags(&at(start, &[50]), previous), [false]);
	}

	#[test]
	fn a_single_key_alone_is_not_a_burst() {
		assert_eq!(burst_flags(&[Instant::now()], None), [false]);
	}
}
