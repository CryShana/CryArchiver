use std::fs::{self, File, FileTimes, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::outcome::{ErrorKind, is_sharing_violation};
use crate::text::display_path;

const TRANSIENT_ATTEMPTS: u32 = 6;

#[derive(Debug, Clone)]
pub struct Artifact {
	pub path: PathBuf,
	pub source: PathBuf,
	pub times: Option<SourceTimes>,
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
	#[error("{} already exists", display_path(.0))]
	Exists(PathBuf),
	#[error("{} is the source file", display_path(.0))]
	IsSource(PathBuf),
	#[error(transparent)]
	Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
	Never,
	ExistingOutput,
}

pub trait OutputSink: Send + Sync {
	fn work_root(&self, source: &Path) -> PathBuf;

	fn commit(&self, artifact: &Artifact) -> Result<PathBuf, CommitError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileIdentity {
	pub volume: u64,
	pub index: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTimes {
	pub accessed: SystemTime,
	pub modified: SystemTime,
	pub created: Option<SystemTime>,
}

impl SourceTimes {
	pub fn from_metadata(meta: &Metadata) -> io::Result<Self> {
		Ok(Self {
			accessed: meta.accessed()?,
			modified: meta.modified()?,
			created: meta.created().ok(),
		})
	}

	pub fn file_times(&self) -> FileTimes {
		let times = FileTimes::new().set_accessed(self.accessed).set_modified(self.modified);

		#[cfg(windows)]
		let times = match self.created {
			Some(created) => std::os::windows::fs::FileTimesExt::set_created(times, created),
			None => times,
		};
		times
	}
}

pub fn commit_file(artifact: &Artifact, target: &Path, replace: Replace) -> Result<(), CommitError> {
	let file = File::options().write(true).open(&artifact.path)?;
	if let Some(times) = &artifact.times {
		file.set_times(times.file_times())?;
	}
	file.sync_all()?;
	drop(file);

	match replace {
		Replace::Never => match retry_transient(|| rename_no_replace(&artifact.path, target)) {
			Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
				return Err(CommitError::Exists(target.to_path_buf()));
			}
			result => result?,
		},
		Replace::ExistingOutput => {
			if target.exists() && same_file(target, &artifact.source).unwrap_or(true) {
				return Err(CommitError::IsSource(target.to_path_buf()));
			}
			retry_transient(|| fs::rename(&artifact.path, target))?;
		}
	}

	// Best effort: the output is already in place, and some filesystems (older CIFS) refuse to fsync a directory.
	#[cfg(unix)]
	let _ = sync_parent(target);
	Ok(())
}

pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
	match native::rename_no_replace(from, to) {
		Err(err) if native::is_unsupported(&err) => fallback_no_replace(from, to),
		result => result,
	}
}

pub fn fallback_no_replace(from: &Path, to: &Path) -> io::Result<()> {
	match fs::hard_link(from, to) {
		Ok(()) => fs::remove_file(from),
		Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(err),
		// FAT and exFAT have no hard links; a check-then-rename is the best they allow.
		Err(_) if fs::symlink_metadata(to).is_ok() => Err(io::Error::from(io::ErrorKind::AlreadyExists)),
		Err(_) => fs::rename(from, to),
	}
}

pub fn check_space(dir: &Path, source_bytes: u64) -> Result<(), ErrorKind> {
	let need = source_bytes.saturating_add(source_bytes / 10);
	let free = free_space(dir).unwrap_or(u64::MAX);

	if free < need {
		return Err(ErrorKind::InsufficientSpace { need, free });
	}
	Ok(())
}

pub fn free_space(path: &Path) -> io::Result<u64> {
	let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
	let existing = absolute.ancestors().find(|dir| dir.exists()).unwrap_or(&absolute);
	native::free_space(existing)
}

pub fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
	Ok(file_id(a)? == file_id(b)?)
}

/// Follows links, and works for folders as well as files.
pub fn file_id(path: &Path) -> io::Result<FileIdentity> {
	native::file_id(path)
}

