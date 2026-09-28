use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cryarchiver_tui::recent::{FILE_NAME, RecentStore};
use cryarchiver_tui::start::form::FormField;
use cryarchiver_tui::start::{Dialog, EntryState};
use cryarchiver_tui::view::NoticeLevel;
use cryarchiver_tui::{ColorDepth, StartMenu};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

struct Setup {
	dir: tempfile::TempDir,
	store: RecentStore,
}

impl Setup {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let store = RecentStore::at(dir.path().join("config").join(FILE_NAME));
		Self { dir, store }
	}

	fn path(&self, relative: &str) -> PathBuf {
		relative
			.split('/')
			.fold(self.dir.path().to_path_buf(), |path, part| path.join(part))
	}

	/// A closed project archiving `<name>` into `archive-<name>`.
	fn project(&self, name: &str) -> PathBuf {
		let source = self.path(name);
		fs::create_dir_all(&source).unwrap();
		cryarchiver_project::create(
			&source,
			&self.path(&format!("archive-{name}")),
			OsStr::new(name),
			|_| {},
		)
		.unwrap()
	}

	/// The menu once every recent project has been read.
	fn menu(&self) -> StartMenu {
		let mut menu = StartMenu::new(self.store.clone(), ColorDepth::TrueColor);
		assert!(menu.wait_for_checks(Duration::from_secs(10)));
		menu
	}
}

fn settled(menu: &mut StartMenu) {
	assert!(menu.wait_for_checks(Duration::from_secs(10)));
}

fn press(menu: &mut StartMenu, code: KeyCode) {
	menu.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), Instant::now());
}

fn paste(menu: &mut StartMenu, text: &str) {
	menu.handle_event(&Event::Paste(text.to_owned()), Instant::now());
}

fn screen(menu: &mut StartMenu, width: u16, height: u16) -> Vec<String> {
	let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
	terminal.draw(|frame| menu.draw(frame)).unwrap();

	let buffer = terminal.backend().buffer();
	(0..height)
		.map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
		.collect()
}

fn shows(menu: &mut StartMenu, text: &str) -> bool {
	screen(menu, 120, 36).iter().any(|line| line.contains(text))
}

fn position(menu: &mut StartMenu, text: &str) -> (u16, u16) {
	let lines = screen(menu, 120, 36);
	lines
		.iter()
		.enumerate()
		.find_map(|(y, line)| {
			let byte = line.find(text)?;
			let x = line[..byte].chars().count();
			Some((u16::try_from(x).unwrap(), u16::try_from(y).unwrap()))
		})
		.unwrap_or_else(|| panic!("{text:?} is not on screen:\n{}", lines.join("\n")))
}

fn click(menu: &mut StartMenu, (column, row): (u16, u16), at: Instant) {
	let event = MouseEvent {
		kind: MouseEventKind::Down(MouseButton::Left),
		column,
		row,
		modifiers: KeyModifiers::NONE,
	};
	menu.handle_event(&Event::Mouse(event), at);
}

fn fill(menu: &mut StartMenu, source: &Path, destination: &Path) {
	paste(menu, &source.display().to_string());
	press(menu, KeyCode::Tab);
	paste(menu, &format!("\"{}\"", destination.display()));
}

#[test]
fn without_recent_projects_the_menu_starts_in_the_form() {
	let setup = Setup::new();
	let mut menu = setup.menu();

	assert!(menu.form().is_some());
	assert!(menu.notice().is_none());
	assert!(shows(&mut menu, "Source folder"));
	assert!(shows(&mut menu, "Create is ready once every check passes"));
}

#[test]
fn recent_projects_show_their_route_and_missing_ones_their_reason() {
	let setup = Setup::new();
	let photos = setup.project("Photos");
	fs::create_dir(setup.path("archive-Music")).unwrap();
	let gone = setup.path("archive-Music/Music.cryarchive");
	setup.store.record(&gone, 1).unwrap();
	setup.store.record(&photos, 2).unwrap();
	let mut menu = setup.menu();

	assert!(menu.form().is_none());
	assert_eq!(menu.entries().len(), 2);
	assert!(shows(&mut menu, "Photos"));
	assert!(shows(&mut menu, "opened on 1970-01-01"));
	assert!(shows(&mut menu, "archive-Photos"));
	assert!(shows(&mut menu, "the project file is missing"));
	assert!(!menu.entries()[0].missing());
	assert!(menu.entries()[1].missing());
}

#[test]
fn the_list_shows_at_once_and_fills_in_as_projects_are_read() {
	let setup = Setup::new();
	let photos = setup.project("Photos");
	setup.store.record(&photos, 1).unwrap();

	let mut menu = StartMenu::new(setup.store.clone(), ColorDepth::TrueColor);

	assert_eq!(menu.entries()[0].state, EntryState::Checking);
	assert!(shows(&mut menu, "checking…"));
	settled(&mut menu);
	assert!(matches!(menu.entries()[0].state, EntryState::Ready(_)));
	assert!(!shows(&mut menu, "checking…"));
}

