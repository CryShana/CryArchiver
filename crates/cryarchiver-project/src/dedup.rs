//! Duplicate detection in three stages (project layer spec §7): exact size, a BLAKE3 over three 64 KiB windows,
//! then a full BLAKE3. Each stage reads only the files the one before it couldn't tell apart. The same file reached
//! twice (a hard link, or a link to a file already in the tree) is recognised by its identity, without hashing.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::keys::RelPath;
use crate::record::{Hash, Identity};
use crate::tree::FileId;

pub const MIN_DEDUP_SIZE: u64 = 64 * 1024;

const WINDOW: u64 = 64 * 1024;
const WHOLE_FILE_LIMIT: u64 = 3 * WINDOW;

/// The first, middle (centred on size / 2) and last 64 KiB. A file of 192 KiB or less is hashed whole, because its
/// windows would overlap.
pub fn window_hash(path: &Path) -> io::Result<Hash> {
	let mut file = File::open(path)?;
	let size = file.metadata()?.len();
	if size <= WHOLE_FILE_LIMIT {
		return full_hash(path);
	}

	let mut hasher = blake3::Hasher::new();
	let mut window = vec![0; usize::try_from(WINDOW).expect("64 KiB fits")];
	for offset in [0, size / 2 - WINDOW / 2, size - WINDOW] {
		file.seek(SeekFrom::Start(offset))?;
		file.read_exact(&mut window)?;
		hasher.update(&window);
	}
	Ok(Hash(*hasher.finalize().as_bytes()))
}

pub fn full_hash(path: &Path) -> io::Result<Hash> {
	let mut hasher = blake3::Hasher::new();
	hasher.update_reader(File::open(path)?)?;
	Ok(Hash(*hasher.finalize().as_bytes()))
}

/// One file of a size group, with whatever hashes the project already has for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
	pub id: FileId,
	pub path: RelPath,
	/// The size and time the project recorded: the same file reached twice is recorded alike.
	pub size: u64,
	pub mtime_ns: i64,
	pub identity: Option<Identity>,
	pub window: Option<Hash>,
	pub full: Option<Hash>,
}

/// What a size group needs next, and what it has already proven.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupPlan {
	pub window_needed: Vec<FileId>,
	pub full_needed: Vec<FileId>,
	/// Duplicate sets, canonical first: the shallowest folder, then name order.
	pub sets: Vec<Vec<FileId>>,
	pub unique: Vec<FileId>,
}

pub fn plan_group(members: &[Member]) -> GroupPlan {
	let mut plan = GroupPlan::default();

	let mut same_file: HashMap<SameFile, Vec<&Member>> = HashMap::new();
	let mut representatives: Vec<&Member> = Vec::new();
	for member in members {
		match same_file_key(member) {
			Some(key) => {
				let mates = same_file.entry(key).or_default();
				if mates.is_empty() {
					representatives.push(member);
				}
				mates.push(member);
			}
			None => representatives.push(member),
		}
	}

	if representatives.len() > 1 {
		plan.window_needed = representatives
			.iter()
			.filter(|member| member.window.is_none())
			.map(|member| member.id)
			.collect();
		if !plan.window_needed.is_empty() {
			return plan;
		}
	}

	let mut by_window: HashMap<Option<Hash>, Vec<&Member>> = HashMap::new();
	for representative in &representatives {
		let key = if representatives.len() > 1 {
			representative.window
		} else {
			None
		};
		by_window.entry(key).or_default().push(representative);
	}

	let mut resolved: Vec<Vec<&Member>> = Vec::new();
	for group in by_window.into_values() {
		if group.len() == 1 {
			resolved.push(with_mates(group[0], &same_file));
			continue;
		}

		let unhashed: Vec<FileId> = group
			.iter()
			.filter(|member| member.full.is_none())
			.map(|member| member.id)
			.collect();
		if !unhashed.is_empty() {
			plan.full_needed.extend(unhashed);
			continue;
		}

		let mut by_full: HashMap<Hash, Vec<&Member>> = HashMap::new();
		for member in group {
			by_full
				.entry(member.full.expect("checked above"))
				.or_default()
				.extend(with_mates(member, &same_file));
		}
		resolved.extend(by_full.into_values());
	}

	for mut set in resolved {
		if set.len() == 1 {
			plan.unique.push(set[0].id);
			continue;
		}

		set.sort_by(|a, b| canonical_order(&a.path, &b.path));
		plan.sets.push(set.iter().map(|member| member.id).collect());
	}

	plan.window_needed.sort();
	plan.full_needed.sort();
	plan.unique.sort();
	plan.sets.sort();
	plan
}

/// Members are the same file only while their identities match and their records describe the file alike.
type SameFile = (Identity, u64, i64);

fn same_file_key(member: &Member) -> Option<SameFile> {
	member.identity.map(|identity| (identity, member.size, member.mtime_ns))
}

fn with_mates<'m>(representative: &'m Member, same_file: &HashMap<SameFile, Vec<&'m Member>>) -> Vec<&'m Member> {
	same_file_key(representative)
		.and_then(|key| same_file.get(&key).cloned())
		.unwrap_or_else(|| vec![representative])
}

