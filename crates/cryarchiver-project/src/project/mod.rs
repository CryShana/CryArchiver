//! The project task and its handle (project layer spec §11). One task owns the in-memory state and the project file;
//! every change goes through it. Front ends send [`Command`]s and fold the [`Update`]s it sends back.

mod dangerous;
mod dependencies;
mod duplicates;
mod finish;
mod jobs;
mod live;
mod open;
mod owner;
mod pipeline;
mod publish;
mod resolve;
mod reveal;
mod scanning;
mod watching;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use cryarchiver_engine::commit::file_id;
use cryarchiver_engine::engine::{Encode, MemoryGate};
use cryarchiver_engine::quality::memory::QualityMemory;
use cryarchiver_engine::text::display_path;
use cryarchiver_engine::workspace::is_link;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::issues::{IssueId, Verb};
use crate::settings::{
	RootConflict, Settings, SettingsChange, SettingsError, check_roots, identity_conflict, plain_path, planned_path,
};
use crate::store::{Store, StoreError};
use crate::tree::{FileId, FolderId};
use crate::view::Update;
use owner::Owner;

pub const PROJECT_EXTENSION: &str = "cryarchive";

/// The timings of the project task. The defaults are the spec's; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
	pub tick: Duration,
	pub view_interval: Duration,
	pub ramp: Duration,
	pub debounce: Duration,
	pub safety_pass: Duration,
	pub retry_base: Duration,
	pub disk_poll: Duration,
	pub listings: usize,
	pub max_encodes: usize,
	/// Watcher events; the safety pass runs either way.
	pub watcher: bool,
}

