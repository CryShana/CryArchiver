use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cryarchiver_engine::commit::OutputSink;
use cryarchiver_engine::engine::{Encode, Job, JobEvent, MemoryGate};
use cryarchiver_engine::outcome::{ErrorKind, JobError, JobOutcome};
use cryarchiver_engine::text::display_path;
use cryarchiver_project::{Backend, Project, ProjectOptions, SystemSample, Tuning};
use cryarchiver_tui::source::Target;
use cryarchiver_tui::view::{FileState, NoticeLevel, RunState};
use cryarchiver_tui::{Action, App, ArchiveSource, ColorDepth, ProjectSource};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(20);

/// Copies everything, except files named `bad.jpg`, which fail as damaged.
struct CopyEverything;

impl Encode for CopyEverything {
	fn encode<'a>(
		&'a self,
		job: &'a Job,
		_sink: &'a dyn OutputSink,
		_on_event: &'a (dyn Fn(JobEvent) + Sync),
		_cancel: &'a CancellationToken,
	) -> Pin<Box<dyn Future<Output = JobOutcome> + Send + 'a>> {
		Box::pin(async move {
			if job.source.file_name() == Some(OsStr::new("bad.jpg")) {
				JobOutcome::Failed(JobError::with_log(
					ErrorKind::SourceDamaged("truncated".into()),
					vec!["[mjpeg] premature end of data".into()],
				))
			} else {
				JobOutcome::Skipped {
					reason: "not media".into(),
				}
			}
		})
	}
}

struct TestBackendOptions;

impl Backend for TestBackendOptions {
	fn encoder(&self, _image_threads: usize, _memory: Arc<dyn MemoryGate>) -> Arc<dyn Encode> {
		Arc::new(CopyEverything)
	}

	fn toolchain(&self) -> String {
		"test".into()
	}

	fn sample(&self) -> SystemSample {
		SystemSample {
			cpu_busy: 0.1,
			memory_available: 8 << 30,
			memory_total: 16 << 30,
		}
	}
}

struct Setup {
	_dir: tempfile::TempDir,
	source: PathBuf,
	project: PathBuf,
}

fn setup(files: &[(&str, &[u8])]) -> Setup {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");

	for (name, content) in files {
		let path = name.split('/').fold(source.clone(), |path, part| path.join(part));
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, content).unwrap();
	}
	fs::create_dir_all(&source).unwrap();

	let project = cryarchiver_project::create(
		&source,
		&dir.path().join("archive"),
		OsStr::new("archive"),
		|settings| {
			settings.stability_window = Duration::ZERO;
		},
	)
	.unwrap();

	Setup {
		_dir: dir,
		source,
		project,
	}
}

fn open(project: &Path) -> ProjectSource {
	let project = Project::open(
		project,
		ProjectOptions {
			backend: Arc::new(TestBackendOptions),
			tuning: Tuning {
				tick: Duration::from_millis(10),
				view_interval: Duration::from_millis(10),
				ramp: Duration::ZERO,
				..Tuning::default()
			},
			fault: None,
		},
	)
	.unwrap();

	ProjectSource::new(project)
}

fn refresh_until(source: &mut ProjectSource, what: &str, done: impl Fn(&ProjectSource) -> bool) {
	let deadline = Instant::now() + WAIT;
	while !done(source) {
		assert!(Instant::now() < deadline, "timed out waiting for {what}");
		if !source.refresh() {
			std::thread::sleep(Duration::from_millis(10));
		}
	}
}

fn finished(source: &ProjectSource) -> bool {
	source.view().overall.parity.is_some()
}

