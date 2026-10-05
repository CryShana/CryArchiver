mod support;

use std::time::{Duration, Instant};

use cryarchiver_tui::view::{
	ArchiveView, FileDetails, FileId, FileRow, FileState, FilterEntry, FilterResult, Issue, IssueId, IssueKind,
	IssueVerbs, Limit, NodeId, Notification, RunState, StateFilter,
};
use cryarchiver_tui::{Action, App, ArchiveSource, ColorDepth, MockSource, OverlayKind, Selection, ViewMode};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

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
	press(&mut app, KeyCode::Enter);
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
	let first_row = support::row_text(&before, 10);
	let selected = app.selected();

	mouse(&mut app, MouseEventKind::ScrollDown, 40, 20, Instant::now());
	let after = draw(&mut app, 120, 36);

	assert_ne!(support::row_text(&after, 10), first_row);
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
	let library_row = support::row_text(&before, 10);
	let second_line = support::row_text(&before, title_row + 2);

	mouse(&mut app, MouseEventKind::ScrollDown, 40, title_row + 2, Instant::now());
	let after = draw(&mut app, 120, 36);

	assert_eq!(support::row_text(&after, 10), library_row);
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
	let before = support::row_text(&draw(&mut app, 120, 36), 10);
	press(&mut app, KeyCode::Char('L'));
	draw(&mut app, 120, 36);

	mouse(&mut app, MouseEventKind::ScrollDown, 40, 0, Instant::now());
	mouse(&mut app, MouseEventKind::ScrollDown, 40, 35, Instant::now());
	press(&mut app, KeyCode::Char('L'));

	assert_eq!(support::row_text(&draw(&mut app, 120, 36), 10), before);
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

	let copied = format!("copied {}", app.take_clipboard().unwrap());
	assert_eq!(app.toast().map(|toast| toast.text.as_str()), Some(copied.as_str()));
}

#[test]
fn q_while_archiving_asks_first() {
	let mut app = app();

	for stay in [KeyCode::Esc, KeyCode::Char('n'), KeyCode::Char('q')] {
		press(&mut app, KeyCode::Char('q'));
		assert_eq!(app.overlay(), Some(OverlayKind::ConfirmQuit));
		assert!(!app.should_quit());

		press(&mut app, stay);
		assert_eq!(app.overlay(), None, "{stay:?} stays");
		assert!(!app.should_quit());
	}

	press(&mut app, KeyCode::Char('q'));
	press(&mut app, KeyCode::Char('y'));
	assert!(app.should_quit());
}

#[test]
fn q_asks_from_the_menu_and_the_settings_too() {
	for open in ['m', 's'] {
		let mut app = app();
		press(&mut app, KeyCode::Char(open));

		press(&mut app, KeyCode::Char('q'));

		assert_eq!(app.overlay(), Some(OverlayKind::ConfirmQuit));
		assert!(!app.should_quit());
	}
}

#[test]
fn staying_returns_to_the_overlay_q_was_pressed_in() {
	for (open, kind) in [
		('m', OverlayKind::Menu),
		('s', OverlayKind::Settings),
		('?', OverlayKind::Help),
	] {
		let mut app = app();
		press(&mut app, KeyCode::Char(open));
		press(&mut app, KeyCode::Char('q'));

		press(&mut app, KeyCode::Esc);

		assert_eq!(app.overlay(), Some(kind));
	}
}

#[test]
fn q_asks_from_every_view() {
	for keys in [&[KeyCode::Enter][..], &[KeyCode::Char('i')], &[KeyCode::Char('L')]] {
		let mut app = app();
		for &key in keys {
			press(&mut app, key);
		}

		press(&mut app, KeyCode::Char('q'));

		assert_eq!(app.overlay(), Some(OverlayKind::ConfirmQuit), "after {keys:?}");
		assert!(!app.should_quit());
	}
}

