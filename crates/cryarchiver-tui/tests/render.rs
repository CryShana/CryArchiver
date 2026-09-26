mod support;

use cryarchiver_tui::source::SettingsChange;
use cryarchiver_tui::{Action, App, ArchiveSource, ColorDepth, MockSource};
use insta::assert_snapshot;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Color;

use support::{app, draw, find_row, press, type_text};

#[test]
fn dashboard_at_100x30() {
	assert_snapshot!(draw(&mut app(), 100, 30).backend());
}

#[test]
fn dashboard_at_120x36() {
	assert_snapshot!(draw(&mut app(), 120, 36).backend());
}

#[test]
fn dashboard_at_160x45() {
	assert_snapshot!(draw(&mut app(), 160, 45).backend());
}

#[test]
fn maximized_at_100x30() {
	let mut app = app();
	press(&mut app, KeyCode::Char('z'));

	assert_snapshot!(draw(&mut app, 100, 30).backend());
}

#[test]
fn maximized_at_160x45() {
	let mut app = app();
	press(&mut app, KeyCode::Char('z'));

	assert_snapshot!(draw(&mut app, 160, 45).backend());
}

#[test]
fn collapsed_log_at_120x36() {
	let mut app = app();
	press(&mut app, KeyCode::Char('l'));

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn file_details_at_120x36() {
	let mut app = app();
	press(&mut app, KeyCode::Enter);

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn file_details_while_maximized_at_100x30() {
	let mut app = app();
	press(&mut app, KeyCode::Char('z'));
	press(&mut app, KeyCode::Enter);

	assert_snapshot!(draw(&mut app, 100, 30).backend());
}

#[test]
fn issues_at_120x36() {
	let mut app = app();
	press(&mut app, KeyCode::Char('i'));

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn context_menu_at_120x36() {
	let mut app = app();
	press(&mut app, KeyCode::Char('m'));

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn help_at_120x36() {
	let mut app = app();
	press(&mut app, KeyCode::Char('?'));

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn search_at_120x36() {
	let mut app = app();
	press(&mut app, KeyCode::Char('/'));
	type_text(&mut app, "fanfares");

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn too_small_at_80x24() {
	assert_snapshot!(draw(&mut app(), 80, 24).backend());
}

#[test]
fn completed_run_at_120x36() {
	let mut app = App::new(MockSource::completed(support::SEED), ColorDepth::TrueColor);

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn settings_at_120x36() {
	let mut app = app();
	press(&mut app, KeyCode::Char('s'));

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn delete_confirmation_at_120x36() {
	let mut app = app();
	press(&mut app, KeyCode::Char('s'));
	for _ in 0..6 {
		press(&mut app, KeyCode::Down);
	}
	press(&mut app, KeyCode::Enter);
	type_text(&mut app, "DEL");

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn deleting_originals_and_a_pending_limit_at_120x36() {
	let mut source = support::source();
	let confirm = SettingsChange::DeleteOriginals {
		enabled: true,
		confirmation: "DELETE".to_owned(),
	};
	source.perform(Action::ChangeSettings(confirm));
	source.perform(Action::ChangeSettings(SettingsChange::PerformanceLimit(Some(25))));
	let mut app = App::new(source, ColorDepth::TrueColor);
	let terminal = draw(&mut app, 120, 36);

	assert_eq!(
		terminal.backend().buffer()[(70, 0)].bg,
		Color::Rgb(0x4A, 0x16, 0x19),
		"the header turns red"
	);
	assert_snapshot!(terminal.backend());
}

#[test]
fn low_disk_pause_at_100x30() {
	let mut source = support::source();
	for _ in support::WARM_UP_STEPS..80 {
		source.step();
	}
	let mut app = App::new(source, ColorDepth::TrueColor);

	assert_snapshot!(draw(&mut app, 100, 30).backend());
}

#[test]
fn issue_without_a_source_file_at_120x36() {
	let mut app = app();
	press(&mut app, KeyCode::Char('i'));
	press(&mut app, KeyCode::Down);
	press(&mut app, KeyCode::Down);
	press(&mut app, KeyCode::Down);

	assert_snapshot!(draw(&mut app, 120, 36).backend());
}

#[test]
fn loading_rows_show_until_the_source_answers() {
	let mut source = support::source().with_load_latency(3);
	source.step();
	let mut app = App::new(source, ColorDepth::TrueColor);

	let terminal = draw(&mut app, 120, 36);
	assert!(find_row(&terminal, "… loading…").is_some());

	for _ in 0..3 {
		app.source_mut().step();
	}
	let terminal = draw(&mut app, 120, 36);
	assert!(find_row(&terminal, "loading…").is_none());
	assert!(
		find_row(&terminal, "● IMG_0001 2.MOV").is_some(),
		"the pending selection lands once rows load"
	);
}

#[test]
fn notifications_show_as_a_toast_in_the_log() {
	let mut app = app();
	press(&mut app, KeyCode::Char('e'));

	let terminal = draw(&mut app, 120, 36);
	let log_title = find_row(&terminal, "─ Log ").unwrap();

	assert!(support::row_text(&terminal, log_title).contains("◆ performance limit 25 %"));
	assert!(app.toast().is_some());
}

#[test]
fn limit_marker_stays_inside_the_gauge_at_100_percent() {
	let mut source = support::source();
	source.perform(Action::ChangeSettings(SettingsChange::PerformanceLimit(Some(100))));
	let mut app = App::new(source, ColorDepth::TrueColor);

	let terminal = draw(&mut app, 120, 36);
	let row = find_row(&terminal, "│ CPU ").unwrap();
	let text = support::row_text(&terminal, row);
	let marker = text.find('┃').unwrap();
	let percent = text[marker..].find('%').unwrap();

	assert!(
		text[marker + '┃'.len_utf8()..marker + percent]
			.trim_start()
			.chars()
			.all(|c| c.is_ascii_digit())
	);
}

#[test]
fn every_cell_has_the_theme_background() {
	let terminal = draw(&mut app(), 120, 36);
	let buffer = terminal.backend().buffer();

	for y in 0..buffer.area.height {
		for x in 0..buffer.area.width {
			let covered_by_wide_glyph = x > 0 && unicode_width(buffer[(x - 1, y)].symbol()) == 2;
			if !covered_by_wide_glyph {
				assert_ne!(buffer[(x, y)].bg, Color::Reset, "cell {x},{y} has no background");
			}
		}
	}
}

fn unicode_width(symbol: &str) -> usize {
	ratatui::text::Span::raw(symbol).width()
}

#[test]
fn selected_row_uses_the_selection_background() {
	let terminal = draw(&mut app(), 120, 36);
	let buffer = terminal.backend().buffer();
	let row = find_row(&terminal, "● IMG_0001 2.MOV").expect("the first worker's file is selected");

	assert_eq!(buffer[(5, row)].bg, Color::Rgb(0x1B, 0x2B, 0x3B));
	assert_eq!(
		buffer[(0, 0)].bg,
		Color::Rgb(0x3C, 0xC4, 0xCE),
		"the header badge is accent"
	);
}

#[test]
fn state_glyphs_carry_their_colour() {
	let terminal = draw(&mut app(), 160, 45);
	let buffer = terminal.backend().buffer();
	let row = find_row(&terminal, "● IMG_0001 2.MOV").unwrap();
	let glyph_x = (0..buffer.area.width)
		.find(|&x| buffer[(x, row)].symbol() == "●")
		.unwrap();

	assert_eq!(buffer[(glyph_x, row)].fg, Color::Rgb(0x3C, 0xC4, 0xCE));
}

#[test]
fn xterm_256_palette_uses_indexed_colours() {
	let mut app = App::new(support::source(), ColorDepth::Xterm256);
	let terminal = draw(&mut app, 100, 30);

	assert_eq!(terminal.backend().buffer()[(50, 20)].bg, Color::Indexed(233));
}
