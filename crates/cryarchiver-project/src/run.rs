//! The file operations of archive jobs (project layer spec §8.1 and §9): staging copies and placeholders with their
//! BLAKE3, moving staged files into the mirrored tree through the engine's no-replace rename, reading outputs back,
//! deleting originals, and folder timestamps. All of them block; jobs run them on blocking threads.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};

use cryarchiver_engine::commit::{SourceTimes, rename_no_replace, retry_transient};

use crate::dedup::full_hash;
use crate::record::{Hash, Identity, mtime_ns};
use native::open_folder_for_times;

const COPY_BUFFER: usize = 1 << 20;
const READ_BACK_BUFFER: usize = 4 << 20;

const NOT_A_REGULAR_FILE: &str = "not a regular file";
const OUTSIDE_THE_SOURCE: &str = "its real path is outside the source folder";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
	pub path: PathBuf,
	pub size: u64,
	pub hash: Hash,
}

/// Copies the source into a new staged file while hashing what it reads, then applies the source's timestamps and
/// fsyncs. The hash is the source's content as read, so a read-back that matches it proves the copy.
pub fn copy_to_staged(source: &Path, staged: &Path) -> io::Result<Staged> {
	let mut input = File::open(source)?;
	let times = SourceTimes::from_metadata(&input.metadata()?)?;
	let output = create_staged(staged)?;

	let (size, hash) = discard_on_error(staged, copy_hashed(&mut input, output, &times))?;

	Ok(Staged {
		path: staged.to_path_buf(),
		size,
		hash,
	})
}

/// Writes generated content (a placeholder, a rewritten playlist) as a staged file.
pub fn write_staged(staged: &Path, content: &[u8], times: Option<SourceTimes>) -> io::Result<Staged> {
	let output = create_staged(staged)?;
	discard_on_error(staged, finish_write(output, content, times))?;

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
	let created = match target.parent() {
		Some(parent) => create_folders(parent)?,
		None => Vec::new(),
	};

	if own_previous == Some(target) {
		retry_transient(|| native::rename_replacing(staged, target))?;
	} else {
		retry_transient(|| rename_no_replace(staged, target))?;
	}

	sync_entries(target, &created);

	Ok(())
}

/// The placeholder's text (project layer spec §7). `link` is the path from the placeholder to the canonical output.
pub fn placeholder_text(link: &str, source: &str, size: u64, hash: Hash) -> String {
	let link = link.replace('/', MAIN_SEPARATOR_STR);
	format!(
		"This file is a duplicate. The archived copy is here:\n{link}\n\nOriginal file: {source}\nSize: {size} · BLAKE3: {}\n",
		hash.to_hex()
	)
}

/// Reads the file back from the medium, past the page cache, with large sequential reads, and checks its size and
/// BLAKE3.
pub fn read_back(path: &Path, size: u64, hash: Hash) -> io::Result<bool> {
	let mut file = native::open_uncached(path)?;
	if file.metadata()?.len() != size {
		return Ok(false);
	}

	#[cfg(windows)]
	let mut buffer = native::AlignedBuffer::new(READ_BACK_BUFFER);
	#[cfg(not(windows))]
	let mut buffer = vec![0; READ_BACK_BUFFER];
	let (read, actual) = hash_through(&mut file, &mut buffer, |_| Ok(()))?;

	Ok(read == size && actual == hash)
}

pub fn source_unchanged(path: &Path, size: u64, modified_ns: i64) -> bool {
	fs::metadata(path).is_ok_and(|meta| {
		meta.is_file() && meta.len() == size && meta.modified().map(mtime_ns).ok() == Some(modified_ns)
	})
}

/// What the original looked like when it was archived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
	pub size: u64,
	pub mtime_ns: i64,
	/// The file's identity when the project recorded one.
	pub identity: Option<Identity>,
}