#[test]
fn a_project_out_of_reach_is_unavailable_and_kept_by_clear_missing() {
	let setup = Setup::new();
	let offline = setup.path("unplugged/archive/Old.cryarchive");
	let folder = setup.path("Folder.cryarchive");
	fs::create_dir(&folder).unwrap();
	setup.store.record(&offline, 1).unwrap();
	setup.store.record(&folder, 2).unwrap();
	setup.store.record(&setup.path("gone.cryarchive"), 3).unwrap();
	let mut menu = setup.menu();

	assert_eq!(menu.entries()[2].state, EntryState::Unavailable);
	assert!(!menu.entries()[2].missing());
	assert!(shows(&mut menu, "unavailable (drive or share offline)"));
	assert!(shows(&mut menu, "not a project file"));

	press(&mut menu, KeyCode::Char('c'));
	settled(&mut menu);

	let names: Vec<&str> = menu.entries().iter().map(|entry| entry.name.as_str()).collect();
	assert_eq!(names, ["Folder", "Old"]);
	assert!(menu.notice().unwrap().text.contains("removed 1 missing project"));
}

#[test]
fn a_project_that_needs_recovery_still_shows_its_route_and_badge() {
	let setup = Setup::new();
	let source = setup.path("Photos");
	fs::create_dir(&source).unwrap();
	let project = cryarchiver_project::create(
		&source,
		&setup.path("archive-Photos"),
		OsStr::new("Photos"),
		|settings| {
			settings.delete_originals = true;
		},
	)
	.unwrap();
	let clean = fs::read(&project).unwrap();
	fs::write(&project, unclean_copy(&clean)).unwrap();
	setup.store.record(&project, 1).unwrap();

	let mut menu = setup.menu();

	let summary = menu.entries()[0].summary().expect("the settings are read").clone();
	assert!(summary.needs_recovery);
	assert!(shows(&mut menu, "⚠ deletes originals"));
	assert!(shows(&mut menu, "archive-Photos"));
	assert!(shows(&mut menu, "opening it recovers it"));
}

/// The same database as it would be on disk while still open: redb only marks a file closed cleanly when it closes.
fn unclean_copy(clean: &[u8]) -> Vec<u8> {
	use redb::StorageBackend;

	#[derive(Debug, Clone)]
	struct Shared(std::sync::Arc<redb::backends::InMemoryBackend>);

	impl StorageBackend for Shared {
		fn len(&self) -> std::io::Result<u64> {
			self.0.len()
		}

		fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
			self.0.read(offset, out)
		}

		fn set_len(&self, len: u64) -> std::io::Result<()> {
			self.0.set_len(len)
		}

		fn sync_data(&self) -> std::io::Result<()> {
			self.0.sync_data()
		}

		fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
			self.0.write(offset, data)
		}
	}

	let memory = Shared(std::sync::Arc::default());
	memory.set_len(clean.len() as u64).unwrap();
	memory.write(0, clean).unwrap();
	let db = redb::Builder::new().create_with_backend(memory.clone()).unwrap();
	db.begin_write().unwrap().commit().unwrap();

	let mut bytes = vec![0; usize::try_from(memory.len().unwrap()).unwrap()];
	memory.read(0, &mut bytes).unwrap();
	std::mem::forget(db);
	bytes
}

#[test]
fn enter_opens_a_project_but_not_a_missing_one() {
	let setup = Setup::new();
	let photos = setup.project("Photos");
	setup.store.record(&setup.path("gone.cryarchive"), 1).unwrap();
	setup.store.record(&photos, 2).unwrap();
	let mut menu = setup.menu();

	press(&mut menu, KeyCode::Down);
	press(&mut menu, KeyCode::Enter);
	assert_eq!(menu.take_request(), None);
	assert_eq!(menu.notice().unwrap().level, NoticeLevel::Warning);

	press(&mut menu, KeyCode::Up);
	press(&mut menu, KeyCode::Enter);
	assert_eq!(menu.take_request(), Some(photos));
	assert!(shows(&mut menu, "Opening Photos…"));
}

