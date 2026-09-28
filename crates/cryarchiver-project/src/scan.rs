//! Listing the source one folder at a time, comparing a listing with what the project knows, and walking the
//! destination (project layer spec §6.1 and §6.2). Sizes and times come from the directory listing where the OS
//! provides them; links are followed so the destination always gets real files and folders.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::BuildHasher;
use std::io;
use std::path::Path;

use cryarchiver_engine::commit::file_id;
use cryarchiver_engine::workspace::ARCHIVE_DIR;

use crate::keys::RelPath;
use crate::record::{Identity, mtime_ns};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
	File {
		size: u64,
		mtime_ns: i64,
	},
	Folder,
	/// A link that can't be followed, or an entry that can't be read.
	Broken(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
	pub name: OsString,
	pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
	pub folder: RelPath,
	pub mtime_ns: Option<i64>,
	pub identity: Option<Identity>,
	/// In name order.
	pub entries: Vec<Entry>,
	/// The folder itself couldn't be read.
	pub error: Option<String>,
	/// The folder no longer exists, deleted or moved away rather than made unreadable: the nearest folder above it
	/// that is still there, whose listing sees what vanished.
	pub gone: Option<RelPath>,
	/// Entries never listed: the volume's own folders at the source root.
	pub skipped: Vec<OsString>,
}

/// A volume's own folders, which only ever appear at its root.
const VOLUME_FOLDERS: [&str; 3] = ["System Volume Information", "$RECYCLE.BIN", "lost+found"];

/// Blocking: run it on a blocking thread.
pub fn list_folder(root: &Path, folder: &RelPath) -> Listing {
	let dir = folder.to_path(root);
	let mut listing = Listing {
		folder: folder.clone(),
		mtime_ns: fs::metadata(&dir).and_then(|meta| meta.modified()).ok().map(mtime_ns),
		identity: file_id(&dir).ok().map(Identity::from),
		..Listing::default()
	};

	let entries = match fs::read_dir(&dir) {
		Ok(entries) => entries,
		Err(err) => {
			if !folder.is_root() && err.kind() == io::ErrorKind::NotFound && absent(&dir) {
				listing.gone = Some(nearest_present(root, folder));
			}
			listing.error = Some(err.to_string());
			return listing;
		}
	};

	for entry in entries {
		// A partial listing would make every file it misses look deleted.
		let entry = match entry {
			Ok(entry) => entry,
			Err(err) => {
				listing.entries.clear();
				listing.error = Some(err.to_string());
				return listing;
			}
		};

		let name = entry.file_name();
		if folder.is_root() && is_volume_folder(&name) {
			listing.skipped.push(name);
			continue;
		}

		let followed = match entry.file_type() {
			Ok(kind) if kind.is_symlink() => fs::metadata(entry.path()),
			Ok(_) => entry.metadata(),
			Err(err) => Err(err),
		};

		let kind = match followed {
			Ok(meta) if meta.is_dir() => EntryKind::Folder,
			Ok(meta) => EntryKind::File {
				size: meta.len(),
				mtime_ns: meta.modified().map(mtime_ns).unwrap_or_default(),
			},
			Err(err) => EntryKind::Broken(err.to_string()),
		};
		listing.entries.push(Entry { name, kind });
	}

	listing
		.entries
		.sort_by(|a, b| a.name.as_encoded_bytes().cmp(b.name.as_encoded_bytes()));
	listing
}

/// No folder or link by that name: a link whose target is offline still exists itself, and its listing fails as
/// unreadable, not gone.
fn absent(path: &Path) -> bool {
	match fs::symlink_metadata(path) {
		Ok(meta) => meta.is_file(),
		Err(err) => err.kind() == io::ErrorKind::NotFound,
	}
}

fn nearest_present(root: &Path, folder: &RelPath) -> RelPath {
	let mut ancestor = folder.parent().unwrap_or_else(RelPath::root);
	while !ancestor.is_root() && absent(&ancestor.to_path(root)) {
		ancestor = ancestor.parent().unwrap_or_else(RelPath::root);
	}
	ancestor
}

