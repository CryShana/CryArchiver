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
	type_text(&mut app, "battle-theme");
	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::End);
	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Char('?'));

	assert_eq!(app.overlay(), Some(OverlayKind::Help));
	assert_eq!(app.mode(), ViewMode::Details);
	assert_eq!(app.filter(), "battle-theme");

	press(&mut app, KeyCode::Esc);
	assert_eq!(app.overlay(), None);
	assert_eq!(app.mode(), ViewMode::Details);

	press(&mut app, KeyCode::Esc);
	assert_eq!(app.mode(), ViewMode::Library);
	assert_eq!(app.filter(), "battle-theme");

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
	type_text(&mut app, "さくら");
	assert_eq!(app.filter(), "さくら");
	assert_eq!(app.visible_rows(), 4, "the match plus Family, Music and Anime OST");

	press(&mut app, KeyCode::Enter);
	assert!(!app.searching());
	assert_eq!(app.filter(), "さくら");

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

fn open_battle_theme_original(app: &mut cryarchiver_tui::App<cryarchiver_tui::MockSource>) -> Selection {
	press(app, KeyCode::Char('/'));
	type_text(app, "battle-theme");
	press(app, KeyCode::Enter);
	press(app, KeyCode::End);
	press(app, KeyCode::Enter);
	app.selected().unwrap()
}

#[test]
fn g_cycles_through_the_duplicates_of_an_original() {
	let mut app = app();
	let original = open_battle_theme_original(&mut app);

	press(&mut app, KeyCode::Char('g'));
	let first = app.selected().unwrap();
	assert_ne!(first, original);
	assert_eq!(app.mode(), ViewMode::Details);

	press(&mut app, KeyCode::Char('g'));
	assert_eq!(app.selected(), Some(original), "a duplicate leads back to its original");

	press(&mut app, KeyCode::Char('g'));
	let second = app.selected().unwrap();
	assert_ne!(second, first);
	assert_ne!(second, original);

	press(&mut app, KeyCode::Char('g'));
	press(&mut app, KeyCode::Char('g'));
	assert_eq!(app.selected(), Some(first), "the cycle wraps around");
}

#[test]
fn g_starts_over_once_the_selection_moves_elsewhere() {
	let mut app = app();
	let original = open_battle_theme_original(&mut app);
	press(&mut app, KeyCode::Char('g'));
	let first = app.selected().unwrap();
	press(&mut app, KeyCode::Char('g'));

	press(&mut app, KeyCode::Up);
	press(&mut app, KeyCode::Down);
	assert_eq!(app.selected(), Some(original));

	press(&mut app, KeyCode::Char('g'));
	assert_eq!(app.selected(), Some(first));
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
	assert_eq!(app.source().view().limit(), Some(Limit { cap: 25, pending: true }));
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
	let row = find_row(&terminal, "comic-strip").unwrap();

	mouse(
		&mut app,
		MouseEventKind::Down(MouseButton::Right),
		20,
		row,
		Instant::now(),
	);

	assert_eq!(app.selected_name().as_deref(), Some("comic-strip-tall.jpg"));
	assert_eq!(app.overlay(), Some(OverlayKind::Menu));

	draw(&mut app, 120, 36);
	click(&mut app, 110, 2, Instant::now());
	assert_eq!(app.overlay(), None, "a click outside the menu closes it");
}

