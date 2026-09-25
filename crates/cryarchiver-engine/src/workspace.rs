use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use ulid::Ulid;

use crate::commit::retry_transient;

pub const CLI_WORK_DIR: &str = ".cry-work";

const LOCK_FILE: &str = "job.lock";

pub struct Workspace {
	dir: PathBuf,
	root: PathBuf,
	lock: Option<File>,
}

impl Workspace {
	pub fn create(root: &Path, source: &Path) -> io::Result<Self> {
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

pub fn sweep(root: &Path) -> usize {
	let Ok(entries) = fs::read_dir(root) else { return 0 };

	let mut removed = 0;
	for dir in entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()) {
		if is_abandoned(&dir) {
			remove_dir(&dir);
			removed += 1;
		}
	}

	let _ = fs::remove_dir(root);
	removed
}

fn is_abandoned(dir: &Path) -> bool {
	let lock = match OpenOptions::new().write(true).open(dir.join(LOCK_FILE)) {
		Ok(lock) => lock,
		Err(err) => return err.kind() == io::ErrorKind::NotFound,
	};
	lock.try_lock().is_ok()
}

fn remove_dir(dir: &Path) {
	let _ = retry_transient(|| match fs::remove_dir_all(dir) {
		Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
		result => result,
	});
}
