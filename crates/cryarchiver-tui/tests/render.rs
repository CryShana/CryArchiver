mod support;

use cryarchiver_tui::{App, ColorDepth, MockSource};
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
