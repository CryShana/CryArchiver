use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cryarchiver_engine::commit::OutputSink;
use cryarchiver_engine::decision::{CrfChoice, Decision, VideoDecision};
use cryarchiver_engine::engine::{Encode, Job, JobEvent, MemoryGate};
use cryarchiver_engine::options::BluRay;
use cryarchiver_engine::outcome::{ErrorKind, JobError, JobOutcome, KeepReason};
use cryarchiver_engine::preset::{Overrides, VmafOverride};
use cryarchiver_engine::quality::memory::QualityMemory;
use cryarchiver_engine::text::display_path;
use cryarchiver_project::{Backend, Project, ProjectOptions, SystemSample, Tuning};
use cryarchiver_tui::recent::{FILE_NAME, RecentStore};
use cryarchiver_tui::source::{SettingsChange, Target};
use cryarchiver_tui::view::{FileState, Filter, FilterEntry, NoticeLevel, RunState, StateFilter};
use cryarchiver_tui::{Action, App, ArchiveSource, ColorDepth, OverlayKind, ProjectSource, StartMenu};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(20);

/// Copies everything, except files named `bad.jpg`, which fail as damaged, and `kept.mp4`, which the engine keeps
/// after deciding on a learned CRF.
struct CopyEverything;

fn learned_decision() -> Decision {
	Decision {
		video: Some(VideoDecision {
			encoder: "SVT-AV1 preset 5".into(),
			crf: Some(30.0),
			chosen: Some(CrfChoice::Learned),
			near_miss: true,
			..VideoDecision::default()
		}),
		..Decision::default()
	}
}

