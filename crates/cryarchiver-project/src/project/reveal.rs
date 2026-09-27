//! Front-end conveniences that need real paths: opening a folder in the system's file manager and showing a path.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cryarchiver_engine::process::limits;
use cryarchiver_engine::text::display_path;

use super::Target;
use super::owner::{Message, Owner};
use crate::view::{LogKind, NoticeLevel, Update};

impl Owner {
	/// Looking at the folder and starting the file manager run off the project task: a slow network path must not
	/// stall it.
	pub(super) fn reveal(&mut self, target: Target, destination: bool) {
		let Some(folder) = self.target_folder(target, destination) else {
			return;
		};

		let messages = self.messages.clone();
		tokio::task::spawn_blocking(move || {
			let result = open_in_file_manager(&folder).map_err(|err| err.to_string());
			let _ = messages.send(Message::Revealed(folder, result));
		});
	}

	pub(super) fn on_revealed(&mut self, folder: &Path, result: Result<(), String>) {
		match result {
			Ok(()) => self.log(LogKind::Action, format!("opened {}", display_path(folder))),
			Err(err) => self.notice(
				NoticeLevel::Warning,
				format!("{} can't be opened: {err}", display_path(folder)),
			),
		}
	}

	pub(super) fn show_path(&mut self, target: Target) {
		let path = match target {
			Target::File(file) if self.tree.has_file(file) => self.source_path(&self.tree.file(file).key),
			Target::Folder(folder) if self.tree.has_folder(folder) => self.source_path(&self.tree.folder(folder).key),
			Target::File(_) | Target::Folder(_) => return,
		};

		self.send(Update::Path(display_path(&path)));
	}

	fn target_folder(&self, target: Target, destination: bool) -> Option<PathBuf> {
		let folder = match target {
			Target::Folder(folder) if self.tree.has_folder(folder) => folder,
			Target::File(file) if self.tree.has_file(file) => self.tree.file(file).folder,
			Target::Folder(_) | Target::File(_) => return None,
		};

		Some(if destination {
			self.destination_path(&self.destination_folder(folder))
		} else {
			self.source_path(&self.tree.folder(folder).key)
		})
	}
}

/// Hands the folder straight to the file manager as one argument, never through a shell, and only once it is a
/// folder: the file manager is never asked to open a file.
fn open_in_file_manager(folder: &Path) -> io::Result<()> {
	if !folder.is_dir() {
		return Err(io::Error::new(io::ErrorKind::NotFound, "the folder doesn't exist yet"));
	}

	let (program, mut command) = file_manager(folder);
	command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
	let mut child = limits::spawn_outside_limits(&mut command).map_err(|err| {
		if err.kind() == io::ErrorKind::NotFound {
			io::Error::new(io::ErrorKind::NotFound, format!("{program} isn't installed"))
		} else {
			err
		}
	})?;

	std::thread::spawn(move || {
		let _ = child.wait();
	});
	Ok(())
}

#[cfg(windows)]
fn file_manager(folder: &Path) -> (&'static str, Command) {
	use std::os::windows::process::CommandExt;

	let mut command = Command::new("explorer");
	command.raw_arg(explorer_argument(folder));
	("explorer", command)
}

/// Explorer reads switches from an unquoted argument and splits it at commas, so the folder is always quoted. A
/// Windows name can't hold `"`; a trailing `\` is doubled so it doesn't escape the closing quote. Explorer doesn't
/// understand the `\\?\` form that canonical roots keep for UNC and long paths, so it gets the plain form.
#[cfg(windows)]
fn explorer_argument(folder: &Path) -> std::ffi::OsString {
	let raw = folder.as_os_str().to_string_lossy();
	let plain = match raw.strip_prefix(r"\\?\UNC\") {
		Some(share) => format!(r"\\{share}"),
		None => raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_owned(),
	};
	let folder = Path::new(&plain);

	let mut argument = std::ffi::OsString::from("\"");
	argument.push(folder.as_os_str());
	if folder.as_os_str().as_encoded_bytes().ends_with(b"\\") {
		argument.push("\\");
	}
	argument.push("\"");
	argument
}

/// `-R` shows the folder in Finder; a plain `open` would launch an application bundle, which is a folder too.
#[cfg(target_os = "macos")]
fn file_manager(folder: &Path) -> (&'static str, Command) {
	let mut command = Command::new("open");
	command.arg("-R").arg(folder);
	("open", command)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn file_manager(folder: &Path) -> (&'static str, Command) {
	let mut command = Command::new("xdg-open");
	command.arg(folder);
	("xdg-open", command)
}

#[cfg(test)]
mod tests {
	use std::sync::mpsc::Receiver;
	use std::time::Duration;

	use super::*;
	use crate::project::scanning::tests::owner;
	use crate::record::FolderRecord;
	use crate::tree::FolderId;

	fn notices(updates: &Receiver<Update>) -> Vec<String> {
		updates
			.try_iter()
			.filter_map(|update| match update {
				Update::Notice(notice) => Some(notice.text),
				_ => None,
			})
			.collect()
	}

	#[test]
	fn revealing_a_folder_answers_later_from_off_the_project_task() {
		let runtime = tokio::runtime::Runtime::new().unwrap();
		let _entered = runtime.enter();
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		std::fs::create_dir_all(&source).unwrap();
		let (mut owner, updates) = owner(&source, &dir.path().join("archive"));
		let mut inbox = owner.inbox.take().unwrap();
		let photos = owner
			.tree
			.add_folder(FolderId::ROOT, std::ffi::OsStr::new("Photos"), FolderRecord::default());

		owner.reveal(Target::Folder(photos), true);
		assert!(notices(&updates).is_empty(), "nothing is looked at on the project task");

		let message = runtime
			.block_on(async { tokio::time::timeout(Duration::from_secs(20), inbox.recv()).await })
			.unwrap()
			.unwrap();
		let Message::Revealed(folder, result) = message else {
			panic!("not the reveal's answer");
		};
		assert!(result.is_err(), "the folder isn't in the archive yet");
		owner.on_revealed(&folder, result);
		assert_eq!(notices(&updates).len(), 1);
	}

	#[test]
	fn a_missing_folder_is_never_handed_to_the_file_manager() {
		let dir = tempfile::tempdir().unwrap();
		let file = dir.path().join("a.txt");
		std::fs::write(&file, b"a").unwrap();

		for path in [dir.path().join("missing"), file] {
			let err = open_in_file_manager(&path).unwrap_err();
			assert_eq!(err.kind(), io::ErrorKind::NotFound);
		}
	}

	#[cfg(windows)]
	#[test]
	fn explorer_gets_the_folder_as_one_quoted_argument() {
		assert_eq!(explorer_argument(Path::new(r"C:\a,b\c d")), r#""C:\a,b\c d""#);
		assert_eq!(explorer_argument(Path::new(r"C:\")), r#""C:\\""#);
		assert_eq!(
			explorer_argument(Path::new(r"\\?\UNC\nas\share\Photos")),
			r#""\\nas\share\Photos""#
		);
		assert_eq!(explorer_argument(Path::new(r"\\?\C:\a")), r#""C:\a""#);
	}
}