/// The shallowest folder first; ties go to the first in name order.
pub fn canonical_order(a: &RelPath, b: &RelPath) -> std::cmp::Ordering {
	a.depth().cmp(&b.depth()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	fn member(id: u64, path: &str) -> Member {
		Member {
			id: FileId(id),
			path: RelPath::from_bytes(path.as_bytes().to_vec()),
			size: 100_000,
			mtime_ns: 1,
			identity: None,
			window: None,
			full: None,
		}
	}

	fn hashed(id: u64, path: &str, window: u8, full: u8) -> Member {
		Member {
			window: Some(Hash([window; 32])),
			full: Some(Hash([full; 32])),
			..member(id, path)
		}
	}

	#[test]
	fn small_files_are_hashed_whole() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("a.bin");
		fs::write(&path, vec![7u8; 100_000]).unwrap();

		assert_eq!(window_hash(&path).unwrap(), full_hash(&path).unwrap());
	}

	#[test]
	fn damage_outside_the_windows_is_caught_only_by_the_full_hash() {
		let dir = tempfile::tempdir().unwrap();
		let size = 1_000_000;
		let original = dir.path().join("original.bin");
		let damaged = dir.path().join("damaged.bin");
		let mut bytes: Vec<u8> = (0..size).map(|index| u8::try_from(index % 251).unwrap()).collect();
		fs::write(&original, &bytes).unwrap();
		bytes[200_000] ^= 0xFF;
		fs::write(&damaged, &bytes).unwrap();

		assert_eq!(window_hash(&original).unwrap(), window_hash(&damaged).unwrap());
		assert_ne!(full_hash(&original).unwrap(), full_hash(&damaged).unwrap());

		bytes[size / 2] ^= 0xFF;
		fs::write(&damaged, &bytes).unwrap();
		assert_ne!(window_hash(&original).unwrap(), window_hash(&damaged).unwrap());
	}

	#[test]
	fn a_lone_file_needs_no_hash() {
		let plan = plan_group(&[member(1, "a.jpg")]);

		assert_eq!(plan.unique, [FileId(1)]);
		assert!(plan.window_needed.is_empty());
	}

	#[test]
	fn a_size_group_first_needs_window_hashes() {
		let plan = plan_group(&[member(1, "a.jpg"), hashed(2, "b.jpg", 1, 1), member(3, "c.jpg")]);

		assert_eq!(plan.window_needed, [FileId(1), FileId(3)]);
		assert!(plan.sets.is_empty() && plan.unique.is_empty());
	}

	#[test]
	fn matching_windows_need_the_full_hash_before_anything_is_a_duplicate() {
		let a = Member {
			window: Some(Hash([1; 32])),
			..member(1, "a.jpg")
		};
		let b = Member {
			window: Some(Hash([1; 32])),
			..member(2, "b.jpg")
		};
		let c = Member {
			window: Some(Hash([2; 32])),
			..member(3, "c.jpg")
		};

		let plan = plan_group(&[a, b, c]);

		assert_eq!(plan.full_needed, [FileId(1), FileId(2)]);
		assert_eq!(plan.unique, [FileId(3)]);
		assert!(plan.sets.is_empty());
	}

	#[test]
	fn equal_full_hashes_form_a_set_with_the_shallowest_first() {
		let plan = plan_group(&[
			hashed(1, "x/deep/a.jpg", 1, 1),
			hashed(2, "b.jpg", 1, 1),
			hashed(3, "x/a.jpg", 1, 1),
			hashed(4, "x/b.jpg", 1, 2),
		]);

		assert_eq!(plan.sets, [vec![FileId(2), FileId(3), FileId(1)]]);
		assert_eq!(plan.unique, [FileId(4)]);
	}

	#[test]
	fn the_same_file_reached_twice_needs_no_hash() {
		let identity = Some(Identity { volume: 1, index: 9 });
		let plan = plan_group(&[
			Member {
				identity,
				..member(1, "sub/link.jpg")
			},
			Member {
				identity,
				..member(2, "a.jpg")
			},
		]);

		assert!(plan.window_needed.is_empty());
		assert_eq!(plan.sets, [vec![FileId(2), FileId(1)]]);
	}

	#[test]
	fn the_same_identity_recorded_with_another_size_or_time_is_not_the_same_file() {
		let identity = Some(Identity { volume: 1, index: 9 });
		let plan = plan_group(&[
			Member {
				identity,
				..member(1, "a.jpg")
			},
			Member {
				identity,
				mtime_ns: 2,
				..member(2, "sub/link.jpg")
			},
		]);

		assert!(plan.sets.is_empty());
		assert_eq!(plan.window_needed, [FileId(1), FileId(2)]);
	}

	#[test]
	fn a_hard_link_joins_the_set_its_content_matches() {
		let identity = Some(Identity { volume: 1, index: 9 });
		let plan = plan_group(&[
			Member {
				identity,
				..hashed(1, "a.jpg", 1, 1)
			},
			Member {
				identity,
				..member(2, "z/link.jpg")
			},
			hashed(3, "b.jpg", 1, 1),
		]);

		assert_eq!(plan.sets, [vec![FileId(1), FileId(3), FileId(2)]]);
	}
}
