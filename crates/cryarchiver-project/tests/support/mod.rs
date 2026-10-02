#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use cryarchiver_engine::commit::{Artifact, OutputSink, SourceTimes};
use cryarchiver_engine::decision::Decision;
use cryarchiver_engine::engine::{Encode, Job, JobEvent, MemoryGate, Stage};
use cryarchiver_engine::naming::is_media_name;
use cryarchiver_engine::outcome::{ErrorKind, IoSide, JobError, JobOutcome, KeepReason};
use cryarchiver_engine::quality::memory::QualityMemory;
use cryarchiver_engine::workspace::Workspace;
use cryarchiver_project::issues::IssueId;
use cryarchiver_project::settings::Settings;
use cryarchiver_project::tree::{FileId, FolderId, FolderStats};
use cryarchiver_project::view::{
	Details, FileRow, FilterView, IssueView, LogEntry, Notice, Overall, RunState, Status, SystemView, Update, Worker,
};
use cryarchiver_project::{Backend, Fault, Project, ProjectOptions, SystemSample, Tuning};
use tokio_util::sync::CancellationToken;

pub const WAIT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
	/// Stages an output with this extension through the sink, as the engine does.
	Encode(&'static str),
	Keep,
	NotMedia,
	Fail(ErrorKind),
	/// Runs until it is cancelled.
	Hold,
	/// Takes this long without looking at a cancel, then comes back as not media, so the job copies the source.
	Slow(Duration),
	/// Puts a new file with `content` in `path`'s place, by rename and with its modification time, then encodes. Only
	/// once: later calls encode.
	Replace {
		path: PathBuf,
		content: Vec<u8>,
	},
	/// Writes `content` into the file at `path` in place, as a tagger does, gives it `modified`, then encodes. Only
	/// once: later calls encode.
	Rewrite {
		path: PathBuf,
		content: Vec<u8>,
		modified: SystemTime,
	},
}

#[derive(Default)]
pub struct FakeEncoder {
	rules: Mutex<Vec<(String, Rule)>>,
	decisions: Mutex<Vec<(String, Decision)>>,
	pub calls: Mutex<Vec<String>>,
	running: AtomicUsize,
	pub peak: AtomicUsize,
	pub quality: Mutex<Option<Arc<dyn QualityMemory>>>,
}

impl FakeEncoder {
	pub fn new() -> Arc<Self> {
		Arc::new(Self::default())
	}

	/// Files whose name ends with `suffix` follow `rule`; the latest matching rule wins. Like the engine, media names
	/// are encoded (to AVIF) and anything else comes back as not media.
	pub fn rule(&self, suffix: &str, rule: Rule) {
		self.rules.lock().unwrap().push((suffix.to_owned(), rule));
	}

	pub fn calls(&self) -> Vec<String> {
		self.calls.lock().unwrap().clone()
	}

	/// Files whose name ends with `suffix` report `decision`, as an event and on an encoded outcome, as the engine
	/// does; the latest matching one wins.
	pub fn decide(&self, suffix: &str, decision: Decision) {
		self.decisions.lock().unwrap().push((suffix.to_owned(), decision));
	}

	fn decision_for(&self, name: &str) -> Option<Decision> {
		self.decisions
			.lock()
			.unwrap()
			.iter()
			.rev()
			.find(|(suffix, _)| name.ends_with(suffix.as_str()))
			.map(|(_, decision)| decision.clone())
	}

	fn rule_for(&self, name: &str) -> Rule {
		self.rules
			.lock()
			.unwrap()
			.iter()
			.rev()
			.find(|(suffix, _)| name.ends_with(suffix.as_str()))
			.map_or_else(
				|| {
					if is_media_name(OsStr::new(name)) {
						Rule::Encode("avif")
					} else {
						Rule::NotMedia
					}
				},
				|(_, rule)| rule.clone(),
			)
	}
}

