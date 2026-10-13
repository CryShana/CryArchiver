pub mod bluray;
pub mod dvd;
pub mod ps;
pub mod read;
pub mod udf;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SECTOR_LEN: usize = 2048;
pub const SECTOR: u64 = SECTOR_LEN as u64;

const READ_CHUNK: usize = 512;
const DAY_S: i64 = 86_400;

/// `(file, offset, length)`: a byte range of a file.
pub type Range = (PathBuf, u64, u64);

/// Ranges with each one that continues the one before it in the same file merged into it.
pub fn joined(ranges: impl IntoIterator<Item = Range>) -> Vec<Range> {
	let mut joined: Vec<Range> = Vec::new();
	for (path, offset, length) in ranges {
		match joined.last_mut() {
			Some((last, at, len)) if *last == path && *at + *len == offset => *len += length,
			_ => joined.push((path, offset, length)),
		}
	}
	joined
}

/// Reads byte ranges of files, keeping each file open.
#[derive(Default)]
pub struct Reader {
	files: HashMap<PathBuf, File>,
}

impl Reader {
	pub fn read(&mut self, path: &Path, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
		if !self.files.contains_key(path) {
			self.files.insert(path.to_path_buf(), File::open(path)?);
		}

		let file = self.files.get_mut(path).expect("the file was just opened");
		file.seek(SeekFrom::Start(offset))?;
		file.read_exact(bytes)
	}

	/// Calls `visit` with each whole sector of `ranges` read in order and its byte position in them, until it returns
	/// `false`, or for at most `limit` sectors.
	pub fn each_sector(
		&mut self,
		ranges: &[Range],
		limit: usize,
		mut visit: impl FnMut(u64, &[u8]) -> bool,
	) -> io::Result<()> {
		let mut position = 0;
		let mut left = limit;
		let mut chunk = vec![0; READ_CHUNK * SECTOR_LEN];
		for (path, offset, length) in ranges {
			let mut done = 0;
			while done + SECTOR <= *length && left > 0 {
				let sectors = usize::try_from((length - done) / SECTOR)
					.unwrap_or(usize::MAX)
					.min(READ_CHUNK)
					.min(left);
				let bytes = &mut chunk[..sectors * SECTOR_LEN];
				self.read(path, offset + done, bytes)?;

				for sector in bytes.as_chunks::<SECTOR_LEN>().0 {
					if !visit(position, sector) {
						return Ok(());
					}
					position += SECTOR;
				}
				done += (sectors * SECTOR_LEN) as u64;
				left -= sectors;
			}
		}
		Ok(())
	}
}

/// `time` as RFC 3339 in UTC to the second, as ffmpeg's `creation_time` takes it; `None` before 1970.
pub fn rfc3339(time: SystemTime) -> Option<String> {
	let seconds = i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
	let (year, month, day) = civil(seconds.div_euclid(DAY_S));
	let of_day = seconds.rem_euclid(DAY_S);
	Some(format!(
		"{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
		of_day / 3600,
		of_day / 60 % 60,
		of_day % 60
	))
}

/// The time of a UTC date and time of day, `None` when the date doesn't exist or is before 1970.
fn utc_time(year: i64, month: i64, day: i64, of_day_s: i64) -> Option<SystemTime> {
	if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
		return None;
	}

	let shifted = if month <= 2 { year - 1 } else { year };
	let era = shifted.div_euclid(400);
	let of_era = shifted - era * 400;
	let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
	let days = era * 146_097 + of_era * 365 + of_era / 4 - of_era / 100 + of_year - 719_468;
	if civil(days) != (year, month, day) {
		return None;
	}

	let seconds = u64::try_from(days * DAY_S + of_day_s).ok()?;
	Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// The proleptic Gregorian `(year, month, day)` of a day count since 1970-01-01.
fn civil(days: i64) -> (i64, i64, i64) {
	let days = days + 719_468;
	let era = days.div_euclid(146_097);
	let of_era = days - era * 146_097;
	let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
	let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let shifted_month = (5 * of_year + 2) / 153;
	let day = of_year - (153 * shifted_month + 2) / 5 + 1;
	let month = if shifted_month < 10 {
		shifted_month + 3
	} else {
		shifted_month - 9
	};
	(year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// The `N` bytes at `at`, when `bytes` holds them.
fn array<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
	bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn get<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], String> {
	array(bytes, at).ok_or_else(|| format!("it ends before byte {at}"))
}

fn byte(bytes: &[u8], at: usize) -> Result<u8, String> {
	get::<1>(bytes, at).map(|[byte]| byte)
}

fn be16(bytes: &[u8], at: usize) -> Result<u16, String> {
	get(bytes, at).map(u16::from_be_bytes)
}

fn be32(bytes: &[u8], at: usize) -> Result<u32, String> {
	get(bytes, at).map(u32::from_be_bytes)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ranges_that_continue_each_other_are_merged() {
		let a = PathBuf::from("a.vob");
		let b = PathBuf::from("b.vob");

		let merged = joined([
			(a.clone(), 0, 2048),
			(a.clone(), 2048, 4096),
			(a.clone(), 10_240, 2048),
			(b.clone(), 12_288, 2048),
		]);

		assert_eq!(merged, [(a.clone(), 0, 6144), (a, 10_240, 2048), (b, 12_288, 2048)]);
	}

	#[test]
	fn dates_turn_into_times_and_back_into_rfc_3339() {
		let time = utc_time(2003, 7, 14, 18 * 3600 + 30 * 60 + 5).unwrap();

		assert_eq!(time, UNIX_EPOCH + Duration::from_secs(1_058_207_405));
		assert_eq!(rfc3339(time).unwrap(), "2003-07-14T18:30:05Z");
		assert_eq!(
			rfc3339(utc_time(2000, 2, 29, 0).unwrap()).unwrap(),
			"2000-02-29T00:00:00Z"
		);
	}

	#[test]
	fn dates_that_do_not_exist_or_precede_1970_have_no_time() {
		assert_eq!(utc_time(2001, 2, 29, 0), None);
		assert_eq!(utc_time(2001, 0, 1, 0), None);
		assert_eq!(utc_time(2001, 4, 31, 0), None);
		assert_eq!(utc_time(1969, 12, 31, 0), None);
		assert_eq!(rfc3339(UNIX_EPOCH - Duration::from_secs(1)), None);
	}
}
