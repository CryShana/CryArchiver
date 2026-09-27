//! The file operations of archive jobs (project layer spec §8.1 and §9): staging copies and placeholders with their
//! BLAKE3, moving staged files into the mirrored tree through the engine's no-replace rename, reading outputs back,
//! deleting originals, and folder timestamps. All of them block; jobs run them on blocking threads.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};

use cryarchiver_engine::commit::{SourceTimes, rename_no_replace, retry_transient};

use crate::dedup::full_hash;
use crate::record::{Hash, mtime_ns};

const COPY_BUFFER: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
	pub path: PathBuf,
	pub size: u64,
	pub hash: Hash,
}

/// Copies the source into a new staged file while hashing what it reads, then applies the source's timestamps and
/// fsyncs. The hash is the source's content as read, so a read-back that matches it proves the copy.
pub fn copy_to_staged(source: &Path, staged: &Path) -> io::Result<Staged> {
	let input = File::open(source)?;
	let times = SourceTimes::from_metadata(&input.metadata()?)?;
	let mut reader = BufReader::with_capacity(COPY_BUFFER, input);
	let mut output = create_staged(staged)?;

	let mut hasher = blake3::Hasher::new();
	let mut buffer = vec![0; COPY_BUFFER];
	let mut size = 0u64;
	loop {
		let read = reader.read(&mut buffer)?;
		if read == 0 {
			break;
		}
		hasher.update(&buffer[..read]);
		output.write_all(&buffer[..read])?;
		size += read as u64;
	}

	output.set_times(times.file_times())?;
	output.sync_all()?;
	Ok(Staged {
		path: staged.to_path_buf(),
		size,
		hash: Hash(*hasher.finalize().as_bytes()),
	})
}

/// Writes generated content (a placeholder, a rewritten playlist) as a staged file.
pub fn write_staged(staged: &Path, content: &[u8], times: Option<SourceTimes>) -> io::Result<Staged> {
	let mut output = create_staged(staged)?;
	output.write_all(content)?;
	if let Some(times) = times {
		output.set_times(times.file_times())?;
	}
	output.sync_all()?;

	Ok(Staged {
		path: staged.to_path_buf(),
		size: content.len() as u64,
		hash: Hash(*blake3::hash(content).as_bytes()),
	})
}

/// The size and BLAKE3 of a file the engine staged.
pub fn hash_staged(path: &Path) -> io::Result<Staged> {
	Ok(Staged {
		path: path.to_path_buf(),
		size: fs::metadata(path)?.len(),
		hash: full_hash(path)?,
	})
}

/// Renames a staged file to its final path. It never replaces anything, except the output this same source file
/// had before (a re-archive after the source changed, or a rewritten placeholder).
pub fn move_into_place(staged: &Path, target: &Path, own_previous: Option<&Path>) -> io::Result<()> {
	if let Some(parent) = target.parent() {
		fs::create_dir_all(parent)?;
	}

	if own_previous == Some(target) {
		retry_transient(|| fs::rename(staged, target))
	} else {
		retry_transient(|| rename_no_replace(staged, target))
	}
}

/// The placeholder's text (project layer spec §7). `link` is the path from the placeholder to the canonical output.
pub fn placeholder_text(link: &str, source: &str, size: u64, hash: Hash) -> String {
	let link = link.replace('/', MAIN_SEPARATOR_STR);
	format!(
		"This file is a duplicate. The archived copy is here:\n{link}\n\nOriginal file: {source}\nSize: {size} · BLAKE3: {}\n",
		hash.to_hex()
	)
}

/// Reads the file back from disk and checks its size and BLAKE3.
pub fn read_back(path: &Path, size: u64, hash: Hash) -> io::Result<bool> {
	if fs::metadata(path)?.len() != size {
		return Ok(false);
	}
	Ok(full_hash(path)? == hash)
}

