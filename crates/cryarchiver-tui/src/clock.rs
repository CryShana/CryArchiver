//! Local wall-clock time for the Log. The project stamps its log lines in UTC seconds since midnight.

const DAY: i64 = 86_400;

/// The local time of day, in seconds since midnight, of `utc` seconds since midnight UTC in a zone `offset` seconds
/// east of UTC.
pub fn local_seconds(utc: u32, offset: i32) -> u32 {
	let local = (i64::from(utc) + i64::from(offset)).rem_euclid(DAY);
	u32::try_from(local).unwrap_or(0)
}

/// How many seconds the local zone is east of UTC right now, daylight saving included. 0 when the OS can't say.
#[cfg(windows)]
pub fn utc_offset() -> i32 {
	use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_ID_INVALID, TIME_ZONE_INFORMATION};

	const TIME_ZONE_ID_DAYLIGHT: u32 = 2;

	let mut zone = TIME_ZONE_INFORMATION::default();

	// SAFETY: `zone` is a valid, writable TIME_ZONE_INFORMATION for the call.
	let bias = match unsafe { GetTimeZoneInformation(&raw mut zone) } {
		TIME_ZONE_ID_INVALID => return 0,
		TIME_ZONE_ID_DAYLIGHT => zone.Bias + zone.DaylightBias,
		_ => zone.Bias + zone.StandardBias,
	};
	-bias * 60
}

/// How many seconds the local zone is east of UTC right now, daylight saving included. 0 when the OS can't say.
#[cfg(unix)]
pub fn utc_offset() -> i32 {
	// SAFETY: `time` accepts a null pointer and only returns the current time.
	let now = unsafe { libc::time(std::ptr::null_mut()) };

	// SAFETY: an all-zero `tm` is a valid value.
	let mut local: libc::tm = unsafe { std::mem::zeroed() };

	// SAFETY: `localtime_r` reads only `now` and writes only into `local`.
	if unsafe { libc::localtime_r(&raw const now, &raw mut local) }.is_null() {
		return 0;
	}
	i32::try_from(local.tm_gmtoff).unwrap_or(0)
}

#[cfg(not(any(windows, unix)))]
pub fn utc_offset() -> i32 {
	0
}

#[cfg(test)]
mod tests {
	use super::*;

	const HOUR: i32 = 3600;

	fn clock(hours: u32, minutes: u32) -> u32 {
		hours * 3600 + minutes * 60
	}

	#[test]
	fn an_evening_event_shows_at_its_local_hour() {
		assert_eq!(local_seconds(clock(19, 42), 2 * HOUR), clock(21, 42));
		assert_eq!(local_seconds(clock(19, 42), 0), clock(19, 42));
	}

	#[test]
	fn offsets_wrap_around_midnight_both_ways() {
		assert_eq!(local_seconds(clock(23, 30), 2 * HOUR), clock(1, 30));
		assert_eq!(local_seconds(clock(0, 30), -5 * HOUR), clock(19, 30));
		assert_eq!(local_seconds(clock(12, 0), 14 * HOUR), clock(2, 0));
		assert_eq!(local_seconds(clock(12, 0), -12 * HOUR), 0);
	}

	#[test]
	fn offsets_need_not_be_whole_hours() {
		assert_eq!(local_seconds(clock(10, 0), 5 * HOUR + 45 * 60), clock(15, 45));
		assert_eq!(local_seconds(clock(10, 0), -(9 * HOUR + 30 * 60)), clock(0, 30));
	}

	#[test]
	fn a_stamp_past_midnight_still_lands_within_the_day() {
		assert_eq!(local_seconds(clock(24, 10), HOUR), clock(1, 10));
	}

	#[test]
	fn the_machine_offset_is_a_real_zone_offset() {
		assert!((-12 * HOUR..=14 * HOUR).contains(&utc_offset()));
	}
}