#[test]
fn the_footer_quit_chip_asks_first() {
	let mut app = app();
	let terminal = draw(&mut app, 160, 45);
	let chip = find_on_row(&terminal, 44, " q ").unwrap();

	click(&mut app, chip + 1, 44, Instant::now());

	assert_eq!(app.overlay(), Some(OverlayKind::ConfirmQuit));
	assert!(!app.should_quit());
}

#[test]
fn q_quits_at_once_when_nothing_runs() {
	let mut app = App::new(MockSource::completed(support::SEED), ColorDepth::TrueColor);

	press(&mut app, KeyCode::Char('q'));

	assert!(app.should_quit());
}

#[test]
fn ctrl_c_quits_from_the_quit_question() {
	let mut app = app();
	press(&mut app, KeyCode::Char('q'));

	press_with(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);

	assert!(app.should_quit());
}

#[test]
fn the_quit_question_buttons_are_clickable() {
	let mut app = app();
	press(&mut app, KeyCode::Char('q'));
	let terminal = draw(&mut app, 120, 36);
	let row = find_row(&terminal, "Work in progress stops").unwrap() + 2;
	let stay = find_on_row(&terminal, row, " Esc ").unwrap();

	click(&mut app, stay + 1, row, Instant::now());
	assert_eq!(app.overlay(), None);
	assert!(!app.should_quit());

	press(&mut app, KeyCode::Char('q'));
	let terminal = draw(&mut app, 120, 36);
	let quit = find_on_row(&terminal, row, " ⏎ ").unwrap();
	click(&mut app, quit + 1, row, Instant::now() + Duration::from_secs(1));
	assert!(app.should_quit());
}

fn library_names(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, header: u16) -> Vec<String> {
	(header + 1..header + 15)
		.map(|row| {
			support::row_text(terminal, row)
				.chars()
				.take(50)
				.collect::<String>()
				.trim_end()
				.to_owned()
		})
		.collect()
}

#[test]
fn clicking_progress_cycles_the_sort_and_off_restores_the_order() {
	let mut app = app();
	press(&mut app, KeyCode::Home);
	let terminal = draw(&mut app, 120, 36);
	let header = find_row(&terminal, "Status").unwrap();
	let progress = find_on_row(&terminal, header, "Progress").unwrap();
	let original = library_names(&terminal, header);
	let now = Instant::now();

	click(&mut app, progress + 2, header, now);
	let terminal = draw(&mut app, 120, 36);
	assert!(support::row_text(&terminal, header).contains("Progress ▼"));
	let descending = library_names(&terminal, header);
	assert_ne!(descending, original);

	click(&mut app, progress + 2, header, now + Duration::from_secs(1));
	let terminal = draw(&mut app, 120, 36);
	assert!(support::row_text(&terminal, header).contains("Progress ▲"));
	assert_ne!(library_names(&terminal, header), descending);

	click(&mut app, progress + 2, header, now + Duration::from_secs(2));
	let terminal = draw(&mut app, 120, 36);
	let row = support::row_text(&terminal, header);
	assert!(row.contains("Progress") && !row.contains('▼') && !row.contains('▲'));
	assert_eq!(library_names(&terminal, header), original);
}

/// The Library body's screen rows, between the column header and the panel's bottom border.
fn library_body(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, header: u16) -> Vec<u16> {
	let height = terminal.backend().buffer().area.height;
	(header + 1..height)
		.take_while(|&row| !support::row_text(terminal, row).starts_with('╰'))
		.collect()
}

fn library_row_of(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, header: u16, name: &str) -> Option<u16> {
	library_body(terminal, header)
		.into_iter()
		.find(|&row| support::row_text(terminal, row).contains(name))
}