impl Expected {
	fn matches(&self, size: u64, mtime_ns: i64, identity: Identity) -> bool {
		self.size == size && self.mtime_ns == mtime_ns && self.identity.is_none_or(|expected| expected == identity)
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deletion {
	Deleted,
	AlreadyGone,
	/// Another program has the original open; nothing was deleted. The job re-runs its checks later.
	Busy,
	/// The original no longer matches what was archived; nothing was deleted.
	Changed,
	Refused(&'static str),
}

/// Deletes one original, permanently. The checks run on the file at the moment of deletion: only a regular file whose
/// real path lies inside the source root and that still matches `expected` is deleted. Never a folder, never a link,
/// never a file reached through a link that leads outside the source. Nothing is retried: a file another program has
/// open is `Busy`.
pub fn delete_original(path: &Path, source_root: &Path, expected: &Expected) -> io::Result<Deletion> {
	// An unreachable source root (an unplugged drive) is an error, never proof that the original is gone.
	let root = fs::canonicalize(source_root)?;

	match native::delete_checked(path, &root, expected) {
		Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Deletion::AlreadyGone),
		result => result,
	}
}

pub fn apply_folder_times(source: &Path, destination: &Path) -> io::Result<()> {
	let meta = fs::symlink_metadata(source)?;
	if meta.is_symlink() {
		return Ok(());
	}

	let times = SourceTimes::from_metadata(&meta)?;
	open_folder_for_times(destination)?.set_times(times.file_times())
}

fn create_staged(path: &Path) -> io::Result<File> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)?;
	}
	OpenOptions::new().write(true).create_new(true).open(path)
}

fn copy_hashed(input: &mut File, mut output: File, times: &SourceTimes) -> io::Result<(u64, Hash)> {
	let mut buffer = vec![0; COPY_BUFFER];
	let (size, hash) = hash_through(input, &mut buffer, |chunk| output.write_all(chunk))?;

	output.set_times(times.file_times())?;
	output.sync_all()?;

	Ok((size, hash))
}

fn finish_write(mut output: File, content: &[u8], times: Option<SourceTimes>) -> io::Result<()> {
	output.write_all(content)?;
	if let Some(times) = times {
		output.set_times(times.file_times())?;
	}
	output.sync_all()
}

/// The staged file must be closed by the time `result` exists, so it can be removed on every OS.
fn discard_on_error<T>(staged: &Path, result: io::Result<T>) -> io::Result<T> {
	if result.is_err() {
		let _ = retry_transient(|| fs::remove_file(staged));
	}

	result
}