impl Default for Tuning {
	fn default() -> Self {
		Self {
			tick: Duration::from_millis(250),
			view_interval: Duration::from_millis(100),
			ramp: Duration::from_secs(3),
			debounce: Duration::from_secs(2),
			safety_pass: Duration::from_mins(15),
			retry_base: Duration::from_secs(10),
			disk_poll: Duration::from_mins(1),
			listings: 4,
			max_encodes: std::thread::available_parallelism().map_or(4, usize::from),
			watcher: true,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SystemSample {
	/// The share of the whole machine's CPU in use, from 0 to 1.
	pub cpu_busy: f32,
	pub memory_available: u64,
	pub memory_total: u64,
}

/// What the project needs from the outside world: the encoder, and the machine's load.
pub trait Backend: Send + Sync {
	/// Builds the encoder for the jobs that start from now on.
	fn encoder(
		&self,
		image_threads: usize,
		memory: Arc<dyn MemoryGate>,
		quality: Arc<dyn QualityMemory>,
	) -> Arc<dyn Encode>;

	fn toolchain(&self) -> String;

	fn sample(&self) -> SystemSample;
}

pub struct ProjectOptions {
	pub backend: Arc<dyn Backend>,
	pub tuning: Tuning,
	/// Only for crash-recovery tests.
	pub fault: Option<Fault>,
}

/// A point where the project task stops dead, as if the process crashed: nothing more is written, not even the
/// batch in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
	/// `Committing` is durable; the job never renames its output.
	AfterCommitIntent,
	/// The output is in place, but its `Done` is still in the unwritten batch.
	AfterPlaced,
	/// A placeholder adopted its canonical copy's earlier output, still in the unwritten batch.
	AfterAdoption,
	/// `DeletePending` is durable; the original is never deleted.
	AfterDeleteIntent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
	/// `delete_confirmed` is the front end's confirmation for a project that deletes originals (spec §9). Without it
	/// such a project archives but deletes nothing, until a later start or `ChangeSettings` with the typed `DELETE`
	/// confirms it for the session.
	Start {
		delete_confirmed: bool,
	},
	Pause,
	StopNow,
	Rescan,
	RestartPendingJobs,
	ChangeSettings(SettingsChange),
	Resolve(IssueId, Verb),
	RetryFailedIn(FolderId),
	/// A folder was opened or closed in the front end: its rows are sent, then kept up to date, or released.
	Watch(FolderId, bool),
	/// An empty query clears the filter.
	Filter(String),
	Details(FileId),
	IssueLog(IssueId, u32),
	/// Opens the folder, or the file's folder, in the system's file manager: the source side, or the archive's.
	Reveal {
		target: Target,
		destination: bool,
	},
	/// Answers with [`Update::Path`](crate::view::Update::Path), the full source path.
	ShowPath(Target),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
	Folder(FolderId),
	File(FileId),
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
	#[error(transparent)]
	Store(#[from] StoreError),
	#[error(transparent)]
	Settings(#[from] SettingsError),
	#[error(transparent)]
	Io(#[from] io::Error),
	#[error(
		"this project file was moved or copied away from its archive, which is {destination}; open the project file there"
	)]
	Moved { destination: String },
	#[error("the project name {name:?} can't be used: {reason}")]
	Name { name: String, reason: String },
	#[error(
		"{path} is a link or a junction, which could lead the project's writes and deletes elsewhere; the project doesn't open while it is one"
	)]
	Link { path: String },
}

pub const MAX_NAME_CHARS: usize = 100;
const INVALID_NAME_CHARS: &str = "<>:\"/\\|?*";

/// The rules for a project name, the same for every front end: one plain file name that every supported system can
/// hold, so `<destination>/<name>.cryarchive` always stays in the destination. The reason reads after the name.
pub fn check_name(name: &OsStr) -> Result<(), String> {
	let text = name.to_string_lossy();
	if text.is_empty() {
		return Err("enter a name for the project".into());
	}

	if text.chars().count() > MAX_NAME_CHARS {
		return Err(format!("at most {MAX_NAME_CHARS} characters"));
	}

	if text.chars().any(char::is_control) {
		return Err("can't contain control characters".into());
	}

	if let Some(bad) = text.chars().find(|c| INVALID_NAME_CHARS.contains(*c)) {
		return Err(format!("can't contain {bad}"));
	}

	if text.ends_with(['.', ' ']) {
		return Err("can't end with a dot or a space".into());
	}

	let stem = text.split('.').next().unwrap_or(&text).trim_end();
	if cryarchiver_engine::naming::is_reserved_device_name(stem) {
		return Err(format!("{stem} is reserved by Windows for a device"));
	}

	if one_plain_component(name) {
		Ok(())
	} else {
		Err("must be a plain file name".into())
	}
}

/// Checked on the real `OsStr`, not the lossy text the other rules read.
fn one_plain_component(name: &OsStr) -> bool {
	let mut components = Path::new(name).components();
	matches!((components.next(), components.next()), (Some(Component::Normal(only)), None) if only == name)
}

fn checked_name(name: &OsStr) -> Result<(), ProjectError> {
	check_name(name).map_err(|reason| ProjectError::Name {
		name: name.to_string_lossy().into_owned(),
		reason,
	})
}

/// Creates `<destination>/<name>.cryarchive` for a new project. The destination is created if needed. Nothing is
/// created for settings that are refused.
pub fn create(
	source: &Path,
	destination: &Path,
	name: &OsStr,
	configure: impl FnOnce(&mut Settings),
) -> Result<PathBuf, ProjectError> {
	let mut settings = Settings::new(source, destination);
	configure(&mut settings);
	settings.overrides.validate().map_err(SettingsError::Overrides)?;

	checked_name(name)?;
	let (canonical_source, planned) = approved_destination(source, destination)?;
	create_folders(&planned, |folder| outside_source(&canonical_source, folder))?;
	let (source, destination) = check_roots(source, &planned)?;

	let path = project_file(&destination, name)?;
	refuse_outside(&path, &destination, name)?;
	settings.source = source;
	settings.destination = destination;
	settings.overrides = settings.overrides.normalized();
	Store::create(&path, &settings)?;
	Ok(path)
}

/// The project file must sit directly in the checked destination, by path and by folder identity.
fn refuse_outside(path: &Path, destination: &Path, name: &OsStr) -> Result<(), ProjectError> {
	let parent = path.parent().unwrap_or(path);
	let same_identity = match (file_id(parent), file_id(destination)) {
		(Ok(parent), Ok(destination)) => parent == destination,
		_ => false,
	};

	if parent == destination && same_identity {
		return Ok(());
	}
	Err(ProjectError::Name {
		name: name.to_string_lossy().into_owned(),
		reason: "the project file would not be in the destination".into(),
	})
}

/// Where [`create`] writes the project file for `destination`, whether the destination exists yet or not.
pub fn project_file(destination: &Path, name: &OsStr) -> Result<PathBuf, ProjectError> {
	checked_name(name)?;

	let mut file_name = name.to_os_string();
	file_name.push(".");
	file_name.push(PROJECT_EXTENSION);
	Ok(plain_path(planned_path(destination)?).join(file_name))
}

/// Runs before anything is created, and returns the canonical source and the only path that may be created: the
/// destination's planned path, with no `.` or `..` left. The source must already be a folder: creating a destination
/// against a missing source could create the source's own folders.
fn approved_destination(source: &Path, destination: &Path) -> Result<(PathBuf, PathBuf), ProjectError> {
	let meta = fs::metadata(source).map_err(|err| naming(source, &err))?;
	if !meta.is_dir() {
		return Err(SettingsError::Roots(format!("{}: the source is not a folder", display_path(source))).into());
	}

	let source = fs::canonicalize(source).map_err(|err| naming(source, &err))?;
	let planned = planned_path(destination)?;
	let conflict = RootConflict::between(&source, &planned).or_else(|| identity_conflict(&source, &planned));
	match conflict {
		Some(conflict) => Err(SettingsError::Roots(conflict.message().into()).into()),
		None => Ok((source, planned)),
	}
}

/// Creates the missing folders of `planned` one name at a time. The folder each new one goes into is checked first,
/// and the last one after it is made; when a check fails nothing more is created, and the folders made so far are
/// removed again, each only while it is empty.
fn create_folders(planned: &Path, check: impl Fn(&Path) -> Result<(), ProjectError>) -> Result<(), ProjectError> {
	let Some(existing) = planned.ancestors().find(|dir| dir.exists()) else {
		let missing = io::Error::new(io::ErrorKind::NotFound, "no part of the path exists");
		return Err(naming(planned, &missing).into());
	};
	let names: Vec<Component> = planned
		.strip_prefix(existing)
		.map_or_else(|_| Vec::new(), |tail| tail.components().collect());

	let mut created: Vec<PathBuf> = Vec::new();
	let mut folder = existing.to_path_buf();
	let undo = |created: &[PathBuf]| {
		for made in created.iter().rev() {
			let _ = fs::remove_dir(made);
		}
	};

	for name in names {
		if let Err(err) = check(&folder) {
			undo(&created);
			return Err(err);
		}

		folder.push(name);
		match fs::create_dir(&folder) {
			Ok(()) => created.push(folder.clone()),
			Err(err) if err.kind() == io::ErrorKind::AlreadyExists && folder.is_dir() => {}
			Err(err) => {
				undo(&created);
				return Err(naming(&folder, &err).into());
			}
		}
	}

	check(&folder).inspect_err(|_| undo(&created))
}

/// A folder about to receive a new one must be neither the source nor inside it, by its canonical path and by folder
/// identity.
fn outside_source(source: &Path, folder: &Path) -> Result<(), ProjectError> {
	let canonical = fs::canonicalize(folder).map_err(|err| naming(folder, &err))?;
	let conflict = RootConflict::between(source, &canonical).or_else(|| identity_conflict(source, &canonical));

	match conflict {
		Some(conflict @ (RootConflict::Same | RootConflict::DestinationInsideSource)) => {
			Err(SettingsError::Roots(conflict.message().into()).into())
		}
		_ => Ok(()),
	}
}

fn naming(path: &Path, err: &io::Error) -> io::Error {
	io::Error::new(err.kind(), format!("{}: {err}", display_path(path)))
}

/// What a list of projects shows about one, read without opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
	pub source: PathBuf,
	pub destination: PathBuf,
	pub delete_originals: bool,
	/// The project wasn't closed cleanly; opening it recovers it.
	pub needs_recovery: bool,
}

