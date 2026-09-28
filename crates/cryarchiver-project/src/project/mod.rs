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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use cryarchiver_engine::engine::{Encode, MemoryGate};
use cryarchiver_engine::quality::memory::QualityMemory;
use cryarchiver_engine::text::display_path;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::issues::{IssueId, Verb};
use crate::settings::{Settings, SettingsChange, SettingsError, check_roots, root_conflict};
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// Creates `<destination>/<name>.cryarchive` for a new project. The destination is created if needed.
pub fn create(
	source: &Path,
	destination: &Path,
	name: &OsStr,
	configure: impl FnOnce(&mut Settings),
) -> Result<PathBuf, ProjectError> {
	refuse_overlapping_roots(source, destination)?;
	fs::create_dir_all(destination).map_err(|err| naming(destination, &err))?;
	let (source, destination) = check_roots(source, destination)?;

	let mut file_name = name.to_os_string();
	file_name.push(".");
	file_name.push(PROJECT_EXTENSION);
	let path = destination.join(file_name);

	let mut settings = Settings::new(&source, &destination);
	configure(&mut settings);
	Store::create(&path, &settings)?;
	Ok(path)
}

/// The destination may not exist yet; creating it inside the source would already write to the source, so the
/// roots are compared on their planned paths first.
fn refuse_overlapping_roots(source: &Path, destination: &Path) -> Result<(), ProjectError> {
	match root_conflict(source, destination)? {
		Some(conflict) => Err(SettingsError::Roots(conflict.message().into()).into()),
		None => Ok(()),
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
}

/// Reads a closed project's settings through a read-only handle that is dropped before returning, so it never holds
/// the lock an open takes. A project open elsewhere is [`StoreError::AlreadyOpen`].
pub fn peek(path: &Path) -> Result<Summary, ProjectError> {
	let meta = crate::store::peek(path)?;
	Ok(Summary {
		source: meta.settings.source,
		destination: meta.settings.destination,
		delete_originals: meta.settings.delete_originals,
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
