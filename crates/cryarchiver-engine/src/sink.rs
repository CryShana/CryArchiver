use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ulid::Ulid;

use crate::commit::{Artifact, CommitError, OutputSink, Replace, commit_file};
use crate::container::OUTPUT_EXTENSIONS;
use crate::naming::{cli_output_dir, cli_output_path};
use crate::workspace::{ARCHIVE_DIR, ARCHIVE_WORK_DIR, CLI_WORK_DIR, CheckedFolders, RealFolders, STAGED_DIR};

#[derive(Debug, Clone)]
pub struct CliSink {
	pub output_dir: Option<PathBuf>,
	pub overwrite: bool,
}

impl CliSink {
	pub fn existing_output(&self, source: &Path) -> Option<PathBuf> {
		if self.overwrite {
			return None;
		}

		OUTPUT_EXTENSIONS
			.iter()
			.map(|extension| cli_output_path(source, self.output_dir.as_deref(), extension))
			.find(|path| path.exists())
	}
}

impl OutputSink for CliSink {
	fn work_root(&self, source: &Path) -> PathBuf {
		// The workspace must be on the output's filesystem so the commit is a rename.
		cli_output_dir(source, self.output_dir.as_deref()).join(CLI_WORK_DIR)
	}

	fn commit(&self, artifact: &Artifact) -> Result<PathBuf, CommitError> {
		let extension = artifact.path.extension().unwrap_or_default();
		let target = cli_output_path(&artifact.source, self.output_dir.as_deref(), extension);
		if let Some(dir) = target.parent().filter(|dir| !dir.as_os_str().is_empty()) {
			fs::create_dir_all(dir)?;
		}

		let replace = if self.overwrite {
			Replace::ExistingOutput
		} else {
			Replace::Never
		};
		commit_file(artifact, &target, replace)?;
		Ok(target)
	}
}

/// The archival sink stages each finished artifact, with the source's timestamps and fsynced, in
/// `<destination>/.cryarchiver/work/staged/`, and returns that path. The project layer records its commit intent and
/// then renames the staged file into the mirrored tree, on the same filesystem.
#[derive(Debug, Clone)]
pub struct ArchivalSink {
	destination: PathBuf,
	folders: Arc<dyn CheckedFolders>,
}

impl ArchivalSink {
	/// Its folders are made and used only while every one from the destination down is a real folder.
	pub fn new(destination: &Path) -> Self {
		Self::checked_by(destination, Arc::new(RealFolders(destination.to_path_buf())))
	}

	/// Every folder the sink or a job's workspace makes, or removes anything from, passes `folders` first.
	pub fn checked_by(destination: &Path, folders: Arc<dyn CheckedFolders>) -> Self {
		Self {
			destination: destination.to_path_buf(),
			folders,
		}
	}

	pub fn archive_work_root(&self) -> PathBuf {
		self.destination.join(ARCHIVE_DIR).join(ARCHIVE_WORK_DIR)
	}

	pub fn staged_dir(&self) -> PathBuf {
		self.archive_work_root().join(STAGED_DIR)
	}

	pub fn staged_path(&self, extension: &str) -> PathBuf {
		let mut name = OsString::from(Ulid::generate().to_string());
		if !extension.is_empty() {
			name.push(".");
			name.push(extension);
		}
		self.staged_dir().join(name)
	}
}

impl OutputSink for ArchivalSink {
	fn work_root(&self, _source: &Path) -> PathBuf {
		self.archive_work_root()
	}

	fn work_folders(&self) -> Option<Arc<dyn CheckedFolders>> {
		Some(self.folders.clone())
	}

	fn commit(&self, artifact: &Artifact) -> Result<PathBuf, CommitError> {
		let extension = artifact.path.extension().unwrap_or_default().to_string_lossy();
		let staged = self.staged_path(&extension);
		self.folders.prepare(&self.staged_dir())?;

		commit_file(artifact, &staged, Replace::Never)?;
		Ok(staged)
	}
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::Path;
	use std::time::{Duration, SystemTime};

	use crate::commit::Artifact;
	use crate::commit::CommitError;
	use crate::commit::SourceTimes;
	use crate::workspace::CLI_WORK_DIR;

	use super::*;

	#[test]
	fn the_archival_sink_stages_with_the_source_times() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.flac");
		let work = dir.path().join("work-output.opus");
		fs::write(&source, b"src").unwrap();
		fs::write(&work, b"encoded").unwrap();
		let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
		fs::create_dir(dir.path().join("archive")).unwrap();
		let sink = ArchivalSink::new(&dir.path().join("archive"));