/// Reads a closed project's settings without writing to it, through [`crate::store::peek`]: a project open elsewhere
/// is [`StoreError::AlreadyOpen`], and an open that starts while the peek reads can fail the same way.
pub fn peek(path: &Path) -> Result<Summary, ProjectError> {
	let peeked = crate::store::peek(path)?;
	let settings = peeked.meta.settings;
	Ok(Summary {
		source: settings.source,
		destination: settings.destination,
		delete_originals: settings.delete_originals,
		needs_recovery: peeked.needs_recovery,
	})
}

/// An open project. Dropping it stops the run safely, like [`Project::shutdown`].
pub struct Project {
	path: PathBuf,
	settings: Settings,
	commands: UnboundedSender<owner::Input>,
	updates: Receiver<Update>,
	owner: Option<JoinHandle<()>>,
	runtime: Option<Runtime>,
}

impl Project {
	/// Opens the project file, recovers from an earlier crash and starts the project task. Work starts only with
	/// [`Command::Start`]; the source is scanned and reconciled right away.
	pub fn open(path: &Path, options: ProjectOptions) -> Result<Self, ProjectError> {
		if fs::symlink_metadata(path).is_ok_and(|meta| is_link(&meta)) {
			return Err(ProjectError::Link {
				path: display_path(path),
			});
		}

		open::refuse_to_open(path, &crate::store::read_closed(path)?.settings)?;

		let (store, meta, loaded) = Store::open(path)?;
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.thread_name("cryarchiver-project")
			.build()?;

		let (updates_sender, updates) = std::sync::mpsc::channel();
		let (commands, inputs) = unbounded_channel();
		let settings = meta.settings.clone();
		let owner = {
			let _entered = runtime.enter();
			Owner::open(path, store, meta.settings, loaded, options, updates_sender)?
		};
		let handle = runtime.spawn(owner.run(inputs));

		Ok(Self {
			path: path.to_path_buf(),
			settings,
			commands,
			updates,
			owner: Some(handle),
			runtime: Some(runtime),
		})
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// The settings as they were when the project opened; later values arrive as [`Update::Settings`].
	pub fn settings(&self) -> &Settings {
		&self.settings
	}

	pub fn send(&self, command: Command) {
		let _ = self.commands.send(owner::Input::Command(command));
	}

	/// Never blocks: for a UI's refresh step.
	pub fn try_update(&self) -> Option<Update> {
		self.updates.try_recv().ok()
	}

	pub fn wait_update(&self, timeout: Duration) -> Option<Update> {
		self.updates.recv_timeout(timeout).ok()
	}

	/// Stop now: cancels and requeues running jobs, writes everything down and ends the task. Blocks until it has.
	pub fn shutdown(&mut self) {
		let Some(handle) = self.owner.take() else {
			return;
		};

		let _ = self.commands.send(owner::Input::Shutdown);
		if let Some(runtime) = self.runtime.take() {
			let _ = runtime.block_on(handle);
			runtime.shutdown_background();
		}
	}
}

impl Drop for Project {
	fn drop(&mut self) {
		self.shutdown();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A second name for `target`: a junction on Windows, a symbolic link elsewhere. `None` when one can't be made.
	fn alias(target: &Path, alias: &Path) -> Option<PathBuf> {
		#[cfg(windows)]
		let made = std::process::Command::new("cmd")
			.args(["/C", "mklink", "/J"])
			.arg(alias)
			.arg(target)
			.stdout(std::process::Stdio::null())
			.status()
			.is_ok_and(|status| status.success());
		#[cfg(unix)]
		let made = std::os::unix::fs::symlink(target, alias).is_ok();

		made.then(|| alias.to_path_buf())
	}

	#[test]
	fn folders_are_created_one_at_a_time_and_stop_at_the_first_inside_the_source() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(source.join("sub")).unwrap();
		let Some(link) = alias(&source.join("sub"), &dir.path().join("link")) else {
			eprintln!("skipped: no junction or link can be made here");
			return;
		};
		let canonical = fs::canonicalize(&source).unwrap();

		let created = create_folders(&link.join("new").join("deeper"), |folder| {
			outside_source(&canonical, folder)
		});

		assert!(matches!(created, Err(ProjectError::Settings(_))), "{created:?}");
		assert_eq!(fs::read_dir(source.join("sub")).unwrap().count(), 0);
		fs::remove_dir(&link).or_else(|_| fs::remove_file(&link)).unwrap();
	}