fn hash_through(
	input: &mut File,
	buffer: &mut [u8],
	mut each: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<(u64, Hash)> {
	let mut hasher = blake3::Hasher::new();
	let mut size = 0u64;

	loop {
		let read = match input.read(buffer) {
			Ok(0) => break,
			Ok(read) => read,
			Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
			Err(err) => return Err(err),
		};
		hasher.update(&buffer[..read]);
		each(&buffer[..read])?;
		size += read as u64;
	}

	Ok((size, Hash(*hasher.finalize().as_bytes())))
}

fn create_folders(folder: &Path) -> io::Result<Vec<PathBuf>> {
	let missing = folder
		.ancestors()
		.take_while(|dir| !dir.as_os_str().is_empty() && fs::symlink_metadata(dir).is_err())
		.map(Path::to_path_buf)
		.collect();
	fs::create_dir_all(folder)?;

	Ok(missing)
}

#[cfg(unix)]
fn sync_entries(target: &Path, created: &[PathBuf]) {
	use cryarchiver_engine::commit::sync_parent;

	// Best effort, like the engine's commit: some filesystems (older CIFS) refuse to fsync a folder.
	let _ = sync_parent(target);
	for folder in created {
		let _ = sync_parent(folder);
	}
}

#[cfg(not(unix))]
fn sync_entries(_target: &Path, _created: &[PathBuf]) {}

#[cfg(windows)]
mod native {
	use std::alloc::{self, Layout};
	use std::ffi::OsString;
	use std::fs::{File, OpenOptions};
	use std::io;
	use std::ops::{Deref, DerefMut};
	use std::os::windows::ffi::OsStringExt;
	use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
	use std::os::windows::io::AsRawHandle;
	use std::path::{Path, PathBuf};
	use std::ptr::NonNull;

	use cryarchiver_engine::commit::long_wide;
	use cryarchiver_engine::outcome::is_sharing_violation;
	use windows::Win32::Foundation::HANDLE;
	use windows::Win32::Storage::FileSystem::{
		BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
		FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO,
		FILE_DISPOSITION_INFO_EX, FILE_DISPOSITION_INFO_EX_FLAGS, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING,
		FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN, FILE_INFO_BY_HANDLE_CLASS, FILE_NAME_NORMALIZED,
		FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_WRITE_ATTRIBUTES, FileDispositionInfo, FileDispositionInfoEx,
		GETFINALPATHNAMEBYHANDLE_FLAGS, GetFileInformationByHandle, GetFinalPathNameByHandleW,
		MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, SetFileInformationByHandle, VOLUME_NAME_DOS,
	};
	use windows::core::PCWSTR;

	use super::{Deletion, Expected, NOT_A_REGULAR_FILE, OUTSIDE_THE_SOURCE};
	use crate::record::{Identity, mtime_ns};

	const SECTOR_ALIGNMENT: usize = 4096;

	pub fn delete_checked(path: &Path, root: &Path, expected: &Expected) -> io::Result<Deletion> {
		let file = match OpenOptions::new()
			.access_mode(DELETE.0 | FILE_READ_ATTRIBUTES.0)
			.share_mode(FILE_SHARE_READ.0)
			.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0)
			.open(path)
		{
			Ok(file) => file,
			Err(err) if is_sharing_violation(&err) => return Ok(Deletion::Busy),
			Err(err) => return Err(err),
		};

		let meta = file.metadata()?;
		if meta.file_attributes() & (FILE_ATTRIBUTE_REPARSE_POINT.0 | FILE_ATTRIBUTE_DIRECTORY.0) != 0 {
			return Ok(Deletion::Refused(NOT_A_REGULAR_FILE));
		}

		if !final_path(&file)?.starts_with(root) {
			return Ok(Deletion::Refused(OUTSIDE_THE_SOURCE));
		}

		if !expected.matches(meta.len(), mtime_ns(meta.modified()?), identity(&file)?) {
			return Ok(Deletion::Changed);
		}

		mark_for_deletion(&file)?;
		drop(file);
		Ok(Deletion::Deleted)
	}

	pub fn rename_replacing(from: &Path, to: &Path) -> io::Result<()> {
		let (from, to) = (long_wide(from)?, long_wide(to)?);

		// SAFETY: both buffers are NUL-terminated and outlive the call.
		unsafe {
			MoveFileExW(
				PCWSTR(from.as_ptr()),
				PCWSTR(to.as_ptr()),
				MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
			)
		}
		.map_err(|err| to_io(&err))
	}

	pub fn open_uncached(path: &Path) -> io::Result<File> {
		OpenOptions::new()
			.read(true)
			.custom_flags(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_SEQUENTIAL_SCAN.0)
			.open(path)
	}

	pub fn open_folder_for_times(path: &Path) -> io::Result<File> {
		OpenOptions::new()
			.access_mode(FILE_WRITE_ATTRIBUTES.0)
			.custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
			.open(path)
	}

	/// Zeroed memory aligned for unbuffered reads, whose sizes and offsets must be multiples of the sector size.
	pub struct AlignedBuffer {
		data: NonNull<u8>,
		layout: Layout,
	}

	impl AlignedBuffer {
		pub fn new(size: usize) -> Self {
			let layout = Layout::from_size_align(size, SECTOR_ALIGNMENT).expect("a valid buffer layout");

			// SAFETY: the layout has a non-zero size.
			let data = unsafe { alloc::alloc_zeroed(layout) };
			let data = NonNull::new(data).unwrap_or_else(|| alloc::handle_alloc_error(layout));
			Self { data, layout }
		}
	}

	impl Deref for AlignedBuffer {
		type Target = [u8];

		fn deref(&self) -> &[u8] {
			// SAFETY: the allocation is initialised (zeroed), `layout.size()` bytes long, and owned by `self`.
			unsafe { std::slice::from_raw_parts(self.data.as_ptr(), self.layout.size()) }
		}
	}

	impl DerefMut for AlignedBuffer {
		fn deref_mut(&mut self) -> &mut [u8] {
			// SAFETY: as in `deref`, and `&mut self` makes this the only reference.
			unsafe { std::slice::from_raw_parts_mut(self.data.as_ptr(), self.layout.size()) }
		}
	}

	impl Drop for AlignedBuffer {
		fn drop(&mut self) {
			// SAFETY: `data` came from `alloc_zeroed` with this same layout and is freed once.
			unsafe { alloc::dealloc(self.data.as_ptr(), self.layout) };
		}
	}

	fn identity(file: &File) -> io::Result<Identity> {
		let mut info = BY_HANDLE_FILE_INFORMATION::default();

		// SAFETY: the handle is owned by `file` and valid for the call.
		unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }
			.map_err(|err| to_io(&err))?;
		Ok(Identity {
			volume: u64::from(info.dwVolumeSerialNumber),
			index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
		})
	}

	/// Normalised the way `fs::canonicalize` normalises the source root, so the two compare component-wise.
	fn final_path(file: &File) -> io::Result<PathBuf> {
		let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
		let mut buffer = vec![0u16; 512];

		loop {
			// SAFETY: the handle is owned by `file` and the buffer is writable for its whole length.
			let len = unsafe { GetFinalPathNameByHandleW(HANDLE(file.as_raw_handle()), &mut buffer, flags) } as usize;
			if len == 0 {
				return Err(io::Error::last_os_error());
			}

			if len < buffer.len() {
				buffer.truncate(len);
				return Ok(PathBuf::from(OsString::from_wide(&buffer)));
			}
			buffer.resize(len, 0);
		}
	}

	fn mark_for_deletion(file: &File) -> io::Result<()> {
		const ERROR_INVALID_FUNCTION: i32 = 1;
		const ERROR_NOT_SUPPORTED: i32 = 50;
		const ERROR_INVALID_PARAMETER: i32 = 87;

		let posix = FILE_DISPOSITION_INFO_EX {
			Flags: FILE_DISPOSITION_INFO_EX_FLAGS(
				FILE_DISPOSITION_FLAG_DELETE.0 | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0,
			),
		};
		match set_information(file, FileDispositionInfoEx, &posix) {
			Err(err)
				if matches!(
					err.raw_os_error(),
					Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
				) =>
			{
				set_information(file, FileDispositionInfo, &FILE_DISPOSITION_INFO { DeleteFile: true })
			}
			result => result,
		}
	}

	fn set_information<T>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS, info: &T) -> io::Result<()> {
		let size = u32::try_from(size_of::<T>()).expect("file information structs are small");

		// SAFETY: the handle is owned by `file`; the pointer and size describe `info`, which outlives the call.
		unsafe {
			SetFileInformationByHandle(
				HANDLE(file.as_raw_handle()),
				class,
				std::ptr::from_ref(info).cast(),
				size,
			)
		}
		.map_err(|err| to_io(&err))
	}

	fn to_io(err: &windows::core::Error) -> io::Error {
		io::Error::from_raw_os_error(err.code().0 & 0xFFFF)
	}
}

