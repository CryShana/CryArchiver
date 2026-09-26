mod support;

use std::time::{Duration, Instant};

use cryarchiver_tui::view::{IssueKind, Limit, RunState};
use cryarchiver_tui::{ArchiveSource, OverlayKind, Selection, ViewMode};
use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use support::{app, click, draw, find_on_row, find_row, mouse, press, press_with, release, type_text};

#[test]
fn z_toggles_maximize_and_esc_restores() {
	let mut app = app();

	press(&mut app, KeyCode::Char('z'));
	assert!(app.maximized());

	press(&mut app, KeyCode::Char('z'));
	assert!(!app.maximized());

	press(&mut app, KeyCode::Char('z'));
	press(&mut app, KeyCode::Esc);
	assert!(!app.maximized());
}

#[test]
fn esc_unwinds_overlay_then_details_then_filter_then_maximize() {
	let mut app = app();
	press(&mut app, KeyCode::Char('z'));
	press(&mut app, KeyCode::Char('/'));
	type_text(&mut app, "fanfares");
	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::End);
	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Char('?'));

	assert_eq!(app.overlay(), Some(OverlayKind::Help));
	assert_eq!(app.mode(), ViewMode::Details);
	assert_eq!(app.filter(), "fanfares");

	press(&mut app, KeyCode::Esc);
	assert_eq!(app.overlay(), None);
	assert_eq!(app.mode(), ViewMode::Details);

	press(&mut app, KeyCode::Esc);
	assert_eq!(app.mode(), ViewMode::Library);
	assert_eq!(app.filter(), "fanfares");

	press(&mut app, KeyCode::Esc);
	assert_eq!(app.filter(), "");
	assert!(app.maximized());

	press(&mut app, KeyCode::Esc);
	assert!(!app.maximized());
}

#[test]
fn l_toggles_the_log() {
	let mut app = app();

	press(&mut app, KeyCode::Char('l'));
	assert!(!app.log_open());

	press(&mut app, KeyCode::Char('l'));
	assert!(app.log_open());
}

#[test]
fn question_mark_opens_help_and_esc_closes_it() {
	let mut app = app();

	press(&mut app, KeyCode::Char('?'));
	assert_eq!(app.overlay(), Some(OverlayKind::Help));

	press(&mut app, KeyCode::Char('z'));
	assert!(!app.maximized(), "keys behind an overlay do nothing");

	press(&mut app, KeyCode::Esc);
	assert_eq!(app.overlay(), None);
}

#[test]
fn menu_offers_issue_actions_only_for_issues() {
	let mut app = app();

	press(&mut app, KeyCode::Char('m'));
	let items = app.menu_items();
	assert_eq!(app.overlay(), Some(OverlayKind::Menu));
	assert!(items.iter().any(|item| item == "Show details"));
	assert!(!items.iter().any(|item| item == "Retry"));

	press(&mut app, KeyCode::Esc);
	press(&mut app, KeyCode::Char('i'));
	press(&mut app, KeyCode::Down);
	press(&mut app, KeyCode::Char('m'));
	let items = app.menu_items();
	assert_eq!(&items[..3], ["Retry", "Copy original", "Exclude"]);
}

#[test]
fn folder_menu_expands_everything_below() {
	let mut app = app();
	press(&mut app, KeyCode::Home);
	let before = app.visible_rows();

	press(&mut app, KeyCode::Char('m'));
	assert_eq!(app.menu_items()[0], "Expand all");
	press(&mut app, KeyCode::Enter);

	let folders = app.source().view().library.len();
	assert_eq!(app.overlay(), None);
	assert!(app.visible_rows() >= folders && app.visible_rows() > before);
	assert!(
		app.visible_rows() < 1_000,
		"expand all opens folders only, never every file list"
	);
}

