pub mod bluray;
pub mod dvd;
pub mod ps;
pub mod read;
pub mod udf;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const SECTOR_LEN: usize = 2048;
pub const SECTOR: u64 = SECTOR_LEN as u64;

const READ_CHUNK: usize = 512;

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
}