#[cfg(unix)]
mod native {
	use std::ffi::CString;
	use std::fs::{self, File, OpenOptions};
	use std::io;
	use std::mem::MaybeUninit;
	use std::os::unix::ffi::OsStrExt;
	use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
	use std::os::unix::io::AsRawFd;
	use std::path::Path;

	use super::{Deletion, Expected, NOT_A_REGULAR_FILE, OUTSIDE_THE_SOURCE};
	use crate::record::Identity;

	pub fn delete_checked(path: &Path, root: &Path, expected: &Expected) -> io::Result<Deletion> {
		let Some(name) = path.file_name() else {
			return Ok(Deletion::Refused(NOT_A_REGULAR_FILE));
		};
		let name = CString::new(name.as_bytes())?;
		let parent = path
			.parent()
			.filter(|dir| !dir.as_os_str().is_empty())
			.unwrap_or(Path::new("."));

		let real_parent = fs::canonicalize(parent)?;
		if !real_parent.starts_with(root) {
			return Ok(Deletion::Refused(OUTSIDE_THE_SOURCE));
		}

		let seen = fs::metadata(&real_parent)?;
		let folder = open_folder(&real_parent)?;
		let opened = folder.metadata()?;
		if (opened.dev(), opened.ino()) != (seen.dev(), seen.ino()) {
			return Ok(Deletion::Changed);
		}

		let stat = stat_entry(&folder, &name)?;
		if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
			return Ok(Deletion::Refused(NOT_A_REGULAR_FILE));
		}