impl Encode for FakeEncoder {
	fn encode<'a>(
		&'a self,
		job: &'a Job,
		sink: &'a dyn OutputSink,
		on_event: &'a (dyn Fn(JobEvent) + Sync),
		cancel: &'a CancellationToken,
	) -> Pin<Box<dyn Future<Output = JobOutcome> + Send + 'a>> {
		Box::pin(async move {
			let name = job.source.file_name().unwrap().to_string_lossy().into_owned();
			self.calls.lock().unwrap().push(name.clone());
			let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
			self.peak.fetch_max(running, Ordering::SeqCst);

			let decision = self.decision_for(&name);
			if let Some(decision) = &decision {
				on_event(JobEvent::Decision(Box::new(decision.clone())));
			}

			let outcome = match self.rule_for(&name) {
				Rule::Encode(extension) => {
					on_event(JobEvent::Stage(Stage::Encode));
					on_event(JobEvent::Progress(0.5));
					stage(job, sink, extension, &name, decision.unwrap_or_default())
				}
				Rule::Keep => JobOutcome::KeptOriginal {
					reason: KeepReason::NotSmaller {
						src_bytes: 10,
						out_bytes: 10,
					},
				},
				Rule::NotMedia => JobOutcome::Skipped {
					reason: "not media".into(),
				},
				Rule::Fail(kind) => JobOutcome::Failed(JobError::with_log(kind, vec!["fake tool output".into()])),
				Rule::Hold => {
					on_event(JobEvent::Stage(Stage::Encode));
					cancel.cancelled().await;
					JobOutcome::Failed(ErrorKind::Cancelled.into())
				}
				Rule::Slow(duration) => {
					on_event(JobEvent::Stage(Stage::Encode));
					tokio::time::sleep(duration).await;
					JobOutcome::Skipped {
						reason: "not media".into(),
					}
				}
				Rule::Replace { path, content } => {
					replace_keeping_the_time(&path, &content, &sink.work_root(&job.source));
					self.rule(&name, Rule::Encode("avif"));
					on_event(JobEvent::Stage(Stage::Encode));
					stage(job, sink, "avif", &name, Decision::default())
				}
				Rule::Rewrite {
					path,
					content,
					modified,
				} => {
					let mut file = fs::File::options().write(true).truncate(true).open(&path).unwrap();
					file.write_all(&content).unwrap();
					file.set_modified(modified).unwrap();
					drop(file);
					self.rule(&name, Rule::Encode("avif"));
					on_event(JobEvent::Stage(Stage::Encode));
					stage(job, sink, "avif", &name, Decision::default())
				}
			};

			self.running.fetch_sub(1, Ordering::SeqCst);
			outcome
		})
	}
}

fn replace_keeping_the_time(path: &Path, content: &[u8], scratch: &Path) {
	let modified = fs::metadata(path).unwrap().modified().unwrap();
	fs::create_dir_all(scratch).unwrap();
	let replacement = scratch.join(format!("replacement-{}", ulid::Ulid::generate()));
	fs::write(&replacement, content).unwrap();
	fs::File::options()
		.write(true)
		.open(&replacement)
		.unwrap()
		.set_modified(modified)
		.unwrap();
	fs::rename(&replacement, path).unwrap();
}

/// Works in a job folder of the engine's own workspace, as the engine does.
fn stage(job: &Job, sink: &dyn OutputSink, extension: &str, name: &str, decision: Decision) -> JobOutcome {
	let workspace = match Workspace::for_sink(sink, &job.source) {
		Ok(workspace) => workspace,
		Err(err) => return JobOutcome::Failed(JobError::io(IoSide::Destination(&sink.work_root(&job.source)), &err)),
	};
	let work = workspace.file(&format!("fake.{extension}"));
	fs::write(&work, format!("encoded {name}")).unwrap();

	let times = SourceTimes::from_metadata(&fs::metadata(&job.source).unwrap()).unwrap();
	match sink.commit(&Artifact {
		path: work,
		source: job.source.clone(),
		times: Some(times),
	}) {
		Ok(staged) => JobOutcome::Encoded {
			out_bytes: fs::metadata(&staged).unwrap().len(),
			output: staged,
			src_bytes: fs::metadata(&job.source).unwrap().len(),
			warnings: Vec::new(),
			decision: Box::new(decision),
		},
		Err(err) => JobOutcome::Failed(ErrorKind::DestinationIo(err.to_string()).into()),
	}
}

pub struct FakeBackend {
	pub encoder: Arc<FakeEncoder>,
}

impl Backend for FakeBackend {
	fn encoder(
		&self,
		_image_threads: usize,
		_memory: Arc<dyn MemoryGate>,
		quality: Arc<dyn QualityMemory>,
	) -> Arc<dyn Encode> {
		*self.encoder.quality.lock().unwrap() = Some(quality);
		self.encoder.clone()
	}

	fn toolchain(&self) -> String {
		"fake encoder".into()
	}

	fn sample(&self) -> SystemSample {
		SystemSample {
			cpu_busy: 0.1,
			memory_available: 16 << 30,
			memory_total: 32 << 30,
		}
	}
}

pub fn tuning() -> Tuning {
	Tuning {
		tick: Duration::from_millis(10),
		view_interval: Duration::from_millis(10),
		ramp: Duration::ZERO,
		debounce: Duration::from_millis(100),
		safety_pass: Duration::from_hours(1),
		retry_base: Duration::from_millis(20),
		disk_poll: Duration::from_millis(100),
		listings: 4,
		max_encodes: 4,
		watcher: true,
	}
}

