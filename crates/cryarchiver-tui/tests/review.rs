mod support;

use std::time::{Duration, Instant};

use cryarchiver_tui::source::SettingsChange;
use cryarchiver_tui::view::{FilterEntry, NoticeLevel, RunState};
use cryarchiver_tui::{Action, App, ArchiveSource, ColorDepth, MockSource, OverlayKind};
use insta::assert_snapshot;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};

use support::{app, draw, find_row, mouse, press, row_text, type_text};

const FIRST_BODY_ROW: u16 = 12;

fn keys_at(start: Instant, text: &str, gap: Duration) -> Vec<(Event, Instant)> {
	(0u32..)
		.zip(text.chars())
		.map(|(index, c)| {
			let code = if c == '\n' { KeyCode::Enter } else { KeyCode::Char(c) };
			(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), start + gap * index)
		})
		.collect()
}

#[test]
fn a_rebuild_keeps_the_viewport_where_the_wheel_left_it() {
	let mut app = app();
	draw(&mut app, 120, 36);
	mouse(&mut app, MouseEventKind::ScrollDown, 40, 20, Instant::now());
	let before = row_text(&draw(&mut app, 120, 36), FIRST_BODY_ROW);

	let desktop = {
		let library = &app.source().view().library;
		library.ids().find(|&id| library.node(id).name == "Documents").unwrap()
	};
	app.source_mut().perform(Action::Expanded(desktop, true));
	app.source_mut().step();

	assert_eq!(row_text(&draw(&mut app, 120, 36), FIRST_BODY_ROW), before);
}

#[test]
fn a_new_filter_result_keeps_the_viewport_too() {
	let mut app = app();
	press(&mut app, KeyCode::Char('/'));
	type_text(&mut app, "Screenshot 2019");
	press(&mut app, KeyCode::Enter);
	draw(&mut app, 120, 36);
	mouse(&mut app, MouseEventKind::ScrollDown, 40, 20, Instant::now());
	let before = row_text(&draw(&mut app, 120, 36), FIRST_BODY_ROW);

	let version = app.source().rows_version();
	while app.source().rows_version() == version {
		app.source_mut().step();
	}

	assert_eq!(row_text(&draw(&mut app, 120, 36), FIRST_BODY_ROW), before);
}

#[test]
fn a_paste_typed_as_keys_never_reaches_shortcuts() {
	let mut app = app();
	let start = Instant::now();

	app.handle_batch(&keys_at(start, "quiz", Duration::from_millis(1)));

	assert!(!app.should_quit());
	assert!(!app.maximized());
	assert_eq!(app.mode(), cryarchiver_tui::ViewMode::Library);
}

#[test]
fn a_paste_into_search_keeps_its_newline_out() {
	let mut app = app();
	press(&mut app, KeyCode::Char('/'));

	app.handle_batch(&keys_at(Instant::now(), "over\nlord", Duration::from_millis(1)));

	assert!(app.searching(), "a pasted newline doesn't close the search");
	assert_eq!(app.filter(), "fanfares");
}

#[test]
fn slow_keys_in_one_batch_still_act() {
	let mut app = app();

	app.handle_batch(&keys_at(Instant::now(), "z", Duration::from_millis(80)));

	assert!(app.maximized());
}

#[test]
fn the_most_severe_notification_wins() {
	let mut app = app();
	app.source_mut()
		.perform(Action::ChangeSettings(SettingsChange::DeleteOriginals {
			enabled: true,
			confirmation: "nope".to_owned(),
		}));
	app.source_mut().perform(Action::Rescan);
	app.refresh();
	assert_eq!(app.toast().map(|toast| toast.level), Some(NoticeLevel::Error));

	app.source_mut().perform(Action::Rescan);
	app.refresh();
	assert_eq!(
		app.toast().map(|toast| toast.level),
		Some(NoticeLevel::Error),
		"a fresh error isn't replaced by info"
	);
}