		let size = u64::try_from(stat.st_size).unwrap_or(u64::MAX);
		if !expected.matches(size, stat_mtime_ns(&stat), stat_identity(&stat)) {
			return Ok(Deletion::Changed);
		}

		// SAFETY: the folder descriptor is open and the name is NUL-terminated.
		if unsafe { libc::unlinkat(folder.as_raw_fd(), name.as_ptr(), 0) } != 0 {
			return Err(io::Error::last_os_error());
		}
		Ok(Deletion::Deleted)
	}

	pub fn rename_replacing(from: &Path, to: &Path) -> io::Result<()> {
		fs::rename(from, to)
	}

	/// The fsync writes back any dirty pages so that dropping the cached ones leaves nothing to read but the medium.
	pub fn open_uncached(path: &Path) -> io::Result<File> {
		let file = File::open(path)?;

		#[cfg(target_os = "linux")]
		{
			file.sync_all()?;

			// SAFETY: the descriptor is owned by `file` and open for the call.
			let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
			if rc != 0 {
				return Err(io::Error::from_raw_os_error(rc));
			}
		}

		Ok(file)
	}

	pub fn open_folder_for_times(path: &Path) -> io::Result<File> {
		open_folder(path)
	}

	fn open_folder(path: &Path) -> io::Result<File> {
		OpenOptions::new()
			.read(true)
			.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
			.open(path)
	}

	fn stat_entry(folder: &File, name: &CString) -> io::Result<libc::stat> {
		let mut stat = MaybeUninit::<libc::stat>::uninit();

		// SAFETY: the folder descriptor is open, the name is NUL-terminated and `stat` is writable.
		let rc = unsafe {
			libc::fstatat(
				folder.as_raw_fd(),
				name.as_ptr(),
				stat.as_mut_ptr(),
				libc::AT_SYMLINK_NOFOLLOW,
			)
		};
		if rc != 0 {
			return Err(io::Error::last_os_error());
		}

		// SAFETY: fstatat succeeded, so it filled `stat`.
		Ok(unsafe { stat.assume_init() })
	}

	/// The same value `record::mtime_ns` gives for the `SystemTime` std builds from these fields.
	fn stat_mtime_ns(stat: &libc::stat) -> i64 {
		let nanos = i128::from(stat.st_mtime) * 1_000_000_000 + i128::from(stat.st_mtime_nsec);
		i64::try_from(nanos).unwrap_or(if nanos < 0 { i64::MIN } else { i64::MAX })
	}

	/// Built like std's `MetadataExt::dev` and `ino`, which the engine's `file_id` uses.
	#[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
	fn stat_identity(stat: &libc::stat) -> Identity {
		Identity {
			volume: stat.st_dev as u64,
			index: stat.st_ino as u64,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, SystemTime};

	use cryarchiver_engine::commit::file_id;

	use super::*;

	fn old_time() -> SystemTime {
		SystemTime::UNIX_EPOCH + Duration::from_secs(1_234_567_891)
	}

	fn expected(path: &Path) -> Expected {
		let meta = fs::metadata(path).unwrap();
		Expected {
			size: meta.len(),
			mtime_ns: mtime_ns(meta.modified().unwrap()),
			identity: Some(file_id(path).unwrap().into()),
		}
	}

	fn set_modified(path: &Path, time: SystemTime) {
		File::options()
			.write(true)
			.open(path)
			.unwrap()
			.set_modified(time)
			.unwrap();
	}

	#[cfg(windows)]
	fn junction(link: &Path, target: &Path) -> bool {
		std::process::Command::new("cmd")
			.arg("/C")
			.arg("mklink")
			.arg("/J")
			.arg(link)
			.arg(target)
			.output()
			.is_ok_and(|out| out.status.success())
	}

	#[test]
	fn copies_are_hashed_and_keep_the_source_times() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.txt");
		fs::write(&source, b"hello archive").unwrap();
		set_modified(&source, old_time());
		let staged_path = dir.path().join("staged").join("x.txt");

		let staged = copy_to_staged(&source, &staged_path).unwrap();

		assert_eq!(fs::read(&staged_path).unwrap(), b"hello archive");
		assert_eq!(staged.size, 13);
		assert_eq!(staged.hash, Hash(*blake3::hash(b"hello archive").as_bytes()));
		assert_eq!(fs::metadata(&staged_path).unwrap().modified().unwrap(), old_time());
		assert!(copy_to_staged(&source, &staged_path).is_err());
		assert_eq!(fs::read(&staged_path).unwrap(), b"hello archive");
	}

	#[test]
	fn a_failed_copy_leaves_nothing_staged() {
		let dir = tempfile::tempdir().unwrap();
		let folder = dir.path().join("folder");
		fs::create_dir_all(&folder).unwrap();
		let staged_path = dir.path().join("staged").join("x.txt");

		assert!(copy_to_staged(&folder, &staged_path).is_err());
		assert!(fs::symlink_metadata(&staged_path).is_err());

		#[cfg(windows)]
		{
			let source = dir.path().join("locked.txt");
			fs::write(&source, b"locked content").unwrap();
			let holder = File::options().read(true).write(true).open(&source).unwrap();
			holder.lock().unwrap();

			assert!(copy_to_staged(&source, &staged_path).is_err());
			assert!(fs::symlink_metadata(&staged_path).is_err());
		}
	}

	#[test]
	fn a_failed_write_leaves_nothing_staged() {
		let dir = tempfile::tempdir().unwrap();
		let not_a_folder = dir.path().join("staging");
		fs::write(&not_a_folder, b"a file").unwrap();

		assert!(write_staged(&not_a_folder.join("x.txt"), b"content", None).is_err());
		assert_eq!(fs::read(&not_a_folder).unwrap(), b"a file");

		#[cfg(windows)]
		{
			let staged_path = dir.path().join("staged").join("x.txt");
			let unsettable = SystemTime::UNIX_EPOCH - Duration::from_hours(3_234_576);
			let times = SourceTimes {
				accessed: unsettable,
				modified: unsettable,
				created: None,
			};

			assert!(write_staged(&staged_path, b"content", Some(times)).is_err());
			assert!(fs::symlink_metadata(&staged_path).is_err());
		}
	}

	#[test]
	fn written_files_keep_the_given_times() {
		let dir = tempfile::tempdir().unwrap();
		let staged_path = dir.path().join("staged").join("x.txt");
		let times = SourceTimes {
			accessed: old_time(),
			modified: old_time(),
			created: None,
		};

		let staged = write_staged(&staged_path, b"placeholder", Some(times)).unwrap();

		assert_eq!(fs::read(&staged_path).unwrap(), b"placeholder");
		assert_eq!(staged.size, 11);
		assert_eq!(staged.hash, Hash(*blake3::hash(b"placeholder").as_bytes()));
		assert_eq!(fs::metadata(&staged_path).unwrap().modified().unwrap(), old_time());
	}

	#[test]
	fn staged_files_are_hashed() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("encoded.avif");
		fs::write(&path, b"encoded bytes").unwrap();

		let staged = hash_staged(&path).unwrap();

		assert_eq!(
			staged,
			Staged {
				path: path.clone(),
				size: 13,
				hash: Hash(*blake3::hash(b"encoded bytes").as_bytes()),
			}
		);
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
		assert!(!staged.exists());
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
	fn read_back_reads_files_larger_than_its_buffer() {
		let dir = tempfile::tempdir().unwrap();
		let output = dir.path().join("a.mkv");
		let mut content: Vec<u8> = (0..READ_BACK_BUFFER + 12_345)
			.map(|i| u8::try_from(i % 251).unwrap())
			.collect();
		fs::write(&output, &content).unwrap();
		let size = content.len() as u64;
		let hash = Hash(*blake3::hash(&content).as_bytes());

		assert!(read_back(&output, size, hash).unwrap());
		assert!(!read_back(&output, size + 1, hash).unwrap());

		*content.last_mut().unwrap() ^= 1;
		fs::write(&output, &content).unwrap();
		assert!(!read_back(&output, size, hash).unwrap());
	}

	#[test]
	fn originals_are_deleted_only_inside_the_source() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("source");
		fs::create_dir_all(root.join("sub")).unwrap();
		let inside = root.join("sub").join("a.jpg");
		fs::write(&inside, b"a").unwrap();
		let archived = expected(&inside);

		assert_eq!(delete_original(&inside, &root, &archived).unwrap(), Deletion::Deleted);
		assert!(!inside.exists());
		assert_eq!(
			delete_original(&inside, &root, &archived).unwrap(),
			Deletion::AlreadyGone
		);
		assert_eq!(
			delete_original(&root.join("gone").join("a.jpg"), &root, &archived).unwrap(),
			Deletion::AlreadyGone
		);
		assert!(delete_original(&inside, &dir.path().join("unplugged"), &archived).is_err());
		assert_eq!(
			delete_original(&root.join("sub"), &root, &archived).unwrap(),
			Deletion::Refused("not a regular file")
		);
		assert!(root.join("sub").is_dir());

		let outside = dir.path().join("elsewhere.jpg");
		fs::write(&outside, b"b").unwrap();
		assert_eq!(
			delete_original(&outside, &root, &expected(&outside)).unwrap(),
			Deletion::Refused("its real path is outside the source folder")
		);
		assert!(outside.exists());
	}

	#[test]
	fn an_original_that_changed_is_never_deleted() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("source");
		fs::create_dir_all(&root).unwrap();
		let original = root.join("a.jpg");
		fs::write(&original, b"abc").unwrap();

		let archived = expected(&original);
		File::options()
			.append(true)
			.open(&original)
			.unwrap()
			.write_all(b"d")
			.unwrap();
		set_modified(&original, old_time());
		assert_eq!(delete_original(&original, &root, &archived).unwrap(), Deletion::Changed);
		assert_eq!(fs::read(&original).unwrap(), b"abcd");

		let archived = expected(&original);
		set_modified(&original, old_time() + Duration::from_secs(1));
		assert_eq!(delete_original(&original, &root, &archived).unwrap(), Deletion::Changed);
		assert_eq!(fs::read(&original).unwrap(), b"abcd");

		let archived = expected(&original);
		let replacement = root.join("b.jpg");
		fs::write(&replacement, b"abcd").unwrap();
		set_modified(&replacement, fs::metadata(&original).unwrap().modified().unwrap());
		fs::rename(&replacement, &original).unwrap();
		let now = expected(&original);
		assert_eq!((now.size, now.mtime_ns), (archived.size, archived.mtime_ns));
		assert_ne!(now.identity, archived.identity);
		assert_eq!(delete_original(&original, &root, &archived).unwrap(), Deletion::Changed);
		assert!(original.exists());

		let unidentified = Expected {
			identity: None,
			..archived
		};
		assert_eq!(
			delete_original(&original, &root, &unidentified).unwrap(),
			Deletion::Deleted
		);
		assert!(!original.exists());
	}

	#[cfg(windows)]
	#[test]
	fn an_original_another_program_has_open_is_busy() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("source");
		fs::create_dir_all(&root).unwrap();
		let original = root.join("a.mkv");
		fs::write(&original, b"recording").unwrap();
		let archived = expected(&original);

		let writer = File::options().append(true).open(&original).unwrap();
		assert_eq!(delete_original(&original, &root, &archived).unwrap(), Deletion::Busy);
		assert!(original.exists());

		drop(writer);
		assert_eq!(delete_original(&original, &root, &archived).unwrap(), Deletion::Deleted);
		assert!(!original.exists());
	}

	#[test]
	fn a_link_is_never_deleted_or_followed() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("source");
		fs::create_dir_all(&root).unwrap();
		let target = root.join("target.jpg");
		fs::write(&target, b"target").unwrap();
		let link = root.join("link.jpg");

		#[cfg(unix)]
		let linked = std::os::unix::fs::symlink(&target, &link).is_ok();
		#[cfg(windows)]
		let linked = std::os::windows::fs::symlink_file(&target, &link).is_ok();
		if linked {
			assert_eq!(
				delete_original(&link, &root, &expected(&target)).unwrap(),
				Deletion::Refused("not a regular file")
			);
			assert!(fs::symlink_metadata(&link).is_ok());
			assert_eq!(fs::read(&target).unwrap(), b"target");
		} else {
			eprintln!("skipped the file link: can't create one here");
		}

		#[cfg(windows)]
		{
			let folder = root.join("folder");
			fs::create_dir_all(&folder).unwrap();
			fs::write(folder.join("a.jpg"), b"a").unwrap();
			let joined = root.join("joined");
			if junction(&joined, &folder) {
				assert_eq!(
					delete_original(&joined, &root, &expected(&target)).unwrap(),
					Deletion::Refused("not a regular file")
				);
				assert!(fs::symlink_metadata(&joined).is_ok());
				assert!(folder.join("a.jpg").exists());
			} else {
				eprintln!("skipped the junction: can't create one here");
			}
		}
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
		let linked = junction(&root.join("linked"), &elsewhere);
		if !linked {
			eprintln!("skipped: can't create a folder link here");
			return;
		}

		let through_link = root.join("linked").join("a.jpg");
		assert_eq!(
			delete_original(&through_link, &root, &expected(&through_link)).unwrap(),
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

	#[test]
	fn folder_times_never_follow_links() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		let destination = dir.path().join("destination");
		let elsewhere = dir.path().join("elsewhere");
		fs::create_dir_all(&source).unwrap();
		fs::create_dir_all(&destination).unwrap();
		fs::create_dir_all(&elsewhere).unwrap();
		open_folder_for_times(&source)
			.unwrap()
			.set_times(fs::FileTimes::new().set_modified(old_time()))
			.unwrap();

		let linked_destination = dir.path().join("linked-destination");
		let linked_source = dir.path().join("linked-source");
		#[cfg(unix)]
		let linked = std::os::unix::fs::symlink(&elsewhere, &linked_destination).is_ok()
			&& std::os::unix::fs::symlink(&source, &linked_source).is_ok();
		#[cfg(windows)]
		let linked = junction(&linked_destination, &elsewhere) && junction(&linked_source, &source);
		if !linked {
			eprintln!("skipped: can't create a folder link here");
			return;
		}

		let _ = apply_folder_times(&source, &linked_destination);
		assert_ne!(fs::metadata(&elsewhere).unwrap().modified().unwrap(), old_time());

		apply_folder_times(&linked_source, &destination).unwrap();
		assert_ne!(fs::metadata(&destination).unwrap().modified().unwrap(), old_time());
	}
}
