mod support;

use std::time::Instant;

use cryarchiver_tui::view::{Limit, Speed};
use cryarchiver_tui::{ArchiveSource, OverlayKind, ViewMode};
use ratatui::crossterm::event::{Event, KeyCode};

use support::{app, draw, find_row, press, type_text};

fn open_row(app: &mut cryarchiver_tui::App<cryarchiver_tui::MockSource>, row: usize) {
	press(app, KeyCode::Char('s'));
	for _ in 0..row {
		press(app, KeyCode::Down);
	}
}

#[test]
fn s_opens_settings_and_arrows_change_them() {
	let mut app = app();
	open_row(&mut app, 0);
	assert_eq!(app.overlay(), Some(OverlayKind::Settings));

	press(&mut app, KeyCode::Right);
	assert_eq!(app.source().view().settings.speed, Speed::Fast);

	press(&mut app, KeyCode::Down);
	press(&mut app, KeyCode::Left);
	assert_eq!(app.source().view().settings.min_savings, 5);

	press(&mut app, KeyCode::Down);
	press(&mut app, KeyCode::Char(' '));
	assert!(app.source().view().settings.watch);

	press(&mut app, KeyCode::Esc);
	assert_eq!(app.overlay(), None);
}

#[test]
fn deleting_originals_needs_delete_typed() {
	let mut app = app();
	open_row(&mut app, 6);
	press(&mut app, KeyCode::Enter);
	assert_eq!(app.overlay(), Some(OverlayKind::ConfirmDelete));

	type_text(&mut app, "quit");
	assert!(!app.should_quit(), "typing goes into the confirmation");
	press(&mut app, KeyCode::Enter);
	assert!(!app.source().view().header.deleting_originals);
	assert_eq!(app.overlay(), Some(OverlayKind::Settings));

	press(&mut app, KeyCode::Enter);
	app.handle_event(&Event::Paste("DELETE".to_owned()), Instant::now());
	press(&mut app, KeyCode::Enter);
	assert!(app.source().view().header.deleting_originals);

	press(&mut app, KeyCode::Enter);
	assert!(
		!app.source().view().header.deleting_originals,
		"turning it off needs no confirmation"
	);
}

#[test]
fn restarting_pending_jobs_asks_first() {
	let mut app = app();
	press(&mut app, KeyCode::Char('e'));
	assert!(app.source().view().limit().is_some_and(|limit| limit.pending));

	open_row(&mut app, 8);
	press(&mut app, KeyCode::Enter);
	assert_eq!(app.overlay(), Some(OverlayKind::ConfirmRestart));

	press(&mut app, KeyCode::Esc);
	assert!(app.source().view().limit().is_some_and(|limit| limit.pending));

	open_row(&mut app, 8);
	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Enter);
	assert_eq!(
		app.source().view().limit(),
		Some(Limit {
			cap: 25,
			pending: false
		})
	);
	assert_eq!(app.source().view().workers.jobs.len(), 1);
}

#[test]
fn limit_steps_through_caps() {
	let mut app = app();
	open_row(&mut app, 3);

	press(&mut app, KeyCode::Right);
	assert_eq!(app.source().view().settings.performance_limit, Some(25));
	press(&mut app, KeyCode::Right);
	assert_eq!(app.source().view().settings.performance_limit, Some(50));
	press(&mut app, KeyCode::Left);
	press(&mut app, KeyCode::Left);
	assert_eq!(app.source().view().settings.performance_limit, None);
}

#[test]
fn help_lists_the_settings_key() {
	let mut app = app();
	press(&mut app, KeyCode::Char('?'));

	let terminal = draw(&mut app, 120, 36);
	assert!(find_row(&terminal, "settings").is_some());
}

#[test]
fn issue_attempts_can_be_browsed() {
	let mut app = app();
	press(&mut app, KeyCode::Char('i'));
	press(&mut app, KeyCode::Down);
	press(&mut app, KeyCode::Down);
	assert_eq!(app.mode(), ViewMode::Issues);

	let terminal = draw(&mut app, 120, 36);
	assert!(find_row(&terminal, "attempt 3 of 3").is_some());

	press(&mut app, KeyCode::Left);
	press(&mut app, KeyCode::Left);
	let terminal = draw(&mut app, 120, 36);
	assert!(find_row(&terminal, "attempt 1 of 3").is_some());
	assert!(find_row(&terminal, "exit code 1 (attempt 3/3)").is_none());
}

#[test]
fn unavailable_verbs_do_nothing() {
	let mut app = app();
	press(&mut app, KeyCode::Char('i'));
	for _ in 0..3 {
		press(&mut app, KeyCode::Down);
	}
	let issues = app.source().view().issues.len();

	press(&mut app, KeyCode::Char('r'));
	press(&mut app, KeyCode::Char('x'));
	assert_eq!(
		app.source().view().issues.len(),
		issues,
		"an unknown file can only be acknowledged"
	);

	press(&mut app, KeyCode::Char('c'));
	assert_eq!(app.source().view().issues.len(), issues - 1);
}

#[test]
fn p_resumes_what_it_paused() {
	let mut app = app();
	press(&mut app, KeyCode::Char('p'));
	press(&mut app, KeyCode::Char('p'));

	assert_eq!(
		app.source().view().header.run_state,
		cryarchiver_tui::view::RunState::Running
	);
}