fn is_volume_folder(name: &OsStr) -> bool {
	name.to_str()
		.is_some_and(|name| VOLUME_FOLDERS.iter().any(|folder| folder.eq_ignore_ascii_case(name)))
}

/// What a folder's listing changes, compared with what the project stored for it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderDiff {
	/// Indices into the listing's entries.
	pub new_files: Vec<usize>,
	pub changed_files: Vec<usize>,
	pub unchanged_files: Vec<usize>,
	pub folders: Vec<usize>,
	pub broken: Vec<usize>,
	/// Stored files the listing no longer has.
	pub missing_files: Vec<OsString>,
}

pub fn diff<S: BuildHasher>(stored: &HashMap<OsString, (u64, i64), S>, listing: &Listing) -> FolderDiff {
	let mut diff = FolderDiff::default();
	let mut seen = HashSet::new();

	for (index, entry) in listing.entries.iter().enumerate() {
		match &entry.kind {
			EntryKind::File { size, mtime_ns } => {
				seen.insert(entry.name.as_os_str());
				match stored.get(&entry.name) {
					None => diff.new_files.push(index),
					Some(&(old_size, old_mtime)) if old_size == *size && old_mtime == *mtime_ns => {
						diff.unchanged_files.push(index);
					}
					Some(_) => diff.changed_files.push(index),
				}
			}
			EntryKind::Folder => diff.folders.push(index),
			EntryKind::Broken(_) => {
				seen.insert(entry.name.as_os_str());
				diff.broken.push(index);
			}
		}
	}

	let mut missing: Vec<OsString> = stored
		.keys()
		.filter(|name| !seen.contains(name.as_os_str()))
		.cloned()
		.collect();
	missing.sort();
	diff.missing_files = missing;
	diff
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationFile {
	pub path: RelPath,
	pub size: u64,
	/// A link or a junction, listed as itself and never followed: never an archived file.
	pub link: bool,
}

/// Every file in the destination except the project's own: the `.cryarchiver` folder and the project file.
/// Blocking, breadth-first, and never follows links.
pub fn walk_destination(root: &Path, project_file: &OsStr) -> Vec<DestinationFile> {
	let mut files = Vec::new();
	let mut folders = VecDeque::from([RelPath::root()]);

	while let Some(folder) = folders.pop_front() {
		let Ok(entries) = fs::read_dir(folder.to_path(root)) else {
			continue;
		};

		for entry in entries.flatten() {
			let name = entry.file_name();
			if folder.is_root() && (name == ARCHIVE_DIR || name == project_file) {
				continue;
			}

			let Ok(kind) = entry.file_type() else {
				continue;
			};
			let path = folder.join(&name);
			if kind.is_dir() {
				folders.push_back(path);
			} else if let Ok(meta) = entry.metadata() {
				files.push(DestinationFile {
					path,
					size: meta.len(),
					link: kind.is_symlink(),
				});
			}
		}
	}

	files.sort_by(|a, b| a.path.cmp(&b.path));
	files
}

#[cfg(test)]
mod tests {
	use std::path::PathBuf;

	use super::*;

	fn names(listing: &Listing) -> Vec<String> {
		listing
			.entries
			.iter()
			.map(|entry| entry.name.to_string_lossy().into_owned())
			.collect()
	}

	#[test]
	fn lists_files_and_folders_in_name_order() {
		let dir = tempfile::tempdir().unwrap();
		fs::create_dir(dir.path().join("b-folder")).unwrap();
		fs::write(dir.path().join("c.txt"), b"hello").unwrap();
		fs::write(dir.path().join("a.jpg"), b"").unwrap();

		let listing = list_folder(dir.path(), &RelPath::root());

		assert_eq!(names(&listing), ["a.jpg", "b-folder", "c.txt"]);
		assert_eq!(listing.entries[1].kind, EntryKind::Folder);
		assert!(matches!(listing.entries[2].kind, EntryKind::File { size: 5, .. }));
		assert!(listing.identity.is_some());
		assert!(listing.error.is_none());
	}

	#[test]
	fn a_folder_that_is_gone_reports_an_error_and_that_it_is_gone() {
		let dir = tempfile::tempdir().unwrap();

		let listing = list_folder(dir.path(), &RelPath::root().join(OsStr::new("missing")));

		assert!(listing.error.is_some());
		assert_eq!(listing.gone, Some(RelPath::root()));
		assert!(listing.entries.is_empty());
	}

	#[test]
	fn a_missing_source_root_is_never_gone() {
		let dir = tempfile::tempdir().unwrap();

		let listing = list_folder(&dir.path().join("unplugged"), &RelPath::root());

		assert!(listing.error.is_some());
		assert_eq!(listing.gone, None);
	}

	fn symlink_dir(target: &Path, link: &Path) -> bool {
		#[cfg(unix)]
		return std::os::unix::fs::symlink(target, link).is_ok();

		#[cfg(windows)]
		return std::process::Command::new("cmd")
			.arg("/C")
			.arg("mklink")
			.arg("/J")
			.arg(link)
			.arg(target)
			.output()
			.is_ok_and(|out| out.status.success());
	}

	#[test]
	fn links_to_folders_are_followed() {
		let dir = tempfile::tempdir().unwrap();
		let target = dir.path().join("real");
		fs::create_dir(&target).unwrap();
		fs::write(target.join("a.txt"), b"a").unwrap();
		if !symlink_dir(&target, &dir.path().join("linked")) {
			eprintln!("skipped: can't create a folder link here");
			return;
		}

		let top = list_folder(dir.path(), &RelPath::root());
		let linked = list_folder(dir.path(), &RelPath::root().join(OsStr::new("linked")));

		assert_eq!(top.entries[0].kind, EntryKind::Folder);
		assert_eq!(names(&linked), ["a.txt"]);
		assert_eq!(
			linked.identity,
			list_folder(dir.path(), &RelPath::root().join(OsStr::new("real"))).identity
		);
	}

	#[test]
	fn a_folder_link_whose_target_is_offline_is_unreadable_not_gone() {
		let dir = tempfile::tempdir().unwrap();
		let target = dir.path().join("disk");
		fs::create_dir(&target).unwrap();
		if !symlink_dir(&target, &dir.path().join("linked")) {
			eprintln!("skipped: can't create a folder link here");
			return;
		}
		fs::remove_dir(&target).unwrap();

		let top = list_folder(dir.path(), &RelPath::root());
		let linked = list_folder(dir.path(), &RelPath::root().join(OsStr::new("linked")));

		assert!(matches!(top.entries[0].kind, EntryKind::Broken(_)));
		assert!(linked.error.is_some());
		assert_eq!(linked.gone, None);
	}

	#[cfg(unix)]
	#[test]
	fn broken_links_are_reported() {
		let dir = tempfile::tempdir().unwrap();
		std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("dangling")).unwrap();

		let listing = list_folder(dir.path(), &RelPath::root());

		assert!(matches!(listing.entries[0].kind, EntryKind::Broken(_)));
	}

	#[test]
	fn diff_sorts_entries_into_new_changed_unchanged_and_missing() {
		let listing = Listing {
			entries: vec![
				Entry {
					name: "same.txt".into(),
					kind: EntryKind::File { size: 1, mtime_ns: 5 },
				},
				Entry {
					name: "edited.txt".into(),
					kind: EntryKind::File { size: 2, mtime_ns: 9 },
				},
				Entry {
					name: "new.txt".into(),
					kind: EntryKind::File { size: 3, mtime_ns: 1 },
				},
				Entry {
					name: "sub".into(),
					kind: EntryKind::Folder,
				},
				Entry {
					name: "dangling".into(),
					kind: EntryKind::Broken("not found".into()),
				},
			],
			..Listing::default()
		};
		let stored = HashMap::from([
			(OsString::from("same.txt"), (1, 5)),
			(OsString::from("edited.txt"), (2, 5)),
			(OsString::from("deleted.txt"), (4, 4)),
			(OsString::from("dangling"), (4, 4)),
		]);

		let diff = diff(&stored, &listing);

		assert_eq!(diff.unchanged_files, [0]);
		assert_eq!(diff.changed_files, [1]);
		assert_eq!(diff.new_files, [2]);
		assert_eq!(diff.folders, [3]);
		assert_eq!(diff.broken, [4]);
		assert_eq!(diff.missing_files, [OsString::from("deleted.txt")]);
	}

	#[test]
	fn the_destination_walk_skips_the_project_files() {
		let dir = tempfile::tempdir().unwrap();
		let root: PathBuf = dir.path().to_path_buf();
		fs::create_dir_all(root.join(".cryarchiver").join("work")).unwrap();
		fs::write(root.join(".cryarchiver").join("work").join("x"), b"x").unwrap();
		fs::create_dir_all(root.join(".cryarchiver (source)").join("work")).unwrap();
		fs::write(root.join(".cryarchiver (source)").join("work").join("y"), b"123").unwrap();
		fs::write(root.join("archive.cryarchive"), b"db").unwrap();
		fs::create_dir(root.join("Photos")).unwrap();
		fs::write(root.join("Photos").join("a.avif"), b"1234").unwrap();
		fs::write(root.join("notes.txt"), b"12").unwrap();

		let files = walk_destination(&root, OsStr::new("archive.cryarchive"));

		let found: Vec<(String, u64)> = files.iter().map(|file| (file.path.display(), file.size)).collect();
		assert_eq!(
			found,
			[
				(".cryarchiver (source)/work/y".to_owned(), 3),
				("Photos/a.avif".to_owned(), 4),
				("notes.txt".to_owned(), 2)
			]
		);
	}

	#[test]
	fn the_destination_walk_lists_a_link_as_itself_and_never_follows_it() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("archive");
		let elsewhere = dir.path().join("elsewhere");
		fs::create_dir_all(&root).unwrap();
		fs::create_dir_all(&elsewhere).unwrap();
		fs::write(elsewhere.join("a.avif"), b"1234").unwrap();
		fs::write(root.join("notes.txt"), b"12").unwrap();

		#[cfg(windows)]
		let linked = std::process::Command::new("cmd")
			.args(["/C", "mklink", "/J"])
			.arg(root.join("Photos"))
			.arg(&elsewhere)
			.output()
			.is_ok_and(|out| out.status.success());
		#[cfg(unix)]
		let linked = std::os::unix::fs::symlink(&elsewhere, root.join("Photos")).is_ok();
		if !linked {
			eprintln!("skipped: no folder link can be made here");
			return;
		}

		let files = walk_destination(&root, OsStr::new("archive.cryarchive"));

		let found: Vec<(String, bool)> = files.iter().map(|file| (file.path.display(), file.link)).collect();
		assert_eq!(found, [("Photos".to_owned(), true), ("notes.txt".to_owned(), false)]);
	}

	#[test]
	fn a_file_and_a_same_named_folder_sort_with_the_file_first() {
		let dir = tempfile::tempdir().unwrap();
		let root: PathBuf = dir.path().to_path_buf();
		fs::create_dir(root.join("Photos")).unwrap();
		fs::write(root.join("Photos").join("x.txt"), b"1").unwrap();
		fs::write(root.join("Photos.txt"), b"12").unwrap();

		let files = walk_destination(&root, OsStr::new("archive.cryarchive"));

		let found: Vec<String> = files.iter().map(|file| file.path.display()).collect();
		assert_eq!(found, ["Photos.txt", "Photos/x.txt"]);
	}
}