		let staged = sink
			.commit(&Artifact {
				path: work.clone(),
				source,
				times: Some(SourceTimes {
					accessed: modified,
					modified,
					created: None,
				}),
			})
			.unwrap();

		assert_eq!(
			sink.work_root(Path::new("any")),
			dir.path().join("archive").join(".cryarchiver").join("work")
		);
		assert_eq!(staged.parent().unwrap(), sink.staged_dir());
		assert_eq!(staged.extension().unwrap(), "opus");
		assert_eq!(fs::read(&staged).unwrap(), b"encoded");
		assert_eq!(fs::metadata(&staged).unwrap().modified().unwrap(), modified);
		assert!(!work.exists());
	}

	#[test]
	fn the_archival_sink_never_stages_through_a_link() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.flac");
		fs::write(&source, b"src").unwrap();
		let outside = dir.path().join("outside");
		let destination = dir.path().join("archive");
		fs::create_dir_all(&outside).unwrap();
		fs::create_dir_all(&destination).unwrap();

		#[cfg(windows)]
		let linked = std::process::Command::new("cmd")
			.args(["/C", "mklink", "/J"])
			.arg(destination.join(".cryarchiver"))
			.arg(&outside)
			.output()
			.is_ok_and(|out| out.status.success());
		#[cfg(unix)]
		let linked = std::os::unix::fs::symlink(&outside, destination.join(".cryarchiver")).is_ok();
		if !linked {
			eprintln!("skipped: no folder link can be made here");
			return;
		}

		let staged = ArchivalSink::new(&destination).commit(&staged(dir.path(), &source));

		assert!(matches!(staged, Err(CommitError::Io(_))), "{staged:?}");
		assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
	}

	fn staged(dir: &Path, source: &Path) -> Artifact {
		let path = dir.join("out.opus");
		fs::write(&path, b"encoded").unwrap();
		Artifact {
			path,
			source: source.to_path_buf(),
			times: None,
		}
	}

	#[test]
	fn commits_next_to_the_source_or_into_the_output_dir() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.flac");
		fs::write(&source, b"src").unwrap();

		let next_to = CliSink {
			output_dir: None,
			overwrite: false,
		};
		assert_eq!(next_to.work_root(&source), dir.path().join(CLI_WORK_DIR));
		assert_eq!(
			next_to.commit(&staged(dir.path(), &source)).unwrap(),
			dir.path().join("a.cry.opus")
		);

		let elsewhere = CliSink {
			output_dir: Some(dir.path().join("out")),
			overwrite: false,
		};
		assert_eq!(elsewhere.work_root(&source), dir.path().join("out").join(CLI_WORK_DIR));
		assert_eq!(
			elsewhere.commit(&staged(dir.path(), &source)).unwrap(),
			dir.path().join("out").join("a.cry.opus")
		);
	}

	#[test]
	fn existing_outputs_of_any_kind_are_found_unless_overwriting() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.flac");
		fs::write(&source, b"src").unwrap();
		fs::write(dir.path().join("a.cry.mka"), b"old").unwrap();

		assert_eq!(
			CliSink {
				output_dir: None,
				overwrite: false
			}
			.existing_output(&source),
			Some(dir.path().join("a.cry.mka"))
		);
		assert_eq!(
			CliSink {
				output_dir: None,
				overwrite: true
			}
			.existing_output(&source),
			None
		);
	}

	#[test]
	fn overwrite_replaces_old_outputs_but_never_the_source() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.flac");
		let output = dir.path().join("a.cry.opus");
		fs::write(&source, b"src").unwrap();
		fs::write(&output, b"old").unwrap();

		let careful = CliSink {
			output_dir: None,
			overwrite: false,
		};
		let replacing = CliSink {
			output_dir: None,
			overwrite: true,
		};

		assert!(matches!(
			careful.commit(&staged(dir.path(), &source)),
			Err(CommitError::Exists(_))
		));
		replacing.commit(&staged(dir.path(), &source)).unwrap();
		assert_eq!(fs::read(&output).unwrap(), b"encoded");

		fs::remove_file(&output).unwrap();
		fs::hard_link(&source, &output).unwrap();
		assert!(matches!(
			replacing.commit(&staged(dir.path(), &source)),
			Err(CommitError::IsSource(_))
		));
		assert_eq!(fs::read(&source).unwrap(), b"src");
	}
}
