use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cryarchiver_tui::recent::{FILE_NAME, RecentStore};
use cryarchiver_tui::start::form::FormField;
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

	fn menu(&self) -> StartMenu {
		StartMenu::new(self.store.clone(), ColorDepth::TrueColor)
	}
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
	assert!(!menu.entries()[0].missing);
	assert!(menu.entries()[1].missing);
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
	menu.open_failed("ffmpeg was not found".into());

	assert!(!shows(&mut menu, "Opening Photos"));
	assert!(shows(&mut menu, "ffmpeg was not found"));
	press(&mut menu, KeyCode::Enter);
	assert!(menu.take_request().is_some());
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

	press(&mut menu, KeyCode::Char('q'));
	assert!(!menu.should_quit());
	press(&mut menu, KeyCode::Esc);
	assert!(menu.form().is_none());
	assert!(shows(&mut menu, "No recent projects yet."));

	press(&mut menu, KeyCode::Char('q'));
	assert!(menu.should_quit());
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
	fs::create_dir_all(setup.store.file().parent().unwrap()).unwrap();
	fs::write(setup.store.file(), b"\xc1").unwrap();

	let menu = setup.menu();

	assert!(menu.entries().is_empty());
	assert_eq!(menu.notice().unwrap().level, NoticeLevel::Warning);
}
