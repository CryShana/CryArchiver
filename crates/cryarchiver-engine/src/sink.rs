use std::fs;
use std::path::{Path, PathBuf};

use crate::commit::{Artifact, CommitError, OutputSink, Replace, commit_file};
use crate::container::OUTPUT_EXTENSIONS;
use crate::naming::{cli_output_dir, cli_output_path};
use crate::workspace::CLI_WORK_DIR;

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

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::Path;

	use crate::commit::Artifact;
	use crate::commit::CommitError;
	use crate::workspace::CLI_WORK_DIR;

	use super::*;

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
