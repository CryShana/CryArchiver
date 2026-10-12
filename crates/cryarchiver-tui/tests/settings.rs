mod support;

use std::time::Instant;

use cryarchiver_engine::options::BluRay;
use cryarchiver_engine::preset::{Overrides, VmafOverride};
use cryarchiver_tui::view::{Limit, Speed};
use cryarchiver_tui::{App, ArchiveSource, MockSource, OverlayKind, ViewMode};
use ratatui::crossterm::event::{Event, KeyCode};
use ratatui::style::Modifier;

use support::{app, click, draw, find_on_row, find_row, press, row_text, type_text};

const SPEED: usize = 0;
const VIDEO: usize = 1;
const IMAGE: usize = 2;
const AUDIO: usize = 3;
const RESET_QUALITY: usize = 4;
const MIN_SAVINGS: usize = 5;
const LIMIT: usize = 7;
const KEEP_AUXILIARY_IMAGES: usize = 9;
const BLU_RAY: usize = 10;
const DELETE_ORIGINALS: usize = 11;
const RESTART_PENDING_JOBS: usize = 13;

fn open_row(app: &mut App<MockSource>, row: usize) {
	press(app, KeyCode::Char('s'));
	for _ in 0..row {
		press(app, KeyCode::Down);
	}
}

/// Types `text` into the quality row's editor and presses ⏎; the settings stay open.
fn enter_quality(app: &mut App<MockSource>, row: usize, text: &str) {
	open_row(app, row);
	press(app, KeyCode::Enter);
	assert_eq!(app.overlay(), Some(OverlayKind::EditQuality));
	type_text(app, text);
	press(app, KeyCode::Enter);
}

fn shown_row(app: &mut App<MockSource>, label: &str) -> String {
	let terminal = draw(app, 120, 36);
	let row = find_row(&terminal, label).unwrap_or_else(|| panic!("no row shows {label}"));
	row_text(&terminal, row)
}

fn overrides(app: &App<MockSource>) -> Overrides {
	app.source().view().settings.overrides
}

#[test]
fn quality_rows_show_the_preset_dimmed_until_edited() {
	let mut app = app();
	open_row(&mut app, IMAGE);

	assert!(shown_row(&mut app, "Video VMAF").contains("preset · 91/86"));
	assert!(shown_row(&mut app, "Audio stereo").contains("preset · 128 kbps"));
	let terminal = draw(&mut app, 120, 36);
	let row = find_row(&terminal, "Image SSIMULACRA2").unwrap();
	assert!(row_text(&terminal, row).contains("preset · 80"));
	let preset_cell = &terminal.backend().buffer()[(find_on_row(&terminal, row, "preset").unwrap(), row)];
	assert!(
		!preset_cell.modifier.contains(Modifier::BOLD),
		"the preset value is drawn dim, not bold"
	);

	press(&mut app, KeyCode::Esc);
	enter_quality(&mut app, IMAGE, "80");

	assert_eq!(overrides(&app).image, Some(80.0));
	assert_eq!(app.overlay(), Some(OverlayKind::Settings));
	let terminal = draw(&mut app, 120, 36);
	let row = find_row(&terminal, "Image SSIMULACRA2").unwrap();
	let text = row_text(&terminal, row);
	assert!(text.contains("custom · 80"), "{text}");
	assert!(!text.contains("preset"), "{text}");
	let custom_cell = &terminal.backend().buffer()[(find_on_row(&terminal, row, "custom").unwrap(), row)];
	assert_ne!(preset_cell.fg, custom_cell.fg);
	assert!(
		custom_cell.modifier.contains(Modifier::BOLD),
		"a custom value is drawn bold, unlike the dimmed preset"
	);
	assert!(shown_row(&mut app, "Video VMAF").contains("preset · 91/86"));
}