#[cfg(windows)]
pub use native::long_wide;

pub fn retry_transient<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
	let mut delay = Duration::from_millis(50);
	for _ in 1..TRANSIENT_ATTEMPTS {
		match op() {
			Err(err) if is_transient(&err) => thread::sleep(delay),
			result => return result,
		}
		delay *= 2;
	}

	op()
}

// Antivirus scanners and the search indexer briefly hold new files open on Windows. While they do, a deleted
// file stays "delete pending": opening it reports access denied and removing its folder reports not empty.
fn is_transient(err: &io::Error) -> bool {
	const ERROR_ACCESS_DENIED: i32 = 5;
	const ERROR_DIR_NOT_EMPTY: i32 = 145;

	is_sharing_violation(err)
		|| (cfg!(windows) && matches!(err.raw_os_error(), Some(ERROR_ACCESS_DENIED | ERROR_DIR_NOT_EMPTY)))
}

#[cfg(unix)]
pub fn sync_parent(path: &Path) -> io::Result<()> {
	let dir = path
		.parent()
		.filter(|dir| !dir.as_os_str().is_empty())
		.unwrap_or(Path::new("."));
	File::open(dir)?.sync_all()
}

#[cfg(windows)]
mod native {
	use std::fs::OpenOptions;
	use std::io;
	use std::os::windows::ffi::OsStrExt;
	use std::os::windows::fs::OpenOptionsExt;
	use std::os::windows::io::AsRawHandle;
	use std::path::Path;

	use windows::Win32::Foundation::HANDLE;
	use windows::Win32::Storage::FileSystem::{
		BY_HANDLE_FILE_INFORMATION, GetDiskFreeSpaceExW, GetFileInformationByHandle, MOVEFILE_WRITE_THROUGH,
		MoveFileExW,
	};
	use windows::core::PCWSTR;

	use super::FileIdentity;

	pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
		let (from, to) = (long_wide(from)?, long_wide(to)?);