impl Encode for CopyEverything {
	fn encode<'a>(
		&'a self,
		job: &'a Job,
		_sink: &'a dyn OutputSink,
		on_event: &'a (dyn Fn(JobEvent) + Sync),
		_cancel: &'a CancellationToken,
	) -> Pin<Box<dyn Future<Output = JobOutcome> + Send + 'a>> {
		Box::pin(async move {
			if job.source.file_name() == Some(OsStr::new("bad.jpg")) {
				JobOutcome::Failed(JobError::with_log(
					ErrorKind::SourceDamaged("truncated".into()),
					vec!["[mjpeg] premature end of data".into()],
				))
			} else if job.source.file_name() == Some(OsStr::new("kept.mp4")) {
				on_event(JobEvent::Decision(Box::new(learned_decision())));
				JobOutcome::KeptOriginal {
					reason: KeepReason::NotSmaller {
						src_bytes: 10,
						out_bytes: 10,
					},
				}
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
	fn encoder(
		&self,
		_image_threads: usize,
		_memory: Arc<dyn MemoryGate>,
		_quality: Arc<dyn QualityMemory>,
	) -> Arc<dyn Encode> {
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
	setup_with(files, false)
}

fn setup_with(files: &[(&str, &[u8])], delete_originals: bool) -> Setup {
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
			settings.delete_originals = delete_originals;
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

fn screen(app: &mut App<ProjectSource>) -> String {
	let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
	terminal.draw(|frame| app.draw(frame)).unwrap();
	terminal
		.backend()
		.buffer()
		.content()
		.iter()
		.map(ratatui::buffer::Cell::symbol)
		.collect()
}

fn press(app: &mut App<ProjectSource>, code: KeyCode) {
	app.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), Instant::now());
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

	source.perform(Action::SetFilter(Filter::name("sunset")));
	assert!(source.filter().is_none());
	refresh_until(&mut source, "the filter", |s| s.filter().is_some());

	let filter = source.filter().unwrap();
	assert_eq!(filter.query, "sunset");
	assert_eq!(filter.matching_files, 1);
	assert_eq!(filter.rows.len(), 3);

	source.perform(Action::SetFilter(Filter::default()));
	assert!(source.filter().is_none());
}

#[test]
fn a_state_filter_reaches_the_project() {
	let setup = setup(&[
		("Photos/bad.jpg", b"\xFF\xD8\xFF\xE0 not really a photo".as_slice()),
		("other.txt", b"o".as_slice()),
	]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the issue", |s| !s.view().issues.is_empty());

	source.perform(Action::SetFilter(Filter {
		query: String::new(),
		state: Some(StateFilter::Issues),
	}));
	refresh_until(&mut source, "the filter", |s| s.filter().is_some());

	let filter = source.filter().unwrap();
	assert_eq!(filter.state, Some(StateFilter::Issues));
	assert_eq!(filter.matching_files, 1);
	assert_eq!(filter.rows.len(), 3, "the root, Photos and the failed photo");
	assert!(matches!(&filter.rows[2].entry, FilterEntry::File { row, .. } if row.name == "bad.jpg"));
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

	let path = notifications
		.iter()
		.find(|notice| notice.text == format!("copied {expected}"))
		.unwrap_or_else(|| panic!("no notice with the path: {notifications:?}"));
	assert_eq!(path.level, NoticeLevel::Info);
	assert_eq!(source.take_clipboard(), Some(expected));
	assert_eq!(source.take_clipboard(), None);
}

#[test]
fn a_deleting_project_archives_but_deletes_nothing_until_delete_is_typed_this_session() {
	let setup = setup_with(&[("notes.txt", b"notes")], true);
	let original = setup.source.join("notes.txt");
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the archive", finished);

	assert!(original.exists());
	assert!(source.view().header.deleting_originals);
	assert!(source.view().header.deleting_paused);
	assert_eq!(source.view().overall.originals_deleted, 0);

	let mut app = App::new(source, ColorDepth::TrueColor);
	assert!(screen(&mut app).contains("deleting paused: type DELETE to allow"));
	press(&mut app, KeyCode::Char('s'));
	for _ in 0..11 {
		press(&mut app, KeyCode::Down);
	}
	press(&mut app, KeyCode::Enter);
	assert_eq!(app.overlay(), Some(OverlayKind::ConfirmDelete));
	app.handle_event(&Event::Paste("DELETE".to_owned()), Instant::now());
	press(&mut app, KeyCode::Enter);

	let deadline = Instant::now() + WAIT;
	while app.source().view().overall.originals_deleted < 1 {
		assert!(Instant::now() < deadline, "timed out waiting for the delete");
		if !app.refresh() {
			std::thread::sleep(Duration::from_millis(10));
		}
	}
	assert!(!original.exists());
	assert!(!app.source().view().header.deleting_paused);
	let shown = screen(&mut app);
	assert!(!shown.contains("deleting paused"));
	assert!(shown.contains("⊗ 1 original deleted"), "{shown}");
}

#[test]
fn the_delete_typed_when_creating_in_the_start_menu_does_not_confirm_the_session() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();
	fs::write(source.join("notes.txt"), b"notes").unwrap();
	let store = RecentStore::at(dir.path().join(FILE_NAME));
	let mut menu = StartMenu::new(store, ColorDepth::TrueColor);
	let key = |menu: &mut StartMenu, code: KeyCode| {
		menu.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), Instant::now());
	};

	menu.handle_event(&Event::Paste(display_path(&source)), Instant::now());
	key(&mut menu, KeyCode::Tab);
	menu.handle_event(&Event::Paste(display_path(&dir.path().join("archive"))), Instant::now());
	for _ in 0..5 {
		key(&mut menu, KeyCode::Tab);
	}
	key(&mut menu, KeyCode::Enter);
	menu.handle_event(&Event::Paste("DELETE".into()), Instant::now());
	key(&mut menu, KeyCode::Enter);
	key(&mut menu, KeyCode::Tab);
	key(&mut menu, KeyCode::Enter);
	let project = menu.take_request().expect("the form created the project");
	assert!(cryarchiver_project::peek(&project).unwrap().delete_originals);

	let mut source_view = open(&project);
	refresh_until(&mut source_view, "the run to start", |s| {
		s.view().header.run_state != RunState::Paused
	});

	assert!(source_view.view().header.deleting_originals);
	assert!(source_view.view().header.deleting_paused);
	assert!(source.join("notes.txt").exists());
}

