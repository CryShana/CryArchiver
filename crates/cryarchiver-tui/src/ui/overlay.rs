use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;

use super::widgets::{bold, fg, modal, put, to_u16};
use crate::app::{Hit, HitMap, Menu};
use crate::format::{fit, width};
use crate::theme::Theme;

const HELP_LEFT: [(&str, &str); 10] = [
	("↑ ↓  j k", "move selection"),
	("PgUp PgDn", "page"),
	("Home End", "first / last"),
	("← →", "collapse / expand"),
	("⏎", "details, and back"),
	("/", "filter by name"),
	("Esc", "close / back / clear"),
	("z", "maximize library"),
	("L", "maximize log, where"),
	("", "↑↓ Home End scroll it"),
];

const HELP_RIGHT: [(&str, &str); 10] = [
	("s", "settings"),
	("l", "toggle log"),
	("i", "issues"),
	("m", "context menu"),
	("p", "pause / resume"),
	("e", "performance limit"),
	("r", "retry selected"),
	("F2", "mouse capture on/off"),
	("?", "this help"),
	("q", "quit (safe, resumable)"),
];

const MOUSE_HELP: [&str; 3] = [
	"Mouse  click selects · click ▸ folds · double-click opens details",
	"       right-click menu · wheel scrolls · Shift+drag selects text",
	"       right-click a log line copies it",
];

pub fn help(buf: &mut Buffer, screen: Rect, theme: &Theme) {
	let width = screen.width.saturating_sub(6).min(86);
	let height = screen.height.saturating_sub(4).min(17);
	let area = screen.centered(Constraint::Length(width), Constraint::Length(height));
	let inner = modal(buf, area, screen, theme, "Keys");

	let half = inner.width / 2;
	help_column(
		buf,
		Rect {
			x: inner.x + 2,
			width: half - 2,
			..inner
		},
		&HELP_LEFT,
		theme,
	);
	help_column(
		buf,
		Rect {
			x: inner.x + half + 1,
			width: half - 1,
			..inner
		},
		&HELP_RIGHT,
		theme,
	);

	let cells = usize::from(inner.width.saturating_sub(4));
	let first_row = inner.bottom().saturating_sub(3);
	for (row, text) in (first_row..inner.bottom()).zip(MOUSE_HELP) {
		put(buf, inner.x + 2, row, &fit(text, cells), fg(theme.dim), inner.right());
	}
}

fn help_column(buf: &mut Buffer, area: Rect, entries: &[(&str, &str)], theme: &Theme) {
	let rows = (area.y + 1..area.bottom().saturating_sub(4)).zip(entries);
	for (y, (key, description)) in rows {
		put(buf, area.x, y, &fit(key, 12), bold(theme.accent), area.right());
		put(buf, area.x + 12, y, description, fg(theme.text), area.right());
	}
}

pub fn menu(buf: &mut Buffer, screen: Rect, menu: &Menu, default_anchor: (u16, u16), theme: &Theme, hits: &mut HitMap) {
	let longest = menu.items.iter().map(|(_, label)| width(label)).max().unwrap_or(0);
	let width = to_u16(longest.max(22) + 4);
	let height = to_u16(menu.items.len() + 2);

	let (anchor_x, anchor_y) = menu.anchor.unwrap_or(default_anchor);
	let x = anchor_x.min(screen.right().saturating_sub(width + 1));
	let y = (anchor_y + 1).clamp(1, screen.bottom().saturating_sub(height + 1));
	let area = Rect::new(x, y, width, height).intersection(screen);

	let inner = modal(buf, area, screen, theme, "");
	hits.push(area, Hit::Menu);

	for (index, ((_, label), row)) in menu.items.iter().zip(inner.y..inner.bottom()).enumerate() {
		let hot = index == menu.selected;
		let row_area = Rect::new(inner.x, row, inner.width, 1);
		let (style, background) = if hot {
			(bold(theme.bright), theme.selection)
		} else {
			(fg(theme.text), theme.panel)
		};

		buf.set_style(row_area, Style::new().bg(background));
		put(buf, inner.x + 1, row, label, style, inner.right());
		hits.push(row_area, Hit::MenuItem(index));
	}
}

pub fn stopping(buf: &mut Buffer, screen: Rect, theme: &Theme) {
	let area = screen.centered(Constraint::Length(50), Constraint::Length(5));
	let inner = modal(buf, area, screen, theme, "Stopping");
	let right = inner.right();

	put(
		buf,
		inner.x + 2,
		inner.y + 1,
		"■ Stopping… running jobs are requeued",
		bold(theme.warn),
		right,
	);
	put(
		buf,
		inner.x + 2,
		inner.y + 2,
		"Nothing is lost; reopening resumes the work",
		fg(theme.dim),
		right,
	);
}