pub fn source_unchanged(path: &Path, size: u64, modified_ns: i64) -> bool {
	fs::metadata(path).is_ok_and(|meta| {
		meta.is_file() && meta.len() == size && meta.modified().map(mtime_ns).ok() == Some(modified_ns)
	})
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deletion {
	Deleted,
	AlreadyGone,
	Refused(&'static str),
}

/// Deletes one original, permanently. Only a regular file whose real path lies inside the source root is ever
/// deleted: never a folder, never a link, never a file reached through a link that leads outside the source.
pub fn delete_original(path: &Path, source_root: &Path) -> io::Result<Deletion> {
	let meta = match fs::symlink_metadata(path) {
		Ok(meta) => meta,
		Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Deletion::AlreadyGone),
		Err(err) => return Err(err),
	};
	if !meta.is_file() {
		return Ok(Deletion::Refused("not a regular file"));
	}

	if !fs::canonicalize(path)?.starts_with(fs::canonicalize(source_root)?) {
		return Ok(Deletion::Refused("its real path is outside the source folder"));
	}

	retry_transient(|| fs::remove_file(path))?;
	Ok(Deletion::Deleted)
}

pub fn apply_folder_times(source: &Path, destination: &Path) -> io::Result<()> {
	let times = SourceTimes::from_metadata(&fs::metadata(source)?)?;
	open_folder_for_times(destination)?.set_times(times.file_times())
}

fn create_staged(path: &Path) -> io::Result<File> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)?;
	}
	OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(windows)]
fn open_folder_for_times(path: &Path) -> io::Result<File> {
	use std::os::windows::fs::OpenOptionsExt;

	const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
	const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

	OpenOptions::new()
		.access_mode(FILE_WRITE_ATTRIBUTES)
		.custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
		.open(path)
}