	#[test]
	fn a_name_is_one_plain_file_name() {
		let refused = |name: &str| check_name(OsStr::new(name)).unwrap_err();

		assert!(check_name(OsStr::new("Photos 2026")).is_ok());
		assert!(check_name(OsStr::new("nul-free.v2")).is_ok());
		assert!(refused("").contains("enter"));
		assert!(refused("a/b").contains("can't contain /"));
		assert!(refused(r"a\b").contains(r"can't contain \"));
		assert!(refused("C:rel").contains("can't contain :"));
		assert!(refused("a\u{0}b").contains("control"));
		assert!(refused("trailing.").contains("end with"));
		assert!(refused("..").contains("end with"));
		assert!(refused("con.backup").contains("reserved"));
		assert!(refused(&"x".repeat(MAX_NAME_CHARS + 1)).contains("at most"));
		assert!(check_name(OsStr::new(&"é".repeat(MAX_NAME_CHARS))).is_ok());
	}

	#[test]
	fn the_last_rule_takes_only_one_plain_component() {
		assert!(one_plain_component(OsStr::new("Photos 2026")));
		for name in ["", ".", "..", "a/b", "/a", "a/", "../a"] {
			assert!(!one_plain_component(OsStr::new(name)), "{name:?}");
		}

		#[cfg(windows)]
		for name in [r"a\b", r"C:\a", "C:a", r"\\?\C:\a"] {
			assert!(!one_plain_component(OsStr::new(name)), "{name:?}");
		}
	}

	#[cfg(unix)]
	#[test]
	fn a_name_that_is_not_utf8_reaches_the_last_rule_on_its_real_bytes() {
		use std::os::unix::ffi::OsStrExt;

		assert!(check_name(OsStr::from_bytes(b"a\xff")).is_ok());
		assert!(one_plain_component(OsStr::from_bytes(b"a\xff")));
		assert!(!one_plain_component(OsStr::from_bytes(b"\xff/..")));
		assert!(check_name(OsStr::from_bytes(b"\xff/..")).is_err());
	}

	#[test]
	fn the_project_file_must_sit_directly_in_the_destination() {
		let dir = tempfile::tempdir().unwrap();
		let destination = dir.path().join("archive");
		fs::create_dir_all(dir.path().join("other")).unwrap();
		fs::create_dir(&destination).unwrap();
		let name = OsStr::new("a");

		assert!(refuse_outside(&destination.join("a.cryarchive"), &destination, name).is_ok());
		assert!(matches!(
			refuse_outside(&dir.path().join("other").join("a.cryarchive"), &destination, name),
			Err(ProjectError::Name { .. })
		));
		assert!(refuse_outside(&destination.join("x").join("a.cryarchive"), &destination, name).is_err());
	}

	#[test]
	fn a_refused_step_removes_only_the_empty_folders_this_call_made() {
		let dir = tempfile::tempdir().unwrap();
		fs::create_dir(dir.path().join("kept")).unwrap();
		fs::write(dir.path().join("kept").join("file.txt"), b"x").unwrap();
		let refuse_at = |name: &'static str| {
			move |folder: &Path| {
				if folder.ends_with(name) {
					Err(ProjectError::Settings(SettingsError::Roots("refused".into())))
				} else {
					Ok(())
				}
			}
		};

		let refused = create_folders(&dir.path().join("kept").join("a").join("b").join("c"), refuse_at("b"));

		assert!(refused.is_err());
		assert!(dir.path().join("kept").join("file.txt").is_file());
		assert_eq!(fs::read_dir(dir.path().join("kept")).unwrap().count(), 1);

		let made = create_folders(&dir.path().join("new").join("x"), refuse_at("nothing"));

		assert!(made.is_ok());
		assert!(dir.path().join("new").join("x").is_dir());
	}
}