#[test]
fn the_selection_follows_its_row_when_the_sort_changes() {
	let mut app = app();
	let selected = app.selected();
	let name = app.selected_name().unwrap();
	let terminal = draw(&mut app, 120, 36);
	let header = find_row(&terminal, "Status").unwrap();
	let progress = find_on_row(&terminal, header, "Progress").unwrap();
	let unsorted = library_row_of(&terminal, header, &name).unwrap();
	let now = Instant::now();

	click(&mut app, progress + 2, header, now);
	let terminal = draw(&mut app, 120, 36);
	assert_eq!(app.selected(), selected);
	let sorted = library_row_of(&terminal, header, &name).expect("the selected row stays on screen");
	assert_ne!(
		sorted, unsorted,
		"the selected row moved, and the selection went with it"
	);

	for step in 1..3 {
		click(&mut app, progress + 2, header, now + Duration::from_secs(step));
		let terminal = draw(&mut app, 120, 36);
		assert_eq!(app.selected(), selected);
		assert!(library_row_of(&terminal, header, &name).is_some());
	}
}

/// The demo with its view frozen for editing, and files finished on demand, patched into their folder's rows the way
/// a source patches statuses.
struct Patched {
	inner: MockSource,
	view: ArchiveView,
	rows: std::collections::HashMap<NodeId, Vec<FileRow>>,
	changed: bool,
}

impl Patched {
	fn new() -> Self {
		let inner = support::source();
		Self {
			view: inner.view().clone(),
			inner,
			rows: std::collections::HashMap::new(),
			changed: false,
		}
	}

	fn finish(&mut self, file: FileId, folder: NodeId) {
		let mut rows = self.inner.files(folder).unwrap().to_vec();
		let row = rows.iter_mut().find(|row| row.id == file).unwrap();
		row.status.state = FileState::Done;
		row.status.progress = None;
		self.rows.insert(folder, rows);
		self.changed = true;
	}
}

impl ArchiveSource for Patched {
	fn refresh(&mut self) -> bool {
		std::mem::take(&mut self.changed)
	}

	fn view(&self) -> &ArchiveView {
		&self.view
	}

	fn files(&self, folder: NodeId) -> Option<&[FileRow]> {
		self.rows
			.get(&folder)
			.map(Vec::as_slice)
			.or_else(|| self.inner.files(folder))
	}

	fn rows_version(&self) -> u64 {
		self.inner.rows_version()
	}

	fn filter(&self) -> Option<&FilterResult> {
		self.inner.filter()
	}

	fn details(&self, file: FileId) -> Option<FileDetails> {
		self.inner.details(file)
	}

	fn issue_log(&self, issue: IssueId, attempt: u32) -> Option<Vec<String>> {
		self.inner.issue_log(issue, attempt)
	}

	fn take_notifications(&mut self) -> Vec<Notification> {
		self.inner.take_notifications()
	}

	fn perform(&mut self, action: Action) {
		self.inner.perform(action);
	}

	fn shutdown(&mut self) {
		self.inner.shutdown();
	}
}

fn key_to(app: &mut App<Patched>, code: KeyCode, times: usize) {
	for _ in 0..times {
		app.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), Instant::now());
	}
}

fn draw_patched(app: &mut App<Patched>) -> ratatui::Terminal<ratatui::backend::TestBackend> {
	let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
	terminal.draw(|frame| app.draw(frame)).unwrap();
	terminal
}

#[test]
fn a_live_re_sort_keeps_the_selected_row_where_it_is_on_screen() {
	let mut app = App::new(Patched::new(), ColorDepth::TrueColor);
	let terminal = draw_patched(&mut app);
	let header = find_row(&terminal, "Status").unwrap();
	let progress = find_on_row(&terminal, header, "Progress").unwrap();
	let down = MouseEvent {
		kind: MouseEventKind::Down(MouseButton::Left),
		column: progress + 2,
		row: header,
		modifiers: KeyModifiers::NONE,
	};
	app.handle_event(&Event::Mouse(down), Instant::now());

	draw_patched(&mut app);
	key_to(&mut app, KeyCode::End, 1);
	let terminal = draw_patched(&mut app);
	let page = library_body(&terminal, header).len();
	key_to(&mut app, KeyCode::Up, page - 1);
	let terminal = draw_patched(&mut app);
	let top = app.selected_name().unwrap();
	assert_eq!(
		library_row_of(&terminal, header, &top),
		Some(header + 1),
		"the top row is selected"
	);
	let Some(Selection::File { file, folder }) = app.selected() else {
		panic!("the last page starts with a file without a gauge");
	};

	key_to(&mut app, KeyCode::Down, 5);
	let terminal = draw_patched(&mut app);
	let name = app.selected_name().unwrap();
	let row = library_row_of(&terminal, header, &name).unwrap();

	app.source_mut().finish(file, folder);
	assert!(app.refresh());
	let terminal = draw_patched(&mut app);

	assert_ne!(
		library_row_of(&terminal, header, &top),
		Some(header + 1),
		"the finished file moved up"
	);
	assert_eq!(app.selected_name().as_deref(), Some(name.as_str()));
	assert_eq!(library_row_of(&terminal, header, &name), Some(row));
}