pub struct Setup {
	pub dir: tempfile::TempDir,
	pub source: PathBuf,
	pub destination: PathBuf,
	pub project: PathBuf,
}

/// A source folder, an empty destination beside it, and a project with a zero stability window.
pub fn setup(configure: impl FnOnce(&mut Settings)) -> Setup {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	let destination = dir.path().join("archive");
	fs::create_dir_all(&source).unwrap();

	let project = cryarchiver_project::create(&source, &destination, OsStr::new("archive"), |settings| {
		settings.stability_window = Duration::ZERO;
		configure(settings);
	})
	.unwrap();

	Setup {
		source: fs::canonicalize(&source).unwrap(),
		destination: fs::canonicalize(&destination).unwrap(),
		project,
		dir,
	}
}

pub fn open(setup: &Setup, encoder: &Arc<FakeEncoder>) -> Project {
	open_with(setup, encoder, None)
}

pub fn open_with(setup: &Setup, encoder: &Arc<FakeEncoder>, fault: Option<Fault>) -> Project {
	open_tuned(setup, encoder, tuning(), fault)
}

/// Without watcher events: a change in the source is noticed only by what the project reads itself.
pub fn open_unwatched(setup: &Setup, encoder: &Arc<FakeEncoder>) -> Project {
	let tuning = Tuning {
		watcher: false,
		..tuning()
	};
	open_tuned(setup, encoder, tuning, None)
}

pub fn open_tuned(setup: &Setup, encoder: &Arc<FakeEncoder>, tuning: Tuning, fault: Option<Fault>) -> Project {
	Project::open(
		&setup.project,
		ProjectOptions {
			backend: Arc::new(FakeBackend {
				encoder: encoder.clone(),
			}),
			tuning,
			fault,
		},
	)
	.unwrap()
}

pub fn write(root: &Path, relative: &str, content: &[u8]) -> PathBuf {
	let path = relative
		.split('/')
		.fold(root.to_path_buf(), |path, part| path.join(part));
	fs::create_dir_all(path.parent().unwrap()).unwrap();
	fs::write(&path, content).unwrap();
	path
}

/// A second name for the folder `target`: a junction on Windows, a symbolic link elsewhere. False when one can't be
/// made here.
pub fn link_folder(link: &Path, target: &Path) -> bool {
	#[cfg(windows)]
	return std::process::Command::new("cmd")
		.args(["/C", "mklink", "/J"])
		.arg(link)
		.arg(target)
		.output()
		.is_ok_and(|out| out.status.success());

	#[cfg(unix)]
	return std::os::unix::fs::symlink(target, link).is_ok();
}

/// A symbolic link to the file `target`, which need not exist. False when one can't be made here, as on Windows
/// without Developer Mode.
pub fn link_file(link: &Path, target: &Path) -> bool {
	#[cfg(windows)]
	return std::process::Command::new("cmd")
		.args(["/C", "mklink"])
		.arg(link)
		.arg(target)
		.output()
		.is_ok_and(|out| out.status.success());

	#[cfg(unix)]
	return std::os::unix::fs::symlink(target, link).is_ok();
}

/// `path` as the project's messages name it: stored paths are plain, while [`setup`] hands out canonical ones.
pub fn shown(path: &Path) -> String {
	cryarchiver_engine::text::display_path(&cryarchiver_project::settings::plain_path(path.to_path_buf()))
}

/// The names in `folder`, sorted.
pub fn names_in(folder: &Path) -> Vec<String> {
	let mut names: Vec<String> = fs::read_dir(folder)
		.unwrap()
		.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
		.collect();
	names.sort();
	names
}

/// A folder no one may list until this is dropped: an ACL denying everyone on Windows, mode 000 on Unix.
pub struct Unlistable(PathBuf);

impl Unlistable {
	/// `None` where the denial doesn't hold, as for root, who lists any folder.
	pub fn deny(folder: &Path) -> Option<Self> {
		let denied = Self(folder.to_path_buf());
		assert!(denied.set(false), "can't deny listing {}", folder.display());
		fs::read_dir(folder).is_err().then_some(denied)
	}

	#[cfg(windows)]
	fn set(&self, listable: bool) -> bool {
		let change = if listable {
			["/remove:d", "*S-1-1-0"]
		} else {
			["/deny", "*S-1-1-0:(RD)"]
		};
		std::process::Command::new("icacls")
			.arg(&self.0)
			.args(change)
			.output()
			.is_ok_and(|out| out.status.success())
	}

	#[cfg(unix)]
	fn set(&self, listable: bool) -> bool {
		use std::os::unix::fs::PermissionsExt;

		let mode = if listable { 0o755 } else { 0o000 };
		fs::set_permissions(&self.0, fs::Permissions::from_mode(mode)).is_ok()
	}
}

