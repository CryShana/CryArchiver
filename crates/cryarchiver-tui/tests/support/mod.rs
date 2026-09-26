#![allow(dead_code)]

use std::time::Instant;

use cryarchiver_tui::{App, ColorDepth, MockSource};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
	Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

pub const SEED: u64 = 7;
pub const WARM_UP_STEPS: usize = 12;

pub fn source() -> MockSource {
	let mut source = MockSource::new(SEED);
	for _ in 0..WARM_UP_STEPS {
		source.step();
	}
	source
}

pub fn app() -> App<MockSource> {
	App::new(source(), ColorDepth::TrueColor)
}

pub fn draw(app: &mut App<MockSource>, width: u16, height: u16) -> Terminal<TestBackend> {
	let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
	terminal.draw(|frame| app.draw(frame)).unwrap();
	terminal
}

pub fn press(app: &mut App<MockSource>, code: KeyCode) {
	app.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), Instant::now());
}

pub fn press_with(app: &mut App<MockSource>, code: KeyCode, modifiers: KeyModifiers) {
	app.handle_event(&Event::Key(KeyEvent::new(code, modifiers)), Instant::now());
}

pub fn release(app: &mut App<MockSource>, code: KeyCode) {
	let mut key = KeyEvent::new(code, KeyModifiers::NONE);
	key.kind = KeyEventKind::Release;
	app.handle_event(&Event::Key(key), Instant::now());
}

pub fn type_text(app: &mut App<MockSource>, text: &str) {
	for c in text.chars() {
		press(app, KeyCode::Char(c));
	}
}

pub fn mouse(app: &mut App<MockSource>, kind: MouseEventKind, column: u16, row: u16, at: Instant) {
	let event = MouseEvent {
		kind,
		column,
		row,
		modifiers: KeyModifiers::NONE,
	};
	app.handle_event(&Event::Mouse(event), at);
}

pub fn click(app: &mut App<MockSource>, column: u16, row: u16, at: Instant) {
	mouse(app, MouseEventKind::Down(MouseButton::Left), column, row, at);
}

/// The first cell whose symbol starts `text` on `row`, as the user would see it.
pub fn find_on_row(terminal: &Terminal<TestBackend>, row: u16, text: &str) -> Option<u16> {
	let line = row_text(terminal, row);
	let byte = line.find(text)?;
	Some(u16::try_from(line[..byte].chars().count()).unwrap())
}

pub fn row_text(terminal: &Terminal<TestBackend>, row: u16) -> String {
	let buffer = terminal.backend().buffer();
	(0..buffer.area.width).map(|x| buffer[(x, row)].symbol()).collect()
}

pub fn find_row(terminal: &Terminal<TestBackend>, text: &str) -> Option<u16> {
	let height = terminal.backend().buffer().area.height;
	(0..height).find(|&row| row_text(terminal, row).contains(text))
}