#[test]
fn a_real_project_shows_only_what_it_knows() {
	let setup = setup(&[("a.txt", b"a")]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the finish", finished);
	let root = source.view().library.root();
	source.perform(Action::Expanded(root, true));
	refresh_until(&mut source, "the rows", |s| s.files(root).is_some());
	let file = source.files(root).unwrap()[0].id;

	assert_eq!(source.view().header.codecs, None);
	assert_eq!(source.view().workers.slots, "");
	refresh_until(&mut source, "the details", |s| s.details(file).is_some());
	let details = source.details(file).unwrap();
	assert!(details.stages.is_empty(), "no job runs, so no stage");
	assert!(details.log_tail.is_empty(), "the engine sends no live tool output");
	assert_eq!((details.quality, details.estimated_out), (None, None));
	assert_eq!(details.source_info, "1 B");

	let mut app = App::new(source, ColorDepth::TrueColor);
	let screen = screen(&mut app);
	assert!(!screen.contains("LATEST"), "{screen}");
	assert!(!screen.contains("running ·"), "{screen}");
}

#[test]
fn file_details_show_what_the_engine_decided() {
	let setup = setup(&[("kept.mp4", b"\xFF\xD8\xFF\xE0 kept")]);
	let mut source = open(&setup.project);
	refresh_until(&mut source, "the finish", finished);
	let root = source.view().library.root();
	source.perform(Action::Expanded(root, true));
	refresh_until(&mut source, "the rows", |s| s.files(root).is_some());
	let file = source.files(root).unwrap()[0].id;

	refresh_until(&mut source, "the decision", |s| {
		s.details(file).is_some_and(|details| !details.decision.is_empty())
	});
	let details = source.details(file).unwrap();

	assert_eq!(details.state, FileState::KeptOriginal);
	let lines: Vec<(&str, &str, bool)> = details
		.decision
		.iter()
		.map(|line| (line.label.as_str(), line.value.as_str(), line.marked))
		.collect();
	assert_eq!(
		lines,
		[
			("Kept", "encoding saved too little (10 B → 10 B)", false),
			("Encoder", "SVT-AV1 preset 5", false),
			("CRF", "30 (learned)", false),
			("Target", "near miss: written slightly below the target", true),
		]
	);
}

#[test]
fn quality_overrides_reach_the_project_and_come_back_normalized() {
	let setup = setup(&[("a.txt", b"a")]);
	let mut source = open(&setup.project);
	assert_eq!(source.view().settings.overrides, Overrides::default());

	source.perform(Action::ChangeSettings(SettingsChange::Overrides(Overrides {
		image: Some(80.04),
		audio_stereo_kbps: Some(160),
		..Overrides::default()
	})));
	let custom = Overrides {
		image: Some(80.0),
		audio_stereo_kbps: Some(160),
		..Overrides::default()
	};
	refresh_until(&mut source, "the new overrides", |s| {
		s.view().settings.overrides == custom
	});

	let mut app = App::new(source, ColorDepth::TrueColor);
	assert!(screen(&mut app).contains(" MODERATE · custom "));
	drop(app);

	let mut source = open(&setup.project);
	assert_eq!(source.view().settings.overrides, custom);
	refresh_until(&mut source, "the finish", finished);
}

#[test]
fn the_blu_ray_setting_reaches_the_project_and_is_stored() {
	let setup = setup(&[("a.txt", b"a")]);
	let mut source = open(&setup.project);
	assert_eq!(source.view().settings.blu_ray, BluRay::Keep);

	source.perform(Action::ChangeSettings(SettingsChange::BluRay(BluRay::Convert)));
	refresh_until(&mut source, "the setting", |s| {
		s.view().settings.blu_ray == BluRay::Convert
	});

	let mut app = App::new(source, ColorDepth::TrueColor);
	press(&mut app, KeyCode::Char('s'));
	assert!(screen(&mut app).contains("convert main feature"));
	drop(app);

	let mut source = open(&setup.project);
	assert_eq!(source.view().settings.blu_ray, BluRay::Convert);
	refresh_until(&mut source, "the finish", finished);
}

#[test]
fn a_project_shows_its_stored_overrides_as_the_engine_reads_them() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();
	let project = cryarchiver_project::create(
		&source,
		&dir.path().join("archive"),
		OsStr::new("archive"),
		|settings| {
			settings.overrides = Overrides {
				video: Some(VmafOverride {
					mean: 90.0,
					floor: 85.0,
				}),
				..Overrides::default()
			};
			settings.speed = cryarchiver_project::settings::Speed::Fast;
		},
	)
	.unwrap();

	let mut app = App::new(open(&project), ColorDepth::TrueColor);
	let shown = screen(&mut app);

	assert!(shown.contains(" FAST "), "{shown}");
	assert!(
		!shown.contains("custom"),
		"a video override is not used at FAST: {shown}"
	);
	press(&mut app, KeyCode::Char('s'));
	let shown = screen(&mut app);
	assert!(
		shown.contains("90/85 · not used: FAST never measures video quality"),
		"{shown}"
	);
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