#[cfg(not(windows))]
fn open_folder_for_times(path: &Path) -> io::Result<File> {
	File::open(path)
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, SystemTime};

	use super::*;

	fn old_time() -> SystemTime {
		SystemTime::UNIX_EPOCH + Duration::from_secs(1_234_567_891)
	}

	#[test]
	fn copies_are_hashed_and_keep_the_source_times() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.txt");
		fs::write(&source, b"hello archive").unwrap();
		File::options()
			.write(true)
			.open(&source)
			.unwrap()
			.set_modified(old_time())
			.unwrap();
		let staged_path = dir.path().join("staged").join("x.txt");

		let staged = copy_to_staged(&source, &staged_path).unwrap();

		assert_eq!(fs::read(&staged_path).unwrap(), b"hello archive");
		assert_eq!(staged.size, 13);
		assert_eq!(staged.hash, Hash(*blake3::hash(b"hello archive").as_bytes()));
		assert_eq!(fs::metadata(&staged_path).unwrap().modified().unwrap(), old_time());
		assert!(copy_to_staged(&source, &staged_path).is_err());
	}

	#[test]
	fn moving_into_place_never_replaces_another_file() {
		let dir = tempfile::tempdir().unwrap();
		let staged = dir.path().join("staged.txt");
		let target = dir.path().join("out").join("a.txt");
		fs::create_dir_all(target.parent().unwrap()).unwrap();
		fs::write(&target, b"precious").unwrap();
		fs::write(&staged, b"new").unwrap();

		let err = move_into_place(&staged, &target, None).unwrap_err();

		assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
		assert_eq!(fs::read(&target).unwrap(), b"precious");

		move_into_place(&staged, &target, Some(&target)).unwrap();
		assert_eq!(fs::read(&target).unwrap(), b"new");
	}

	#[test]
	fn moving_into_place_creates_the_folders() {
		let dir = tempfile::tempdir().unwrap();
		let staged = dir.path().join("staged.txt");
		fs::write(&staged, b"new").unwrap();
		let target = dir.path().join("a").join("b").join("c.txt");

		move_into_place(&staged, &target, None).unwrap();

		assert_eq!(fs::read(&target).unwrap(), b"new");
	}

	#[test]
	fn placeholders_name_the_archived_copy() {
		let text = placeholder_text("../Photos/a.avif", "Backup/a.jpg", 70_000, Hash([0xAB; 32]));

		let link = ["..", "Photos", "a.avif"].join(MAIN_SEPARATOR_STR);
		assert_eq!(
			text,
			format!(
				"This file is a duplicate. The archived copy is here:\n{link}\n\nOriginal file: Backup/a.jpg\nSize: 70000 · BLAKE3: {}\n",
				"ab".repeat(32)
			)
		);
	}

	#[test]
	fn read_back_catches_a_changed_output() {
		let dir = tempfile::tempdir().unwrap();
		let output = dir.path().join("a.avif");
		let staged = write_staged(&output, b"encoded bytes", None).unwrap();

		assert!(read_back(&output, staged.size, staged.hash).unwrap());

		fs::write(&output, b"encoded bytez").unwrap();
		assert!(!read_back(&output, staged.size, staged.hash).unwrap());
		fs::write(&output, b"short").unwrap();
		assert!(!read_back(&output, staged.size, staged.hash).unwrap());
	}

	#[test]
	fn originals_are_deleted_only_inside_the_source() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("source");
		fs::create_dir_all(root.join("sub")).unwrap();
		let inside = root.join("sub").join("a.jpg");
		fs::write(&inside, b"a").unwrap();

		assert_eq!(delete_original(&inside, &root).unwrap(), Deletion::Deleted);
		assert!(!inside.exists());
		assert_eq!(delete_original(&inside, &root).unwrap(), Deletion::AlreadyGone);
		assert_eq!(
			delete_original(&root.join("sub"), &root).unwrap(),
			Deletion::Refused("not a regular file")
		);

		let outside = dir.path().join("elsewhere.jpg");
		fs::write(&outside, b"b").unwrap();
		assert_eq!(
			delete_original(&outside, &root).unwrap(),
			Deletion::Refused("its real path is outside the source folder")
		);
		assert!(outside.exists());
	}

	#[test]
	fn a_file_reached_through_a_link_out_of_the_source_is_never_deleted() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("source");
		let elsewhere = dir.path().join("elsewhere");
		fs::create_dir_all(&root).unwrap();
		fs::create_dir_all(&elsewhere).unwrap();
		fs::write(elsewhere.join("a.jpg"), b"a").unwrap();

		#[cfg(unix)]
		let linked = std::os::unix::fs::symlink(&elsewhere, root.join("linked")).is_ok();
		#[cfg(windows)]
		let linked = std::process::Command::new("cmd")
			.arg("/C")
			.arg("mklink")
			.arg("/J")
			.arg(root.join("linked"))
			.arg(&elsewhere)
			.output()
			.is_ok_and(|out| out.status.success());
		if !linked {
			eprintln!("skipped: can't create a folder link here");
			return;
		}

		let through_link = root.join("linked").join("a.jpg");
		assert_eq!(
			delete_original(&through_link, &root).unwrap(),
			Deletion::Refused("its real path is outside the source folder")
		);
		assert!(elsewhere.join("a.jpg").exists());
	}

	#[test]
	fn a_changed_source_is_noticed() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.txt");
		fs::write(&source, b"abc").unwrap();
		let modified = mtime_ns(fs::metadata(&source).unwrap().modified().unwrap());

		assert!(source_unchanged(&source, 3, modified));
		assert!(!source_unchanged(&source, 4, modified));
		assert!(!source_unchanged(&source, 3, modified + 1));
		assert!(!source_unchanged(&dir.path().join("gone"), 3, modified));
	}

	#[test]
	fn folder_times_are_copied() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		let destination = dir.path().join("destination");
		fs::create_dir_all(&source).unwrap();
		fs::create_dir_all(&destination).unwrap();
		open_folder_for_times(&source)
			.unwrap()
			.set_times(fs::FileTimes::new().set_modified(old_time()))
			.unwrap();

		apply_folder_times(&source, &destination).unwrap();

		assert_eq!(fs::metadata(&destination).unwrap().modified().unwrap(), old_time());
	}
}
