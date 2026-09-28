use std::fs::{self, File, FileType, Metadata, OpenOptions, TryLockError};
use std::io;
use std::path::{Component, Path, PathBuf};

use ulid::Ulid;

use crate::commit::retry_transient;
use crate::text::display_path;

pub const CLI_WORK_DIR: &str = ".cry-work";
pub const ARCHIVE_DIR: &str = ".cryarchiver";
pub const ARCHIVE_WORK_DIR: &str = "work";
pub const STAGED_DIR: &str = "staged";

pub const IS_A_LINK: &str = "is a link or a junction, and nothing is written or removed through one";
pub const OUTSIDE_THE_DESTINATION: &str = "leads out of the destination";

const LOCK_FILE: &str = "job.lock";

pub struct Workspace {
	dir: PathBuf,
	root: PathBuf,
	lock: Option<File>,
}

impl Workspace {
	pub fn create(root: &Path, source: &Path) -> io::Result<Self> {
		if fs::symlink_metadata(root).is_ok_and(|meta| is_link(&meta)) {
			return Err(UnsafeFolder::error(root, IS_A_LINK));
		}

		let dir = root.join(Ulid::generate().to_string());
		fs::create_dir_all(&dir)?;

		let lock = OpenOptions::new()
			.create(true)
			.write(true)
			.truncate(true)
			.open(dir.join(LOCK_FILE))?;
		match lock.try_lock() {
			Ok(()) | Err(TryLockError::Error(_)) => {
				// Filesystems without lock support report an error; the job then runs unlocked
				// and sweep treats its folder as live.
			}
			Err(TryLockError::WouldBlock) => return Err(io::Error::other("workspace is locked by another job")),
		}

		let info = serde_json::json!({
			"source": source.to_string_lossy(),
			"pid": std::process::id(),
			"version": crate::VERSION,
		});
		fs::write(dir.join("job.json"), info.to_string())?;

		Ok(Self {
			dir,
			root: root.to_path_buf(),
			lock: Some(lock),
		})
	}

	pub fn dir(&self) -> &Path {
		&self.dir
	}

	pub fn file(&self, name: &str) -> PathBuf {
		self.dir.join(name)
	}
}

impl Drop for Workspace {
	fn drop(&mut self) {
		drop(self.lock.take());
		remove_dir(&self.dir);
		let _ = fs::remove_dir(&self.root);
	}
}

/// Removes the abandoned job folders in `root`, and any link found there: the link itself, never what it leads to.
/// Nothing is listed through a `root` that is itself a link.
pub fn sweep(root: &Path) -> usize {
	if real_folder(root).is_err() {
		return 0;
	}
	let Ok(entries) = fs::read_dir(root) else { return 0 };

	let mut removed = 0;
	for entry in entries.flatten() {
		let Ok(kind) = entry.file_type() else {
			continue;
		};
		let path = entry.path();

		if kind.is_symlink() {
			if remove_link(&path, kind).is_ok() {
				removed += 1;
			}
		} else if kind.is_dir() && is_abandoned(&path) {
			remove_dir(&path);
			removed += 1;
		}
	}

	let _ = fs::remove_dir(root);
	removed
}

/// Whether the entry itself leads elsewhere: a symbolic link, or on Windows a junction or a mount point. Other Windows
/// reparse points, such as cloud-file placeholders, hold their own content and redirect nothing.
pub fn is_link(meta: &Metadata) -> bool {
	meta.is_symlink()
}

/// A folder the archive never writes into or removes anything through.
#[derive(Debug, thiserror::Error)]
#[error("{} {reason}", display_path(.path))]
pub struct UnsafeFolder {
	pub path: PathBuf,
	pub reason: &'static str,
}

impl UnsafeFolder {
	pub fn error(path: &Path, reason: &'static str) -> io::Error {
		io::Error::other(Self {
			path: path.to_path_buf(),
			reason,
		})
	}

	pub fn of(err: &io::Error) -> Option<&Self> {
		err.get_ref()?.downcast_ref()
	}
}

/// Creates what `folder` lacks below `base`, one name at a time and never through a link: `base` and every name below
/// it that is already there must be a real folder. `before_making` checks the folder each new name goes into, before
/// it is made. Returns the folders made, outermost first.
pub fn create_real_folders(
	base: &Path,
	folder: &Path,
	mut before_making: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<Vec<PathBuf>> {
	walk_real_folders(base, folder, Some(&mut before_making))
}

/// Whether `base` and every name of `folder` below it is a real folder, reached without a link.
pub fn check_real_folders(base: &Path, folder: &Path) -> io::Result<()> {
	walk_real_folders(base, folder, None).map(drop)
}

type BeforeMaking<'a> = &'a mut dyn FnMut(&Path) -> io::Result<()>;

fn walk_real_folders(base: &Path, folder: &Path, mut before_making: Option<BeforeMaking>) -> io::Result<Vec<PathBuf>> {
	let below = folder
		.strip_prefix(base)
		.map_err(|_| UnsafeFolder::error(folder, OUTSIDE_THE_DESTINATION))?;
	real_folder(base)?;

	let mut made = Vec::new();
	let mut current = base.to_path_buf();
	for component in below.components() {
		let Component::Normal(name) = component else {
			return Err(UnsafeFolder::error(folder, OUTSIDE_THE_DESTINATION));
		};
		let parent = current.clone();
		current.push(name);

		match real_folder(&current) {
			Err(err) if err.kind() == io::ErrorKind::NotFound => {
				let Some(before_making) = before_making.as_mut() else {
					return Err(err);
				};
				before_making(&parent)?;

				match fs::create_dir(&current) {
					Ok(()) => made.push(current.clone()),
					Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
					Err(err) => return Err(err),
				}
				real_folder(&current)?;
			}
			result => result?,
		}
	}

	Ok(made)
}

fn real_folder(path: &Path) -> io::Result<()> {
	let meta = fs::symlink_metadata(path)?;
	if is_link(&meta) {
		return Err(UnsafeFolder::error(path, IS_A_LINK));
	}

	if !meta.is_dir() {
		let message = format!("{}: not a folder", display_path(path));
		return Err(io::Error::new(io::ErrorKind::NotADirectory, message));
	}
	Ok(())
}

fn remove_link(path: &Path, kind: FileType) -> io::Result<()> {
	#[cfg(windows)]
	if std::os::windows::fs::FileTypeExt::is_symlink_dir(&kind) {
		return fs::remove_dir(path);
	}

	#[cfg(not(windows))]
	let _ = kind;
	fs::remove_file(path)
}

fn is_abandoned(dir: &Path) -> bool {
	let lock = match open_lock(&dir.join(LOCK_FILE)) {
		Ok(lock) => lock,
		Err(err) => return err.kind() == io::ErrorKind::NotFound,
	};
	lock.try_lock().is_ok()
}

/// Opens a link in the lock's place, never what it leads to.
fn open_lock(path: &Path) -> io::Result<File> {
	let mut options = OpenOptions::new();
	options.write(true);

	#[cfg(windows)]
	{
		const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
		std::os::windows::fs::OpenOptionsExt::custom_flags(&mut options, FILE_FLAG_OPEN_REPARSE_POINT);
	}
	#[cfg(unix)]
	std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);

	options.open(path)
}

fn remove_dir(dir: &Path) {
	let _ = retry_transient(|| match fs::remove_dir_all(dir) {
		Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
		result => result,
	});
}
