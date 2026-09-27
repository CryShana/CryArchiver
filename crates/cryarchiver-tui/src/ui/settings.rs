use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use tui_input::Input;

use super::widgets::{bold, fg, modal, put, to_u16};
use crate::app::{Hit, HitMap, SETTINGS_ROWS};
use crate::format::fit;
use crate::theme::Theme;
use crate::view::ArchiveView;

const LABEL: u16 = 32;

pub fn render(buf: &mut Buffer, screen: Rect, view: &ArchiveView, selected: usize, theme: &Theme, hits: &mut HitMap) {
	let height = to_u16(SETTINGS_ROWS.len()) + 6;
	let area = screen.centered(
		Constraint::Length(screen.width.saturating_sub(10).min(84)),
		Constraint::Length(height),
	);
	let inner = modal(buf, area, screen, theme, "Settings");
	hits.push(area, Hit::Modal);

	for (index, (row, y)) in SETTINGS_ROWS.iter().zip(inner.y + 1..).enumerate() {
		let hot = index == selected;
		let line = Rect::new(inner.x, y, inner.width, 1);
		let background = if hot { theme.selection } else { theme.panel };
		buf.set_style(line, Style::new().bg(background));

		let label_style = if hot { bold(theme.bright) } else { fg(theme.text) };
		put(buf, inner.x + 2, y, row.label(), label_style, inner.x + LABEL);

		let value = row.value(view);
		let value_color = match row {
			crate::app::SettingsRow::DeleteOriginals if view.settings.delete_originals => theme.error,
			_ => theme.accent,
		};
		let cells = usize::from(inner.width.saturating_sub(LABEL + 2));
		put(
			buf,
			inner.x + LABEL,
			y,
			&fit(&value, cells),
			bold(value_color),
			inner.right(),
		);
		hits.push(line, Hit::SettingsRow(index));
	}

	let note = "Changes apply at once · the limit reaches running jobs where the system allows";
	put(
		buf,
		inner.x + 2,
		inner.bottom() - 1,
		&fit(note, usize::from(inner.width.saturating_sub(4))),
		fg(theme.dim),
		inner.right(),
	);
}

pub fn confirm_delete(
	buf: &mut Buffer,
	screen: Rect,
	view: &ArchiveView,
	input: &Input,
	theme: &Theme,
	hits: &mut HitMap,
) {
	let area = screen.centered(
		Constraint::Length(screen.width.saturating_sub(10).min(72)),
		Constraint::Length(10),
	);
	let inner = modal(buf, area, screen, theme, "Delete originals?");
	hits.push(area, Hit::Modal);
	let right = inner.right();
	let cells = usize::from(inner.width.saturating_sub(4));

	put(
		buf,
		inner.x + 2,
		inner.y + 1,
		"⚠ Deleting an original is permanent: there is no Recycle Bin.",
		bold(theme.error),
		right,
	);
	let lines = [
		"An original is deleted only after its archived copy is read back",
		"from disk and verified, and only if it has no open issue.",
	];
	for (y, line) in (inner.y + 2..).zip(lines) {
		put(buf, inner.x + 2, y, &fit(line, cells), fg(theme.text), right);
	}

	let ask = if view.settings.delete_originals {
		"Type DELETE to allow it for this session:"
	} else {
		"Type DELETE to turn it on:"
	};
	put(buf, inner.x + 2, inner.y + 5, ask, fg(theme.dim), right);
	let field = Rect::new(inner.x + 2, inner.y + 6, inner.width.saturating_sub(4).min(24), 1);
	buf.set_style(field, Style::new().bg(theme.selection));
	let typed = format!("{}█", input.value());
	put(
		buf,
		field.x + 1,
		field.y,
		&typed,
		bold(theme.bright).bg(theme.selection),
		field.right(),
	);
}

pub fn confirm_restart(buf: &mut Buffer, screen: Rect, view: &ArchiveView, theme: &Theme, hits: &mut HitMap) {
	let area = screen.centered(
		Constraint::Length(screen.width.saturating_sub(10).min(64)),
		Constraint::Length(7),
	);
	let inner = modal(buf, area, screen, theme, "Restart jobs now?");
	hits.push(area, Hit::Modal);
	let right = inner.right();

	let over = view.workers.jobs.iter().filter(|job| job.over_limit).count();
	let noun = if over == 1 { "job" } else { "jobs" };
	put(
		buf,
		inner.x + 2,
		inner.y + 1,
		&format!("{over} {noun} run beyond the new performance limit."),
		fg(theme.text),
		right,
	);
	put(
		buf,
		inner.x + 2,
		inner.y + 2,
		"Restarting cancels and requeues them: their progress will be lost.",
		bold(theme.warn),
		right,
	);
	put(
		buf,
		inner.x + 2,
		inner.y + 4,
		"⏎ restart now · Esc keep them running",
		fg(theme.dim),
		right,
	);
}