		// SAFETY: both buffers are NUL-terminated and outlive the call.
		unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), MOVEFILE_WRITE_THROUGH) }
			.map_err(|err| to_io(&err))
	}

	pub fn is_unsupported(_err: &io::Error) -> bool {
		false
	}

	pub fn free_space(dir: &Path) -> io::Result<u64> {
		let dir = directory_wide(dir)?;
		let mut available = 0u64;

		// SAFETY: the out pointer is valid for the duration of the call.
		unsafe { GetDiskFreeSpaceExW(PCWSTR(dir.as_ptr()), Some(&raw mut available), None, None) }
			.map_err(|err| to_io(&err))?;
		Ok(available)
	}

	pub fn file_id(path: &Path) -> io::Result<FileIdentity> {
		const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

		let file = OpenOptions::new()
			.access_mode(0)
			.custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
			.open(path)?;
		let mut info = BY_HANDLE_FILE_INFORMATION::default();

		// SAFETY: the handle is owned by `file` and valid for the call.
		unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }
			.map_err(|err| to_io(&err))?;
		Ok(FileIdentity {
			volume: u64::from(info.dwVolumeSerialNumber),
			index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
		})
	}

	// Win32 calls outside std are limited to MAX_PATH unless the path carries the \\?\ prefix.
	pub fn long_wide(path: &Path) -> io::Result<Vec<u16>> {
		const BACKSLASH: u16 = b'\\' as u16;
		const QUESTION: u16 = b'?' as u16;

		let absolute = std::path::absolute(path)?;
		let text: Vec<u16> = absolute.as_os_str().encode_wide().collect();
		let prefixed = text.starts_with(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH]);
		let unc = !prefixed && text.starts_with(&[BACKSLASH, BACKSLASH]);

		let mut wide: Vec<u16> = match (prefixed, unc) {
			(true, _) => Vec::new(),
			(false, true) => "\\\\?\\UNC".encode_utf16().collect(),
			(false, false) => "\\\\?\\".encode_utf16().collect(),
		};
		wide.extend_from_slice(if unc { &text[1..] } else { &text[..] });
		wide.push(0);
		Ok(wide)
	}

	// GetDiskFreeSpaceExW rejects a bare UNC share root (e.g. `\\server\share`) unless it carries a
	// trailing backslash; a drive path accepts both forms, so a missing backslash is added uniformly.
	fn directory_wide(dir: &Path) -> io::Result<Vec<u16>> {
		const BACKSLASH: u16 = b'\\' as u16;

		let mut wide = long_wide(dir)?;
		if wide[wide.len() - 2] != BACKSLASH {
			wide.insert(wide.len() - 1, BACKSLASH);
		}
		Ok(wide)
	}

	fn to_io(err: &windows::core::Error) -> io::Error {
		io::Error::from_raw_os_error(err.code().0 & 0xFFFF)
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		fn decode(wide: &[u16]) -> String {
			String::from_utf16(wide).unwrap()
		}

		#[test]
		fn appends_a_trailing_backslash_for_a_unc_share_root() {
			let wide = directory_wide(Path::new(r"\\server\share")).unwrap();

			assert_eq!(decode(&wide), "\\\\?\\UNC\\server\\share\\\0");
		}

		#[test]
		fn appends_a_trailing_backslash_for_a_drive_path() {
			let wide = directory_wide(Path::new(r"C:\music")).unwrap();

			assert_eq!(decode(&wide), "\\\\?\\C:\\music\\\0");
		}

		#[test]
		fn does_not_double_an_existing_trailing_backslash() {
			let wide = directory_wide(Path::new(r"C:\music\")).unwrap();

			assert_eq!(decode(&wide), "\\\\?\\C:\\music\\\0");
		}
	}
}

#[cfg(unix)]
mod native {
	use std::ffi::CString;
	use std::io;
	use std::os::unix::ffi::OsStrExt;
	use std::os::unix::fs::MetadataExt;
	use std::path::Path;

	use super::FileIdentity;

	#[cfg(target_os = "linux")]
	pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
		let from = CString::new(from.as_os_str().as_bytes())?;
		let to = CString::new(to.as_os_str().as_bytes())?;

		// SAFETY: valid NUL-terminated paths; AT_FDCWD resolves relative paths like rename(2).
		let rc = unsafe {
			libc::syscall(
				libc::SYS_renameat2,
				libc::AT_FDCWD,
				from.as_ptr(),
				libc::AT_FDCWD,
				to.as_ptr(),
				libc::RENAME_NOREPLACE,
			)
		};
		if rc == 0 {
			Ok(())
		} else {
			Err(io::Error::last_os_error())
		}
	}

	#[cfg(not(target_os = "linux"))]
	pub fn rename_no_replace(_from: &Path, _to: &Path) -> io::Result<()> {
		Err(io::Error::from(io::ErrorKind::Unsupported))
	}

	pub fn is_unsupported(err: &io::Error) -> bool {
		err.kind() == io::ErrorKind::Unsupported
			|| matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP))
	}

	#[allow(clippy::useless_conversion)]
	pub fn free_space(dir: &Path) -> io::Result<u64> {
		let dir = CString::new(dir.as_os_str().as_bytes())?;
		// SAFETY: statvfs fills a zeroed struct for a valid path.
		unsafe {
			let mut stat: libc::statvfs = std::mem::zeroed();
			if libc::statvfs(dir.as_ptr(), &raw mut stat) != 0 {
				return Err(io::Error::last_os_error());
			}
			Ok(u64::from(stat.f_bavail) * u64::from(stat.f_frsize))
		}
	}

	pub fn file_id(path: &Path) -> io::Result<FileIdentity> {
		let meta = std::fs::metadata(path)?;
		Ok(FileIdentity {
			volume: meta.dev(),
			index: meta.ino(),
		})
	}
}