#[test]
fn search_filters_live_enter_keeps_and_esc_clears() {
	let mut app = app();
	let all_rows = app.visible_rows();

	press(&mut app, KeyCode::Char('/'));
	assert!(app.searching());
	type_text(&mut app, "テスト");
	assert_eq!(app.filter(), "テスト");
	assert_eq!(app.visible_rows(), 4, "the match plus Family, Music and Anime OST");

	press(&mut app, KeyCode::Enter);
	assert!(!app.searching());
	assert_eq!(app.filter(), "テスト");

	press(&mut app, KeyCode::Char('/'));
	press(&mut app, KeyCode::Esc);
	assert!(!app.searching());
	assert_eq!(app.filter(), "");
	assert_eq!(app.visible_rows(), all_rows);
}

#[test]
fn issues_view_opens_details_of_the_selected_issue() {
	let mut app = app();

	press(&mut app, KeyCode::Char('i'));
	assert_eq!(app.mode(), ViewMode::Issues);

	press(&mut app, KeyCode::Down);
	assert_eq!(app.selected_issue(), 1);
	let issue = &app.source().view().issues[1];
	let (file, folder) = (issue.file.unwrap(), issue.folder.unwrap());

	press(&mut app, KeyCode::Enter);
	assert_eq!(app.mode(), ViewMode::Details);
	assert_eq!(app.selected(), Some(Selection::File { file, folder }));

	press(&mut app, KeyCode::Char('i'));
	press(&mut app, KeyCode::Char('i'));
	assert_eq!(app.mode(), ViewMode::Library);
}

#[test]
fn issue_actions_reach_the_source() {
	let mut app = app();
	let issues = app.source().view().issues.len();
	assert_eq!(app.source().view().issues[0].kind, IssueKind::OutputMissing);

	press(&mut app, KeyCode::Char('i'));
	press(&mut app, KeyCode::Char('x'));
	assert_eq!(
		app.source().view().issues.len(),
		issues,
		"a missing output can't be excluded"
	);

	press(&mut app, KeyCode::Char('c'));
	assert_eq!(app.source().view().issues.len(), issues - 1, "c accepts it as is");
}

#[test]
fn details_step_through_files_and_skip_folders() {
	let mut app = app();
	press(&mut app, KeyCode::Enter);
	let first = app.selected().unwrap();

	press(&mut app, KeyCode::Down);
	let second = app.selected().unwrap();

	assert_ne!(first, second);
	assert!(matches!(second, Selection::File { .. }));
	assert_eq!(app.mode(), ViewMode::Details);

	press(&mut app, KeyCode::Backspace);
	assert_eq!(app.mode(), ViewMode::Library);
	assert_eq!(app.selected(), Some(second));
}

#[test]
fn g_goes_back_and_forth_between_a_duplicate_and_its_original() {
	let mut app = app();
	press(&mut app, KeyCode::Char('/'));
	type_text(&mut app, "fanfares");
	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::End);
	press(&mut app, KeyCode::Enter);
	let canonical = app.selected().unwrap();

	press(&mut app, KeyCode::Char('g'));
	let duplicate = app.selected().unwrap();
	assert_ne!(duplicate, canonical);
	assert_eq!(app.mode(), ViewMode::Details);

	press(&mut app, KeyCode::Char('g'));
	assert_eq!(app.selected(), Some(canonical));

	press(&mut app, KeyCode::Char('g'));
	assert_eq!(app.selected(), Some(duplicate), "the only duplicate comes round again");
}

#[test]
fn enter_on_a_folder_folds_it() {
	let mut app = app();
	press(&mut app, KeyCode::Home);
	let expanded = app.visible_rows();

	press(&mut app, KeyCode::Enter);
	assert_eq!(app.visible_rows(), 1);
	assert_eq!(app.mode(), ViewMode::Library);

	press(&mut app, KeyCode::Right);
	assert_eq!(app.visible_rows(), expanded);
}

#[test]
fn key_releases_are_ignored() {
	let mut app = app();

	release(&mut app, KeyCode::Char('z'));

	assert!(!app.maximized());
}