#[test]
fn the_mirror_follows_folders_rows_and_statuses() {
	let setup = setup(&[("notes.txt", b"n"), ("Docs/a.txt", b"a")]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the finish", finished);

	let library = &source.view().library;
	let root = library.root();
	assert_eq!(library.node(root).stats.files_total, 2);
	assert!(library.node(root).stats.complete);

	let docs = library.node(root).children[0];
	assert_eq!(library.node(docs).name, "Docs");
	assert!(source.files(root).is_none());

	source.perform(Action::Expanded(root, true));
	refresh_until(&mut source, "the root rows", |s| s.files(root).is_some());

	let rows = source.files(root).unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].name, "notes.txt");
	assert_eq!(rows[0].status.state, FileState::Copied);

	let version = source.rows_version();
	source.perform(Action::Expanded(root, false));
	assert!(source.files(root).is_none());
	assert!(source.rows_version() > version);
}

#[test]
fn filter_results_arrive_after_the_request() {
	let setup = setup(&[("Beach/sunset.txt", b"s"), ("other.txt", b"o")]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the finish", finished);

	source.perform(Action::SetFilter("sunset".into()));
	assert!(source.filter().is_none());
	refresh_until(&mut source, "the filter", |s| s.filter().is_some());

	let filter = source.filter().unwrap();
	assert_eq!(filter.query, "sunset");
	assert_eq!(filter.matching_files, 1);
	assert_eq!(filter.rows.len(), 3);

	source.perform(Action::SetFilter(String::new()));
	assert!(source.filter().is_none());
}

#[test]
fn issue_logs_are_served_on_demand() {
	let setup = setup(&[("bad.jpg", b"\xFF\xD8\xFF\xE0 not really a photo")]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the issue", |s| !s.view().issues.is_empty());
	let issue = source.view().issues[0].clone();

	assert_eq!(issue.verbs.accept.as_deref(), Some("Copy original"));

	assert_eq!(source.issue_log(issue.id, 1), None);
	refresh_until(&mut source, "the log", |s| s.issue_log(issue.id, 1).is_some());
	assert_eq!(
		source.issue_log(issue.id, 1).unwrap(),
		["source damaged: truncated", "[mjpeg] premature end of data"]
	);
}

#[test]
fn shutdown_stops_the_run_and_releases_the_project() {
	let setup = setup(&[("a.txt", b"a")]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the finish", finished);

	source.shutdown();

	assert_eq!(source.view().header.run_state, RunState::Stopping);
	drop(source);
	drop(open(&setup.project));
}

#[test]
fn the_dashboard_draws_a_real_project() {
	let setup = setup(&[("a.txt", b"a")]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the finish", finished);
	let mut app = App::new(source, ColorDepth::TrueColor);
	let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
	terminal.draw(|frame| app.draw(frame)).unwrap();
	let screen: String = terminal
		.backend()
		.buffer()
		.content()
		.iter()
		.map(ratatui::buffer::Cell::symbol)
		.collect();
	assert!(!screen.contains("DEMO"));
	assert!(screen.contains("complete"), "{screen}");
	assert!(screen.contains(&setup.source.file_name().unwrap().to_string_lossy().into_owned()));
}

#[test]
fn copy_path_shows_the_full_source_path() {
	let setup = setup(&[("Docs/a.txt", b"a")]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the finish", finished);
	let docs = source.view().library.node(source.view().library.root()).children[0];
	let expected = display_path(&Path::new(&source.view().header.source).join("Docs"));

	source.perform(Action::CopyPath(Target::Folder(docs)));

	let mut notifications = Vec::new();
	let deadline = Instant::now() + WAIT;
	while notifications.is_empty() {
		assert!(Instant::now() < deadline, "timed out waiting for the path");
		source.refresh();
		notifications.extend(source.take_notifications());
		std::thread::sleep(Duration::from_millis(10));
	}

	assert_eq!(notifications[0].level, NoticeLevel::Info);
	assert_eq!(notifications[0].text, expected);
}

#[test]
fn a_source_dropped_in_an_async_context_still_stops_safely() {
	let setup = setup(&[("a.txt", b"a")]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the finish", finished);
	let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();

	runtime.block_on(async move { drop(source) });

	let mut source = open(&setup.project);
	runtime.block_on(async { source.shutdown() });
	assert_eq!(source.view().header.run_state, RunState::Stopping);
}