#[test]
fn an_issue_without_a_file_offers_no_folder_retry() {
	let mut source = Patched::new();
	let root = source.view.library.root();
	source.view.issues = vec![Issue {
		id: IssueId(1),
		kind: IssueKind::SourceUnreadable,
		file: None,
		folder: Some(root),
		name: "Locked".to_owned(),
		location: "Locked".to_owned(),
		reason: "Permission denied".to_owned(),
		resolution: String::new(),
		verbs: IssueVerbs {
			retry: Some("Retry".to_owned()),
			accept: None,
			exclude: true,
		},
		attempts: 0,
	}];
	let mut app = App::new(source, ColorDepth::TrueColor);
	key_to(&mut app, KeyCode::Home, 1);

	key_to(&mut app, KeyCode::Char('m'), 1);

	let items = app.menu_items();
	assert!(items.iter().any(|item| item == "Copy path"));
	assert!(!items.iter().any(|item| item == "Retry failed in folder"));
}

fn has(app: &App<MockSource>, item: &str) -> bool {
	app.menu_items().iter().any(|label| label == item)
}

#[test]
fn the_file_menu_fits_the_view_it_opened_in() {
	let mut app = app();
	press(&mut app, KeyCode::Char('m'));
	assert!(has(&app, "Show details"));
	assert!(
		!has(&app, "Show ffmpeg log"),
		"a file without an issue has no log to show"
	);
	press(&mut app, KeyCode::Esc);

	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Char('m'));
	assert!(!has(&app, "Show details"), "its details are already open");
	press(&mut app, KeyCode::Esc);
	press(&mut app, KeyCode::Esc);

	press(&mut app, KeyCode::Char('i'));
	press(&mut app, KeyCode::Down);
	press(&mut app, KeyCode::Char('m'));
	assert!(has(&app, "Show details"));
	assert!(!has(&app, "Show ffmpeg log"), "the issues view already shows it");
	press(&mut app, KeyCode::Esc);

	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Esc);
	assert_eq!(app.mode(), ViewMode::Library);
	press(&mut app, KeyCode::Char('m'));
	assert!(has(&app, "Show ffmpeg log"));
	assert!(has(&app, "Show details"));
}

#[test]
fn the_details_menu_of_an_original_leaves_out_its_duplicates() {
	let mut app = app();
	open_battle_theme_original(&mut app);
	assert_eq!(app.mode(), ViewMode::Details);
	press(&mut app, KeyCode::Char('m'));
	assert!(!has(&app, "Show duplicates"), "the details already list them");
	press(&mut app, KeyCode::Esc);

	press(&mut app, KeyCode::Esc);
	assert_eq!(app.mode(), ViewMode::Library);
	press(&mut app, KeyCode::Char('m'));
	assert!(has(&app, "Show duplicates"));
}

#[test]
fn the_folder_menu_leaves_out_what_would_do_nothing() {
	let mut app = app();
	press(&mut app, KeyCode::Home);
	press(&mut app, KeyCode::Char('m'));
	assert!(has(&app, "Expand all") && has(&app, "Collapse all"));
	assert!(has(&app, "Retry failed in folder"));

	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Char('m'));
	assert!(!has(&app, "Expand all"), "everything below is open");
	assert_eq!(app.menu_items()[0], "Collapse all");

	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Char('m'));
	assert!(has(&app, "Expand all"));
	assert!(!has(&app, "Collapse all"), "everything below is closed");
	press(&mut app, KeyCode::Esc);

	press(&mut app, KeyCode::Char('/'));
	type_text(&mut app, "a");
	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Home);
	press(&mut app, KeyCode::Char('m'));
	assert!(
		!has(&app, "Expand all") && !has(&app, "Collapse all"),
		"a filter shows every match open"
	);
}