impl Drop for Unlistable {
	fn drop(&mut self) {
		self.set(true);
	}
}

/// Bytes that start like a JPEG, so the project treats the file as media.
pub fn jpeg(seed: u8, len: usize) -> Vec<u8> {
	let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00];
	bytes.extend(
		(0..len)
			.map(|index| u8::try_from(index % 256).unwrap())
			.map(|byte| byte.wrapping_mul(31).wrapping_add(seed)),
	);
	bytes
}

#[derive(Default)]
pub struct Snapshot {
	pub status: Option<Status>,
	pub settings: Option<Settings>,
	pub overall: Option<Overall>,
	pub folders: Vec<(FolderId, FolderId, String)>,
	pub stats: HashMap<FolderId, FolderStats>,
	pub rows: HashMap<FolderId, Vec<FileRow>>,
	pub workers: Vec<Worker>,
	pub issues: Vec<IssueView>,
	pub filter: Option<FilterView>,
	pub details: HashMap<FileId, Details>,
	pub issue_logs: HashMap<(IssueId, u32), Vec<String>>,
	pub logs: Vec<LogEntry>,
	pub notices: Vec<Notice>,
	pub paths: Vec<String>,
	pub system: Option<SystemView>,
	pub stopped: bool,
}

impl Snapshot {
	pub fn apply(&mut self, update: Update) {
		match update {
			Update::Status(status) => self.status = Some(status),
			Update::Settings(settings) => self.settings = Some(settings),
			Update::FolderAdded { id, parent, name } => self.folders.push((id, parent, name)),
			Update::Folder(id, stats) => {
				self.stats.insert(id, stats);
			}
			Update::Files { folder, rows } => {
				self.rows.insert(folder, rows);
			}
			Update::FileAdded { folder, row } => self.rows.entry(folder).or_default().push(row),
			Update::FileRemoved { folder, file } => {
				if let Some(rows) = self.rows.get_mut(&folder) {
					rows.retain(|row| row.id != file);
				}
			}
			Update::FileStatus { folder, file, status } => {
				for row in self.rows.get_mut(&folder).into_iter().flatten() {
					if row.id == file {
						row.status = status.clone();
					}
				}
			}
			Update::Overall(overall) => self.overall = Some(overall),
			Update::Workers(workers) => self.workers = workers,
			Update::Issues(issues) => self.issues = issues,
			Update::Filter(filter) => self.filter = filter,
			Update::Details(file, details) => {
				self.details.insert(file, *details);
			}
			Update::IssueLog { issue, attempt, lines } => {
				self.issue_logs.insert((issue, attempt), lines);
			}
			Update::Log(entry) => self.logs.push(entry),
			Update::Notice(notice) => self.notices.push(notice),
			Update::Path(path) => self.paths.push(path),
			Update::System(system) => self.system = Some(system),
			Update::Stopped => self.stopped = true,
		}
	}

	pub fn folder_named(&self, name: &str) -> FolderId {
		self.folders
			.iter()
			.find(|(_, _, folder)| folder == name)
			.map_or_else(|| panic!("no folder {name}: {:?}", self.folders), |(id, _, _)| *id)
	}

	pub fn row(&self, folder: FolderId, name: &str) -> &FileRow {
		self.rows
			.get(&folder)
			.and_then(|rows| rows.iter().find(|row| row.name == name))
			.unwrap_or_else(|| panic!("no row {name} in {folder:?}: {:?}", self.rows.get(&folder)))
	}

	pub fn scan_complete(&self) -> bool {
		self.status.as_ref().is_some_and(|status| status.scan_complete)
	}

	pub fn run_state(&self) -> Option<RunState> {
		self.status.as_ref().map(|status| status.run_state)
	}

	pub fn finished(&self) -> bool {
		self.run_state() == Some(RunState::Finished)
	}

	pub fn root(&self) -> FolderStats {
		self.stats.get(&FolderId::ROOT).copied().unwrap_or_default()
	}
}

/// Folds updates into the snapshot until `done` holds, or panics after [`WAIT`].
pub fn wait_for(project: &Project, snapshot: &mut Snapshot, what: &str, done: impl Fn(&Snapshot) -> bool) {
	let deadline = Instant::now() + WAIT;
	while !done(snapshot) {
		assert!(Instant::now() < deadline, "timed out waiting for {what}");
		if let Some(update) = project.wait_update(Duration::from_millis(20)) {
			snapshot.apply(update);
		}
	}
}

/// Folds everything already sent, without waiting.
pub fn drain(project: &Project, snapshot: &mut Snapshot) {
	while let Some(update) = project.try_update() {
		snapshot.apply(update);
	}
}