#[test]
fn quality_edits_are_validated_as_they_are_typed_and_invalid_ones_are_not_applied() {
	let mut app = app();

	enter_quality(&mut app, IMAGE, "120");
	assert_eq!(app.overlay(), Some(OverlayKind::EditQuality));
	assert!(find_row(&draw(&mut app, 120, 36), "must be between 50 and 95").is_some());
	assert!(!app.should_quit());
	press(&mut app, KeyCode::Esc);
	assert_eq!(overrides(&app), Overrides::default());
	press(&mut app, KeyCode::Esc);

	enter_quality(&mut app, VIDEO, "93");
	assert!(find_row(&draw(&mut app, 120, 36), "MEAN/FLOOR").is_some());
	type_text(&mut app, "/95");
	assert!(find_row(&draw(&mut app, 120, 36), "between 70 and the mean (93)").is_some());
	press(&mut app, KeyCode::Enter);
	assert_eq!(overrides(&app), Overrides::default());
	press(&mut app, KeyCode::Esc);
	press(&mut app, KeyCode::Esc);

	enter_quality(&mut app, AUDIO, "100");
	assert!(find_row(&draw(&mut app, 120, 36), "in steps of 8").is_some());
	press(&mut app, KeyCode::Esc);
	press(&mut app, KeyCode::Esc);

	enter_quality(&mut app, AUDIO, "loud");
	assert!(find_row(&draw(&mut app, 120, 36), "\"loud\" is not a number").is_some());
	assert!(!app.should_quit(), "typing goes into the editor");
	press(&mut app, KeyCode::Esc);
	assert_eq!(overrides(&app), Overrides::default());
}

#[test]
fn reset_returns_one_row_or_every_row_to_the_preset() {
	let mut app = app();
	enter_quality(&mut app, VIDEO, "90/85");
	press(&mut app, KeyCode::Esc);
	enter_quality(&mut app, IMAGE, "80");
	press(&mut app, KeyCode::Esc);
	enter_quality(&mut app, AUDIO, "160");
	assert_eq!(
		overrides(&app),
		Overrides {
			video: Some(VmafOverride {
				mean: 90.0,
				floor: 85.0
			}),
			image: Some(80.0),
			audio_stereo_kbps: Some(160),
		}
	);
	press(&mut app, KeyCode::Esc);

	open_row(&mut app, IMAGE);
	press(&mut app, KeyCode::Char('r'));
	assert_eq!(overrides(&app).image, None);
	assert_eq!(overrides(&app).audio_stereo_kbps, Some(160));
	assert!(shown_row(&mut app, "Image SSIMULACRA2").contains("preset · 80"));
	press(&mut app, KeyCode::Esc);

	open_row(&mut app, RESET_QUALITY);
	press(&mut app, KeyCode::Enter);
	assert_eq!(overrides(&app), Overrides::default());
	assert!(shown_row(&mut app, "Reset quality").contains("nothing to reset"));
}

#[test]
fn the_editor_draws_the_cursor_at_its_real_position_not_always_the_end() {
	let mut app = app();
	open_row(&mut app, AUDIO);
	press(&mut app, KeyCode::Enter);
	assert_eq!(app.overlay(), Some(OverlayKind::EditQuality));
	type_text(&mut app, "160");
	press(&mut app, KeyCode::Left);
	press(&mut app, KeyCode::Left);

	let terminal = draw(&mut app, 120, 36);
	let row = find_row(&terminal, "Audio stereo").unwrap();
	let text = row_text(&terminal, row);
	assert!(text.contains("160"), "{text}");

	let cursor_x = find_on_row(&terminal, row, "6").unwrap();
	let cell = &terminal.backend().buffer()[(cursor_x, row)];
	assert_eq!(cell.symbol(), "6");
	assert!(
		cell.modifier.contains(Modifier::REVERSED),
		"the cursor sits mid-text, not at the end"
	);
}

#[test]
fn clicking_the_reset_chip_resets_the_row_without_closing_settings() {
	let mut app = app();
	enter_quality(&mut app, IMAGE, "80");
	assert_eq!(overrides(&app).image, Some(80.0));

	let terminal = draw(&mut app, 120, 36);
	let chip = find_on_row(&terminal, 35, " r ").unwrap();
	click(&mut app, chip + 1, 35, Instant::now());

	assert_eq!(
		app.overlay(),
		Some(OverlayKind::Settings),
		"the chip must not close settings"
	);
	assert_eq!(overrides(&app).image, None);
}

#[test]
fn an_emptied_editor_returns_the_row_to_the_preset() {
	let mut app = app();
	enter_quality(&mut app, IMAGE, "80");

	press(&mut app, KeyCode::Enter);
	press(&mut app, KeyCode::Backspace);
	press(&mut app, KeyCode::Backspace);
	press(&mut app, KeyCode::Enter);

	assert_eq!(overrides(&app), Overrides::default());
}