#[test]
fn clicking_the_arrow_folds_and_double_click_opens_details() {
	let mut app = app();
	let terminal = draw(&mut app, 120, 36);
	let dune = find_row(&terminal, "▾ Dune").unwrap();
	let arrow = find_on_row(&terminal, dune, "▾ Dune").unwrap();
	let rows = app.visible_rows();

	click(&mut app, arrow, dune, Instant::now());
	assert!(app.visible_rows() < rows);

	let terminal = draw(&mut app, 120, 36);
	let file_row = find_row(&terminal, "● 2018 - mountain").unwrap();
	let now = Instant::now();
	click(&mut app, 30, file_row, now);
	assert_eq!(app.mode(), ViewMode::Library, "a single click only selects");

	click(&mut app, 30, file_row, now + Duration::from_millis(150));
	assert_eq!(app.mode(), ViewMode::Details);
	assert!(app.selected_name().unwrap().starts_with("2018 - mountain"));
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

fn log_title_row(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> u16 {
	find_row(terminal, "─ Log ").unwrap()
}

#[test]
fn shift_l_maximizes_the_log_exclusively_and_esc_restores() {
	let mut app = app();

	press(&mut app, KeyCode::Char('L'));
	assert!(app.log_maximized());
	assert!(!app.maximized());

	press(&mut app, KeyCode::Char('z'));
	assert!(app.maximized());
	assert!(
		!app.log_maximized(),
		"the library and the log are never maximized together"
	);

	press(&mut app, KeyCode::Char('L'));
	assert!(app.log_maximized());
	assert!(!app.maximized());

	press(&mut app, KeyCode::Esc);
	assert!(!app.log_maximized());
	assert!(app.log_open());
}

#[test]
fn clicking_the_log_maximize_toggle() {
	let mut app = app();
	let terminal = draw(&mut app, 120, 36);
	let title_row = log_title_row(&terminal);
	let toggle = find_on_row(&terminal, title_row, "[⤢]").unwrap();

	click(&mut app, toggle + 1, title_row, Instant::now());
	assert!(app.log_maximized());
	assert!(app.log_open(), "the click maximizes, it doesn't collapse");

	let terminal = draw(&mut app, 120, 36);
	assert_eq!(
		log_title_row(&terminal),
		1,
		"the maximized log starts right below the header"
	);
	let toggle = find_on_row(&terminal, 1, "[⤡]").unwrap();
	click(&mut app, toggle + 1, 1, Instant::now() + Duration::from_secs(1));
	assert!(!app.log_maximized());
}

#[test]
fn wheel_over_the_log_scrolls_the_log_not_the_library() {
	let mut app = app();
	let before = draw(&mut app, 120, 36);
	let title_row = log_title_row(&before);
	let library_row = support::row_text(&before, 12);
	let second_line = support::row_text(&before, title_row + 2);

	mouse(&mut app, MouseEventKind::ScrollDown, 40, title_row + 2, Instant::now());
	let after = draw(&mut app, 120, 36);

	assert_eq!(support::row_text(&after, 12), library_row);
	assert!(support::row_text(&after, title_row).contains("↑ 3 newer"));
	assert_ne!(support::row_text(&after, title_row + 2), second_line);

	mouse(&mut app, MouseEventKind::ScrollUp, 40, title_row + 2, Instant::now());
	let back = draw(&mut app, 120, 36);
	assert_eq!(support::row_text(&back, title_row + 2), second_line);
	assert!(!support::row_text(&back, title_row).contains("newer"));
}

#[test]
fn a_scrolled_log_keeps_its_lines_while_new_ones_arrive() {
	let mut app = app();
	let terminal = draw(&mut app, 120, 36);
	let title_row = log_title_row(&terminal);
	mouse(&mut app, MouseEventKind::ScrollDown, 40, title_row + 1, Instant::now());
	let scrolled = draw(&mut app, 120, 36);
	let shown = support::row_text(&scrolled, title_row + 1);

	press(&mut app, KeyCode::Char('p'));
	let later = draw(&mut app, 120, 36);

	assert_eq!(support::row_text(&later, title_row + 1), shown);
	assert!(support::row_text(&later, title_row).contains("↑ 4 newer"));
}

#[test]
fn clicking_the_newer_hint_returns_to_the_newest_line() {
	let mut app = app();
	let terminal = draw(&mut app, 120, 36);
	let title_row = log_title_row(&terminal);
	let newest = support::row_text(&terminal, title_row + 1);
	mouse(&mut app, MouseEventKind::ScrollDown, 40, title_row + 1, Instant::now());
	let scrolled = draw(&mut app, 120, 36);
	let hint = find_on_row(&scrolled, title_row, "↑ 3 newer").unwrap();

	click(&mut app, hint + 1, title_row, Instant::now());
	let back = draw(&mut app, 120, 36);

	assert_eq!(support::row_text(&back, title_row + 1), newest);
	assert!(app.log_open(), "the hint doesn't collapse the log");
}

#[test]
fn keys_scroll_the_maximized_log_and_leave_the_selection_alone() {
	let mut app = app();
	for _ in 0..40 {
		press(&mut app, KeyCode::Char('p'));
	}
	let selected = app.selected();
	press(&mut app, KeyCode::Char('L'));
	draw(&mut app, 120, 36);

	press(&mut app, KeyCode::Down);
	press(&mut app, KeyCode::Down);
	let terminal = draw(&mut app, 120, 36);
	assert!(support::row_text(&terminal, 1).contains("↑ 2 newer"));
	assert_eq!(app.selected(), selected);

	press(&mut app, KeyCode::Home);
	let terminal = draw(&mut app, 120, 36);
	assert!(!support::row_text(&terminal, 1).contains("newer"));
}

#[test]
fn right_click_on_a_log_line_copies_it_and_opens_no_menu() {
	let mut app = app();
	let selected = app.selected();
	let terminal = draw(&mut app, 120, 36);
	let title_row = log_title_row(&terminal);
	let line = app.source().view().log.newest_first().nth(1).unwrap().clone();

	mouse(
		&mut app,
		MouseEventKind::Down(MouseButton::Right),
		60,
		title_row + 2,
		Instant::now(),
	);

	assert_eq!(app.overlay(), None);
	assert_eq!(app.selected(), selected);
	assert_eq!(app.take_clipboard(), Some(format!("{} {}", line.time, line.message)));
	assert!(app.toast().is_some_and(|toast| toast.text.starts_with("copied")));
}

#[test]
fn right_click_on_empty_log_space_or_its_border_does_nothing() {
	let mut app = app();
	press(&mut app, KeyCode::Char('L'));
	let terminal = draw(&mut app, 120, 36);
	let empty_row = (2..34)
		.find(|&row| {
			support::row_text(&terminal, row)
				.trim_matches(|c| c == '│' || c == ' ')
				.is_empty()
		})
		.unwrap();

	for (x, y) in [(60, empty_row), (60, 34), (0, 10), (10, 1)] {
		mouse(&mut app, MouseEventKind::Down(MouseButton::Right), x, y, Instant::now());
		assert_eq!(app.overlay(), None, "right-click at {x},{y}");
		assert_eq!(app.take_clipboard(), None);
	}
}

#[test]
fn right_click_on_the_normal_log_outside_its_lines_does_nothing() {
	let mut app = app();
	let selected = app.selected();
	let terminal = draw(&mut app, 120, 36);
	let title_row = log_title_row(&terminal);
	let toggle = find_on_row(&terminal, title_row, "[⤢]").unwrap();

	for (x, y) in [
		(0, title_row + 1),
		(3, title_row),
		(toggle + 1, title_row),
		(60, title_row + 5),
	] {
		mouse(&mut app, MouseEventKind::Down(MouseButton::Right), x, y, Instant::now());
		assert_eq!(app.overlay(), None, "right-click at {x},{y}");
		assert_eq!(app.take_clipboard(), None);
		assert_eq!(app.selected(), selected);
		draw(&mut app, 120, 36);
	}
}

#[test]
fn wheel_outside_the_maximized_log_leaves_the_hidden_library_alone() {
	let mut app = app();
	let before = support::row_text(&draw(&mut app, 120, 36), 12);
	press(&mut app, KeyCode::Char('L'));
	draw(&mut app, 120, 36);

	mouse(&mut app, MouseEventKind::ScrollDown, 40, 0, Instant::now());
	mouse(&mut app, MouseEventKind::ScrollDown, 40, 35, Instant::now());
	press(&mut app, KeyCode::Char('L'));

	assert_eq!(support::row_text(&draw(&mut app, 120, 36), 12), before);
}

#[test]
fn i_from_the_maximized_log_always_shows_the_issues() {
	let mut app = app();
	press(&mut app, KeyCode::Char('i'));
	press(&mut app, KeyCode::Char('L'));

	press(&mut app, KeyCode::Char('i'));

	assert!(!app.log_maximized());
	assert_eq!(app.mode(), ViewMode::Issues);
}

#[test]
fn the_copy_confirmation_shows_over_a_recent_warning() {
	let mut app = app();
	let terminal = draw(&mut app, 120, 36);
	let title_row = log_title_row(&terminal);
	app.source_mut().perform(cryarchiver_tui::Action::ChangeSettings(
		cryarchiver_tui::source::SettingsChange::DeleteOriginals {
			enabled: true,
			confirmation: "nope".to_owned(),
		},
	));
	app.refresh();
	assert_eq!(
		app.toast().map(|toast| toast.level),
		Some(cryarchiver_tui::view::NoticeLevel::Error)
	);

	mouse(
		&mut app,
		MouseEventKind::Down(MouseButton::Right),
		60,
		title_row + 1,
		Instant::now(),
	);

	assert_eq!(app.toast().map(|toast| toast.text.as_str()), Some("copied log line"));
}
