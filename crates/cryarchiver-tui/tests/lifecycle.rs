mod support;

use std::time::Instant;

use cryarchiver_tui::view::RunState;
use cryarchiver_tui::{ArchiveSource, OverlayKind, ViewMode};
use ratatui::crossterm::event::{KeyCode, MouseEventKind};

use support::{app, draw, mouse, press, row_text};

#[test]
fn header_keeps_the_run_state_at_every_size() {
	for (width, height) in [(100, 30), (120, 36), (160, 45)] {
		for maximized in [false, true] {
			let mut app = app();
			if maximized {
				press(&mut app, KeyCode::Char('z'));
			}

			let header = row_text(&draw(&mut app, width, height), 0);
			assert!(
				header.contains("▶ running"),
				"{width}×{height} maximized={maximized}: {header}"
			);
			assert!(header.contains(" DEMO "), "{width}×{height}: {header}");
		}
	}
}

#[test]
fn header_adds_the_eta_then_the_saving_then_the_free_space_as_room_grows() {
	let mut limited = app();
	press(&mut limited, KeyCode::Char('e'));
	let crowded = row_text(&draw(&mut limited, 100, 30), 0);
	let narrow = row_text(&draw(&mut app(), 100, 30), 0);
	let wide = row_text(&draw(&mut app(), 160, 45), 0);

	assert!(crowded.contains("applying…") && crowded.contains("▶ running") && !crowded.contains("ETA"));
	assert!(narrow.contains("ETA") && narrow.contains("saved") && !narrow.contains("free"));
	assert!(wide.contains("ETA") && wide.contains("saved") && wide.contains("free"));
}

#[test]
fn paused_run_state_shows_in_the_header() {
	let mut app = app();
	press(&mut app, KeyCode::Char('p'));

	assert!(row_text(&draw(&mut app, 100, 30), 0).contains("‖ paused"));
}

#[test]
fn quitting_draws_a_stopping_frame_then_shuts_the_source_down() {
	let mut app = app();
	press(&mut app, KeyCode::Char('q'));
	press(&mut app, KeyCode::Enter);
	assert!(app.should_quit());

	app.begin_shutdown();
	let terminal = draw(&mut app, 120, 36);
	assert!(support::find_row(&terminal, "Stopping… running jobs are requeued").is_some());

	app.shutdown();
	assert_eq!(app.source().view().header.run_state, RunState::Stopping);
	assert!(app.source().view().workers.jobs.is_empty());
}

#[test]
fn q_quits_even_behind_help() {
	let mut app = app();
	press(&mut app, KeyCode::Char('?'));
	assert_eq!(app.overlay(), Some(OverlayKind::Help));

	press(&mut app, KeyCode::Char('q'));
	assert_eq!(app.overlay(), Some(OverlayKind::ConfirmQuit));
	press(&mut app, KeyCode::Enter);

	assert!(app.should_quit());
}

#[test]
fn refused_mouse_capture_leaves_the_app_keyboard_only() {
	let mut app = app();

	app.set_mouse_capture(false);

	assert!(!app.mouse_capture());
	press(&mut app, KeyCode::F(2));
	assert!(app.mouse_capture(), "F2 can still ask again");
}

#[test]
fn spinner_is_only_animated_while_it_is_on_screen() {
	let mut app = app();
	draw(&mut app, 160, 45);
	assert!(app.spinner_visible());

	press(&mut app, KeyCode::Char('z'));
	draw(&mut app, 160, 45);
	assert!(!app.spinner_visible());

	press(&mut app, KeyCode::Char('z'));
	draw(&mut app, 100, 30);
	assert!(!app.spinner_visible(), "the headline has no room for it at 100 columns");
}

#[test]
fn wheel_in_the_issues_list_stays_on_the_last_issue() {
	let mut app = app();
	press(&mut app, KeyCode::Char('i'));
	draw(&mut app, 120, 36);

	for _ in 0..20 {
		mouse(&mut app, MouseEventKind::ScrollDown, 10, 13, Instant::now());
	}

	assert_eq!(app.mode(), ViewMode::Issues);
	assert_eq!(app.selected_issue(), app.source().view().issues.len() - 1);
}

#[test]
fn issues_survive_first_on_a_narrow_overall_panel() {
	let mut app = app();
	let issues = app.source().view().issues.len();
	let terminal = draw(&mut app, 100, 30);

	assert!(
		support::find_row(&terminal, &format!("! {issues}  = ")).is_some(),
		"the counts stay, without their words"
	);
}