#[test]
fn a_folder_without_failed_files_offers_no_retry() {
	let mut app = app();
	press(&mut app, KeyCode::Home);
	press(&mut app, KeyCode::Char('m'));
	press(&mut app, KeyCode::Enter);

	let calm = (0..app.visible_rows()).find_map(|_| {
		press(&mut app, KeyCode::Down);
		match app.selected() {
			Some(Selection::Folder(folder)) if app.source().view().library.stats(folder).open_issues == 0 => {
				Some(folder)
			}
			_ => None,
		}
	});
	assert!(calm.is_some(), "the demo has a folder without issues");

	press(&mut app, KeyCode::Char('m'));
	assert!(has(&app, "Copy path"));
	assert!(!has(&app, "Retry failed in folder"));
}

/// Where the issues, kept and dup groups of the Overall panel are: their row and the column of each glyph. The count
/// starts two cells after the glyph.
fn outcome_groups(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> (u16, [u16; 3]) {
	let row = find_row(terminal, " kept ").unwrap();
	let at = |glyph| find_on_row(terminal, row, glyph).unwrap();
	(row, [at("! "), at("= "), at("↪ ")])
}

fn shown_files(app: &App<MockSource>) -> Vec<FileState> {
	let result = app.source().filter().unwrap();
	result
		.rows
		.iter()
		.filter_map(|row| match &row.entry {
			FilterEntry::File { row, .. } => Some(row.status.state),
			FilterEntry::Folder(_) => None,
		})
		.collect()
}

#[test]
fn clicking_an_outcome_filters_the_library_and_clicking_it_again_clears_it() {
	let mut app = app();
	let before = draw(&mut app, 160, 45);
	let (row, [issues, kept, dup]) = outcome_groups(&before);
	let line = support::row_text(&before, row);
	let plain = before.backend().buffer()[(kept + 2, row)].bg;

	click(&mut app, kept + 2, row, Instant::now());
	assert_eq!(app.state_filter(), Some(StateFilter::Kept));
	let states = shown_files(&app);
	assert_eq!(states.len() as u64, app.source().view().overall.kept);
	assert!(states.iter().all(|&state| state == FileState::KeptOriginal));

	let after = draw(&mut app, 160, 45);
	let buffer = after.backend().buffer();
	assert_eq!(support::row_text(&after, row), line, "the line keeps its layout");
	assert_ne!(buffer[(kept + 2, row)].bg, plain, "the active group is highlighted");
	assert_eq!(buffer[(kept, row)].bg, buffer[(kept + 2, row)].bg, "its glyph too");
	assert_eq!(buffer[(issues + 2, row)].bg, plain);

	click(&mut app, dup + 2, row, Instant::now());
	assert_eq!(app.state_filter(), Some(StateFilter::Duplicates));
	assert!(shown_files(&app).iter().all(|&state| state == FileState::Duplicate));

	click(&mut app, issues, row, Instant::now());
	assert_eq!(
		app.state_filter(),
		Some(StateFilter::Issues),
		"the glyph is part of the group"
	);
	let with_issues = app
		.source()
		.view()
		.issues
		.iter()
		.filter(|issue| issue.file.is_some())
		.count();
	assert_eq!(shown_files(&app).len(), with_issues);

	draw(&mut app, 160, 45);
	click(&mut app, issues + 2, row, Instant::now());
	assert_eq!(app.state_filter(), None);
	assert!(app.source().filter().is_none());
	let cleared = draw(&mut app, 160, 45);
	assert_eq!(cleared.backend().buffer()[(issues + 2, row)].bg, plain);
}

#[test]
fn clicking_an_outcome_from_another_view_shows_the_library() {
	let mut app = app();
	press(&mut app, KeyCode::Char('i'));
	let terminal = draw(&mut app, 160, 45);
	let (row, [_, kept, _]) = outcome_groups(&terminal);

	click(&mut app, kept, row, Instant::now());

	assert_eq!(app.mode(), ViewMode::Library);
	assert_eq!(app.state_filter(), Some(StateFilter::Kept));
}

#[test]
fn esc_clears_the_state_filter_before_the_name_filter() {
	let mut app = app();
	press(&mut app, KeyCode::Char('/'));
	type_text(&mut app, "e");
	press(&mut app, KeyCode::Enter);
	let terminal = draw(&mut app, 160, 45);
	let (row, [_, kept, _]) = outcome_groups(&terminal);
	click(&mut app, kept, row, Instant::now());
	assert_eq!((app.filter(), app.state_filter()), ("e", Some(StateFilter::Kept)));
	let result = app.source().filter().unwrap();
	assert_eq!((result.query.as_str(), result.state), ("e", Some(StateFilter::Kept)));

	press(&mut app, KeyCode::Esc);
	assert_eq!((app.filter(), app.state_filter()), ("e", None));

	press(&mut app, KeyCode::Esc);
	assert_eq!((app.filter(), app.state_filter()), ("", None));
}

#[test]
fn searching_keeps_the_state_filter_and_its_esc_clears_only_the_query() {
	let mut app = app();
	let terminal = draw(&mut app, 160, 45);
	let (row, [_, kept, _]) = outcome_groups(&terminal);
	click(&mut app, kept, row, Instant::now());
	let all_kept = app.visible_rows();

	press(&mut app, KeyCode::Char('/'));
	type_text(&mut app, "short-film");
	assert_eq!(app.state_filter(), Some(StateFilter::Kept));
	assert!(app.visible_rows() < all_kept);

	press(&mut app, KeyCode::Esc);
	assert_eq!((app.filter(), app.state_filter()), ("", Some(StateFilter::Kept)));
	assert_eq!(app.visible_rows(), all_kept);
}

#[test]
fn the_footer_offers_to_clear_a_state_filter() {
	let mut app = app();
	let terminal = draw(&mut app, 160, 45);
	assert!(find_on_row(&terminal, 44, "clear filter").is_none());
	let (row, [_, kept, _]) = outcome_groups(&terminal);
	click(&mut app, kept, row, Instant::now());

	let terminal = draw(&mut app, 160, 45);
	let chip = find_on_row(&terminal, 44, " Esc ").unwrap();
	assert!(find_on_row(&terminal, 44, "clear filter").is_some());
	click(&mut app, chip + 1, 44, Instant::now());

	assert_eq!(app.state_filter(), None);
}

#[test]
fn a_state_filter_without_files_says_so() {
	let mut app = App::new(MockSource::completed(support::SEED), ColorDepth::TrueColor);
	let terminal = draw(&mut app, 160, 45);
	let (row, [issues, _, _]) = outcome_groups(&terminal);

	click(&mut app, issues, row, Instant::now());

	let terminal = draw(&mut app, 160, 45);
	assert!(find_row(&terminal, "Library · issues · 0 files").is_some());
	assert!(find_row(&terminal, "No files with an open issue").is_some());
}

#[test]
fn every_outcome_group_is_clickable_at_100_and_120_columns() {
	for (width, height) in [(100, 30), (120, 36)] {
		let mut app = app();
		let terminal = draw(&mut app, width, height);
		let row = find_row(&terminal, "= 520").unwrap();
		let at = |glyph| find_on_row(&terminal, row, glyph).unwrap();

		for (glyph, state) in [
			("! ", StateFilter::Issues),
			("= ", StateFilter::Kept),
			("↪ ", StateFilter::Duplicates),
		] {
			click(&mut app, at(glyph) + 2, row, Instant::now());
			assert_eq!(app.state_filter(), Some(state), "{glyph} at {width} columns");
		}
	}
}