#[test]
fn the_toast_moves_to_the_footer_while_the_log_is_hidden() {
	let mut app = app();
	press(&mut app, KeyCode::Char('l'));
	press(&mut app, KeyCode::Char('e'));
	let terminal = draw(&mut app, 120, 36);
	assert!(row_text(&terminal, 35).contains("◆ performance limit 25 %"));

	press(&mut app, KeyCode::Char('l'));
	press(&mut app, KeyCode::Char('z'));
	let terminal = draw(&mut app, 120, 36);
	assert!(
		row_text(&terminal, 35).contains("◆ performance limit"),
		"maximized hides the log too"
	);
}

#[test]
fn clearing_the_filter_keeps_the_selected_file() {
	let mut app = app();
	press(&mut app, KeyCode::Char('/'));
	type_text(&mut app, "テスト");
	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::End);
	assert_eq!(app.selected_name().as_deref(), Some("01.テスト.mp3"));

	press(&mut app, KeyCode::Esc);

	assert_eq!(app.filter(), "");
	assert_eq!(app.selected_name().as_deref(), Some("01.テスト.mp3"));
	assert!(find_row(&draw(&mut app, 120, 36), "✓ 01.").is_some());
}

#[test]
fn enter_on_a_folder_does_nothing_while_filtered() {
	let mut app = app();
	press(&mut app, KeyCode::Char('/'));
	type_text(&mut app, "fanfares");
	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Home);
	let rows = app.visible_rows();

	press(&mut app, KeyCode::Enter);

	assert_eq!(app.visible_rows(), rows);
}

#[test]
fn statuses_update_in_place_without_a_new_rows_version() {
	let mut source = MockSource::new(4);
	let bled = {
		let library = &source.view().library;
		library.ids().find(|&id| library.node(id).name == "Bled").unwrap()
	};
	source.perform(Action::Expanded(bled, true));
	source.perform(Action::SetFilter("IMG_0001".to_owned()));
	let version = source.rows_version();
	let progress = |source: &MockSource| {
		source
			.files(bled)
			.unwrap()
			.iter()
			.find(|row| row.name == "IMG_0001 2.MOV")
			.unwrap()
			.status
			.progress
	};
	let before = progress(&source);

	source.step();

	assert_eq!(
		source.rows_version(),
		version,
		"no file was added or removed on the first tick"
	);
	assert_ne!(progress(&source), before);
	let filtered = source.filter().unwrap().rows.iter().find_map(|row| match &row.entry {
		FilterEntry::File { row, .. } => Some(row.status.progress),
		FilterEntry::Folder(_) => None,
	});
	assert_eq!(filtered, Some(progress(&source)));
}

#[test]
fn f2_works_behind_help() {
	let mut app = app();
	press(&mut app, KeyCode::Char('?'));

	press(&mut app, KeyCode::F(2));

	assert!(!app.mouse_capture());
	assert_eq!(app.overlay(), Some(OverlayKind::Help));
}

#[test]
fn a_finished_run_offers_no_pause() {
	let mut app = App::new(MockSource::completed(support::SEED), ColorDepth::TrueColor);

	let footer = row_text(&draw(&mut app, 160, 45), 44);

	assert_eq!(app.source().view().header.run_state, RunState::Finished);
	assert!(!footer.contains("pause") && !footer.contains("resume"));
}

#[test]
fn shutdown_closes_an_open_search_line() {
	let mut app = app();
	press(&mut app, KeyCode::Char('/'));

	app.begin_shutdown();

	assert!(!app.searching());
}

#[test]
fn crowded_header_at_100x30() {
	let mut source = support::source();
	for _ in support::WARM_UP_STEPS..80 {
		source.step();
	}
	source.perform(Action::ChangeSettings(SettingsChange::DeleteOriginals {
		enabled: true,
		confirmation: "DELETE".to_owned(),
	}));
	source.perform(Action::ChangeSettings(SettingsChange::PerformanceLimit(Some(25))));
	let mut app = App::new(source, ColorDepth::TrueColor);
	let terminal = draw(&mut app, 100, 30);
	let header = row_text(&terminal, 0);

	assert_eq!(app.source().view().header.run_state, RunState::PausedLowDisk);
	assert!(
		header.contains(" DEMO ") && header.contains("DELETING ORIGINALS"),
		"{header}"
	);
	assert!(header.contains("limit 25 %"), "the limit chip survives: {header}");
	assert!(header.contains("‖ paused: low disk space"), "{header}");
	assert_snapshot!(terminal.backend());
}