#[test]
fn a_failed_open_returns_to_the_menu_with_the_reason() {
	let setup = Setup::new();
	let photos = setup.project("Photos");
	setup.store.record(&photos, 1).unwrap();
	let mut menu = setup.menu();

	press(&mut menu, KeyCode::Enter);
	menu.take_request().unwrap();
	let reason = format!(
		"ffmpeg was not found; {} run \"cryarchiver doctor\" after changing ffmpeg",
		"x".repeat(150)
	);
	menu.open_failed(reason);
	settled(&mut menu);

	assert!(!shows(&mut menu, "Opening Photos"));
	assert!(shows(&mut menu, "Photos didn't open"));
	assert!(shows(&mut menu, "ffmpeg was not found"));
	assert!(shows(&mut menu, "after changing ffmpeg"));
	press(&mut menu, KeyCode::Char('n'));
	assert!(menu.form().is_none());

	press(&mut menu, KeyCode::Enter);
	assert!(menu.dialog().is_none());
	assert!(menu.take_request().is_none());
	press(&mut menu, KeyCode::Enter);
	assert!(menu.take_request().is_some());
}

#[test]
fn create_checks_again_and_creates_nothing_once_the_source_is_gone() {
	let setup = Setup::new();
	let source = setup.path("Holiday");
	fs::create_dir(&source).unwrap();
	let mut menu = setup.menu();
	fill(&mut menu, &source, &setup.path("archive"));
	fs::remove_dir(&source).unwrap();

	for _ in 0..4 {
		press(&mut menu, KeyCode::Tab);
	}
	press(&mut menu, KeyCode::Enter);

	assert!(menu.form().is_some());
	assert_eq!(menu.take_request(), None);
	assert!(!setup.path("archive").exists());
}

#[test]
fn remove_and_clear_missing_edit_the_saved_list() {
	let setup = Setup::new();
	let (photos, music) = (setup.project("Photos"), setup.project("Music"));
	setup.store.record(&setup.path("gone.cryarchive"), 1).unwrap();
	setup.store.record(&music, 2).unwrap();
	setup.store.record(&photos, 3).unwrap();
	let mut menu = setup.menu();

	press(&mut menu, KeyCode::Char('x'));
	press(&mut menu, KeyCode::Char('c'));

	let saved: Vec<PathBuf> = setup.store.load().entries.into_iter().map(|entry| entry.path).collect();
	assert_eq!(saved, [music]);
	assert_eq!(menu.entries().len(), 1);
	assert!(menu.notice().unwrap().text.contains("removed 1 missing project"));
}

#[test]
fn clicks_select_and_a_double_click_opens() {
	let setup = Setup::new();
	let (photos, music) = (setup.project("Photos"), setup.project("Music"));
	setup.store.record(&music, 1).unwrap();
	setup.store.record(&photos, 2).unwrap();
	let mut menu = setup.menu();
	let at = Instant::now();

	let music_row = position(&mut menu, "Music");
	click(&mut menu, music_row, at);
	assert_eq!(menu.selected(), 1);
	assert_eq!(menu.take_request(), None);

	click(&mut menu, music_row, at + Duration::from_millis(100));
	assert_eq!(menu.take_request(), Some(music));
}

#[test]
fn the_form_creates_the_project_opens_it_and_lists_it() {
	let setup = Setup::new();
	let source = setup.path("Holiday");
	fs::create_dir(&source).unwrap();
	let destination = setup.path("backup/archive");
	let mut menu = setup.menu();

	fill(&mut menu, &source, &destination);
	let form = menu.form().unwrap();
	assert_eq!(form.value(FormField::Name), "Holiday");
	assert_eq!(form.value(FormField::Destination), destination.display().to_string());
	assert!(form.can_create(), "{:?}", form.validation());
	assert!(shows(&mut menu, "Holiday.cryarchive"));

	for _ in 0..4 {
		press(&mut menu, KeyCode::Tab);
	}
	assert_eq!(menu.form().unwrap().focus(), FormField::Create);
	press(&mut menu, KeyCode::Enter);

	let project = destination.join("Holiday.cryarchive");
	assert!(project.is_file());
	assert!(menu.form().is_none());
	assert_eq!(menu.take_request(), Some(project.clone()));
	assert_eq!(setup.store.load().entries[0].path, project);
}

#[test]
fn create_stays_disabled_while_a_check_fails() {
	let setup = Setup::new();
	let source = setup.path("Holiday");
	fs::create_dir(&source).unwrap();
	let mut menu = setup.menu();

	fill(&mut menu, &source, &source.join("archive"));
	assert!(shows(&mut menu, "✗ the destination is inside the source"));

	let create = position(&mut menu, "[ Create ]");
	click(&mut menu, create, Instant::now());
	assert_eq!(menu.take_request(), None);
	assert!(!source.join("archive").exists());
}