#[test]
fn the_header_names_a_custom_profile() {
	let mut app = app();
	let header = |app: &mut App<MockSource>| row_text(&draw(app, 120, 36), 0);
	assert!(header(&mut app).contains(" MODERATE · LATEST "));

	enter_quality(&mut app, AUDIO, "160");

	assert!(header(&mut app).contains(" MODERATE · custom · LATEST "));
	press(&mut app, KeyCode::Char('r'));
	assert!(!header(&mut app).contains("custom"));
}

#[test]
fn at_fast_the_video_row_is_disabled_and_keeps_its_value() {
	let mut app = app();
	enter_quality(&mut app, VIDEO, "90/85");
	press(&mut app, KeyCode::Esc);
	open_row(&mut app, SPEED);
	press(&mut app, KeyCode::Right);
	assert_eq!(app.source().view().settings.speed, Speed::Fast);

	let video = shown_row(&mut app, "Video VMAF");
	assert!(video.contains("FAST never measures video quality"), "{video}");
	assert!(video.contains("90/85"), "{video}");
	assert!(!video.contains("custom"), "{video}");
	let header = row_text(&draw(&mut app, 120, 36), 0);
	assert!(header.contains(" FAST · LATEST "), "{header}");

	press(&mut app, KeyCode::Down);
	press(&mut app, KeyCode::Enter);
	assert_eq!(app.overlay(), Some(OverlayKind::Settings), "the row can't be edited");
	press(&mut app, KeyCode::Char('r'));
	assert!(overrides(&app).video.is_some(), "nor reset");

	press(&mut app, KeyCode::Up);
	press(&mut app, KeyCode::Left);
	assert_eq!(app.source().view().settings.speed, Speed::Moderate);
	assert!(shown_row(&mut app, "Video VMAF").contains("custom · 90/85"));
	assert!(row_text(&draw(&mut app, 120, 36), 0).contains(" MODERATE · custom · LATEST "));
}

#[test]
fn the_motion_photo_row_says_what_turning_it_off_drops() {
	let mut app = app();
	open_row(&mut app, KEEP_AUXILIARY_IMAGES);

	let note = "off: motion and Live Focus photos convert without the video, unblurred original, depth map";
	assert!(find_row(&draw(&mut app, 120, 36), note).is_some(), "{note}");
}

#[test]
fn the_blu_ray_row_switches_between_keeping_and_converting_a_disc() {
	let mut app = app();
	open_row(&mut app, BLU_RAY);
	assert_eq!(app.source().view().settings.blu_ray, BluRay::Keep);
	assert!(shown_row(&mut app, "Blu-ray discs").contains("keep as is"));

	press(&mut app, KeyCode::Right);
	assert_eq!(app.source().view().settings.blu_ray, BluRay::Convert);
	assert!(shown_row(&mut app, "Blu-ray discs").contains("convert main feature"));

	press(&mut app, KeyCode::Left);
	assert_eq!(app.source().view().settings.blu_ray, BluRay::Keep);

	press(&mut app, KeyCode::Enter);
	assert_eq!(app.source().view().settings.blu_ray, BluRay::Convert);
}

#[test]
fn a_fast_project_with_an_unused_video_override_can_still_be_reset() {
	let mut app = app();
	enter_quality(&mut app, VIDEO, "90/85");
	press(&mut app, KeyCode::Esc);
	open_row(&mut app, SPEED);
	press(&mut app, KeyCode::Right);

	for _ in 0..RESET_QUALITY {
		press(&mut app, KeyCode::Down);
	}
	press(&mut app, KeyCode::Enter);

	assert_eq!(overrides(&app), Overrides::default());
}

#[test]
fn s_opens_settings_and_arrows_change_them() {
	let mut app = app();
	open_row(&mut app, 0);
	assert_eq!(app.overlay(), Some(OverlayKind::Settings));

	press(&mut app, KeyCode::Right);
	assert_eq!(app.source().view().settings.speed, Speed::Fast);

	for _ in 0..MIN_SAVINGS {
		press(&mut app, KeyCode::Down);
	}
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
	open_row(&mut app, DELETE_ORIGINALS);
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

	open_row(&mut app, RESTART_PENDING_JOBS);
	press(&mut app, KeyCode::Enter);
	assert_eq!(app.overlay(), Some(OverlayKind::ConfirmRestart));

	press(&mut app, KeyCode::Esc);
	assert!(app.source().view().limit().is_some_and(|limit| limit.pending));

	open_row(&mut app, RESTART_PENDING_JOBS);
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
	open_row(&mut app, LIMIT);

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