#[test]
fn q_and_ctrl_c_quit() {
	let mut app = app();
	press(&mut app, KeyCode::Char('q'));
	assert!(app.should_quit());

	let mut app = support::app();
	press(&mut app, KeyCode::Char('/'));
	press_with(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
	assert!(app.should_quit());
}

#[test]
fn f2_toggles_mouse_capture() {
	let mut app = app();
	assert!(app.mouse_capture());

	press(&mut app, KeyCode::F(2));

	assert!(!app.mouse_capture());
}

#[test]
fn p_and_e_reach_the_source() {
	let mut app = app();

	press(&mut app, KeyCode::Char('p'));
	press(&mut app, KeyCode::Char('e'));

	let header = &app.source().view().header;
	assert_eq!(header.run_state, RunState::Paused);
	assert_eq!(header.performance_limit, Some(Limit { cap: 25, pending: true }));
}

#[test]
fn clicking_the_maximize_toggle_and_double_clicking_the_title() {
	let mut app = app();
	let terminal = draw(&mut app, 120, 36);
	let title_row = find_row(&terminal, "─ Library ").unwrap();
	let toggle = find_on_row(&terminal, title_row, "[⤢]").unwrap();
	let now = Instant::now();

	click(&mut app, toggle + 1, title_row, now);
	assert!(app.maximized());

	let terminal = draw(&mut app, 120, 36);
	let title_row = find_row(&terminal, "─ Library ").unwrap();
	click(&mut app, 4, title_row, now + Duration::from_secs(1));
	assert!(app.maximized(), "a single click on the title does nothing");

	click(&mut app, 4, title_row, now + Duration::from_millis(1200));
	assert!(!app.maximized());
}

#[test]
fn right_click_selects_the_row_and_opens_the_menu() {
	let mut app = app();
	let terminal = draw(&mut app, 120, 36);
	let row = find_row(&terminal, "comicPage").unwrap();

	mouse(
		&mut app,
		MouseEventKind::Down(MouseButton::Right),
		20,
		row,
		Instant::now(),
	);

	assert_eq!(app.selected_name().as_deref(), Some("comic-veryTallImage.jpg"));
	assert_eq!(app.overlay(), Some(OverlayKind::Menu));

	draw(&mut app, 120, 36);
	click(&mut app, 110, 2, Instant::now());
	assert_eq!(app.overlay(), None, "a click outside the menu closes it");
}

#[test]
fn clicking_the_arrow_folds_and_double_click_opens_details() {
	let mut app = app();
	let terminal = draw(&mut app, 120, 36);
	let bled = find_row(&terminal, "▾ Bled").unwrap();
	let arrow = find_on_row(&terminal, bled, "▾ Bled").unwrap();
	let rows = app.visible_rows();

	click(&mut app, arrow, bled, Instant::now());
	assert!(app.visible_rows() < rows);

	let terminal = draw(&mut app, 120, 36);
	let file_row = find_row(&terminal, "● 2018 - HOLIDAY").unwrap();
	let now = Instant::now();
	click(&mut app, 30, file_row, now);
	assert_eq!(app.mode(), ViewMode::Library, "a single click only selects");

	click(&mut app, 30, file_row, now + Duration::from_millis(150));
	assert_eq!(app.mode(), ViewMode::Details);
	assert!(app.selected_name().unwrap().starts_with("2018 - HOLIDAY"));
}

#[test]
fn footer_chips_are_clickable() {
	let mut app = app();
	let terminal = draw(&mut app, 120, 36);
	let chip = find_on_row(&terminal, 35, " z ").unwrap();

	click(&mut app, chip + 1, 35, Instant::now());

	assert!(app.maximized());
}

#[test]
fn wheel_scrolls_the_library_without_moving_the_selection() {
	let mut app = app();
	let before = draw(&mut app, 120, 36);
	let first_row = support::row_text(&before, 12);
	let selected = app.selected();

	mouse(&mut app, MouseEventKind::ScrollDown, 40, 20, Instant::now());
	let after = draw(&mut app, 120, 36);

	assert_ne!(support::row_text(&after, 12), first_row);
	assert_eq!(app.selected(), selected);
}