#[test]
fn turning_deletion_on_asks_for_delete_and_shows_the_warning() {
	let setup = Setup::new();
	let mut menu = setup.menu();
	for _ in 0..4 {
		press(&mut menu, KeyCode::Tab);
	}
	assert_eq!(menu.form().unwrap().focus(), FormField::DeleteOriginals);

	press(&mut menu, KeyCode::Char(' '));
	assert!(shows(&mut menu, "Type DELETE to turn it on:"));
	press(&mut menu, KeyCode::Char('q'));
	press(&mut menu, KeyCode::Enter);
	assert!(!menu.should_quit());
	assert!(!menu.form().unwrap().delete_originals());

	press(&mut menu, KeyCode::Enter);
	paste(&mut menu, "DELETE");
	press(&mut menu, KeyCode::Enter);
	assert!(menu.form().unwrap().delete_originals());
	assert!(shows(&mut menu, "Deleting is permanent"));
}

#[test]
fn pasted_keys_only_reach_text_fields() {
	let setup = Setup::new();
	setup.store.record(&setup.path("gone.cryarchive"), 1).unwrap();
	let mut menu = setup.menu();
	let start = Instant::now();
	let burst = |text: &str| -> Vec<(Event, Instant)> {
		text.chars()
			.enumerate()
			.map(|(index, c)| {
				let key = Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
				(key, start + Duration::from_micros(u64::try_from(index).unwrap() * 100))
			})
			.collect()
	};

	menu.handle_batch(&burst("xq"));
	assert!(!menu.should_quit());
	assert_eq!(menu.entries().len(), 1);

	press(&mut menu, KeyCode::Char('n'));
	menu.handle_batch(&burst("qn"));
	assert_eq!(menu.form().unwrap().value(FormField::Source), "qn");
}

#[test]
fn escape_leaves_the_form_and_q_quits_the_menu() {
	let setup = Setup::new();
	let mut menu = setup.menu();

	press(&mut menu, KeyCode::Esc);
	assert!(menu.form().is_none());
	assert!(shows(&mut menu, "No recent projects yet."));

	press(&mut menu, KeyCode::Char('q'));
	assert!(menu.should_quit());
}

#[test]
fn escape_in_an_edited_form_asks_before_discarding() {
	let setup = Setup::new();
	let mut menu = setup.menu();

	press(&mut menu, KeyCode::Char('q'));
	assert!(!menu.should_quit());
	press(&mut menu, KeyCode::Esc);
	assert_eq!(menu.dialog(), Some(&Dialog::Discard));
	assert!(shows(&mut menu, "Discard the new project?"));

	press(&mut menu, KeyCode::Esc);
	assert!(menu.dialog().is_none());
	assert_eq!(menu.form().unwrap().value(FormField::Source), "q");

	press(&mut menu, KeyCode::Esc);
	let keep = position(&mut menu, "Esc  keep editing");
	click(&mut menu, keep, Instant::now());
	assert!(menu.dialog().is_none());
	assert!(menu.form().is_some());

	press(&mut menu, KeyCode::Esc);
	let discard = position(&mut menu, "⏎  discard");
	click(&mut menu, discard, Instant::now());
	assert!(menu.form().is_none());
}

#[test]
fn the_delete_prompt_chips_work_with_the_mouse() {
	let setup = Setup::new();
	let mut menu = setup.menu();
	for _ in 0..4 {
		press(&mut menu, KeyCode::Tab);
	}

	press(&mut menu, KeyCode::Char(' '));
	let cancel = position(&mut menu, "Esc  cancel");
	click(&mut menu, cancel, Instant::now());
	assert!(menu.form().unwrap().confirming().is_none());
	assert!(!menu.form().unwrap().delete_originals());

	press(&mut menu, KeyCode::Char(' '));
	paste(&mut menu, "DELETE");
	let confirm = position(&mut menu, "⏎  confirm");
	click(&mut menu, confirm, Instant::now());
	assert!(menu.form().unwrap().confirming().is_none());
	assert!(menu.form().unwrap().delete_originals());
}

#[test]
fn a_small_terminal_shows_the_fallback() {
	let setup = Setup::new();
	let mut menu = setup.menu();

	let lines = screen(&mut menu, 80, 24);

	assert!(lines.iter().any(|line| line.contains("Terminal too small")));
	assert!(lines.iter().any(|line| line.contains("80×24 · needs at least 100×30")));
}

#[test]
fn the_form_fits_the_smallest_terminal() {
	let setup = Setup::new();
	let mut menu = setup.menu();

	let lines = screen(&mut menu, 100, 30);

	assert!(lines.iter().any(|line| line.contains("[ Create ]")));
	assert!(lines[29].contains("Esc"));
}

#[test]
fn a_corrupt_list_starts_empty_with_a_notice() {
	let setup = Setup::new();
	let file = setup.store.file().unwrap();
	fs::create_dir_all(file.parent().unwrap()).unwrap();
	fs::write(file, b"\xc1").unwrap();

	let menu = setup.menu();

	assert!(menu.entries().is_empty());
	assert_eq!(menu.notice().unwrap().level, NoticeLevel::Warning);
}
