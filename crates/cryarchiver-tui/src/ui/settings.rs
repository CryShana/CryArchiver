use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use tui_input::Input;

use super::widgets::{bold, fg, modal, put, to_u16};
use crate::app::{FAST_NEVER_MEASURES_VIDEO, Hit, HitMap, SETTINGS_ROWS, SettingsRow};
use crate::format::fit;
use crate::theme::Theme;
use crate::view::ArchiveView;

const LABEL: u16 = 32;

/// `row` is the selected row, with the quality value being typed into it, if any.
pub fn render(
	buf: &mut Buffer,
	screen: Rect,
	view: &ArchiveView,
	(selected, editing): (usize, Option<&Input>),
	theme: &Theme,
	hits: &mut HitMap,
) {
	let height = to_u16(SETTINGS_ROWS.len()) + 6;
	let area = screen.centered(
		Constraint::Length(screen.width.saturating_sub(10).min(96)),
		Constraint::Length(height),
	);
	let inner = modal(buf, area, screen, theme, "Settings");
	hits.push(area, Hit::Modal);
	let settings = &view.settings;
	let cells = usize::from(inner.width.saturating_sub(LABEL + 2));

	for (index, (&row, y)) in SETTINGS_ROWS.iter().zip(inner.y + 1..).enumerate() {
		let hot = index == selected;
		let line = Rect::new(inner.x, y, inner.width, 1);
		let background = if hot { theme.selection } else { theme.panel };
		buf.set_style(line, Style::new().bg(background));

		let label_style = if hot { bold(theme.bright) } else { fg(theme.text) };
		put(buf, inner.x + 2, y, row.label(), label_style, inner.x + LABEL);

		if let (SettingsRow::Quality(field), Some(input)) = (row, editing.filter(|_| hot)) {
			let end = draw_editor(buf, inner.x + LABEL, y, input, bold(theme.bright), inner.right());
			if let Some(preset) = field.preset_text(settings.speed.preset()) {
				let hint = format!("preset {}", field.shown(&preset));
				put(buf, end + 2, y, &hint, fg(theme.dim), inner.right());
			}
		} else {
			let style = match row {
				SettingsRow::DeleteOriginals if settings.delete_originals => bold(theme.error),
				_ if row.dimmed(settings) => fg(theme.dim),
				_ => bold(theme.accent),
			};
			put(
				buf,
				inner.x + LABEL,
				y,
				&fit(&row.value(view), cells),
				style,
				inner.right(),
			);
		}
		hits.push(line, Hit::SettingsRow(index));
	}

	let (note, style) = note(
		view,
		SETTINGS_ROWS[selected.min(SETTINGS_ROWS.len() - 1)],
		editing,
		theme,
	);
	put(
		buf,
		inner.x + 2,
		inner.bottom() - 1,
		&fit(&note, usize::from(inner.width.saturating_sub(4))),
		style,
		inner.right(),
	);
}

/// Draws the input's value with the cursor at `tui_input`'s real position: a reversed cell mid-text, or a block
/// cursor past the end. Returns where the value ended.
fn draw_editor(buf: &mut Buffer, x: u16, y: u16, input: &Input, style: Style, right: u16) -> u16 {
	let chars: Vec<char> = input.value().chars().collect();
	let cursor = input.cursor().min(chars.len());

	let before: String = chars[..cursor].iter().collect();
	let after: String = chars.get(cursor + 1..).unwrap_or_default().iter().collect();
	let under = chars.get(cursor).map_or_else(|| "█".to_owned(), char::to_string);
	let cursor_style = if cursor < chars.len() {
		style.add_modifier(Modifier::REVERSED)
	} else {
		style
	};

	let end = put(buf, x, y, &before, style, right);
	let end = put(buf, end, y, &under, cursor_style, right);
	put(buf, end, y, &after, style, right)
}

/// While a quality value is typed, the reason it can't be applied, as it is typed.
fn note(view: &ArchiveView, row: SettingsRow, editing: Option<&Input>, theme: &Theme) -> (String, Style) {
	let settings = &view.settings;
	if let (SettingsRow::Quality(field), Some(input)) = (row, editing) {
		return match field.with(settings.overrides, input.value()) {
			Err(reason) => (format!("✗ {reason}"), fg(theme.error)),
			Ok(_) => (
				"⏎ apply · an empty value returns to the preset · Esc cancel".to_owned(),
				fg(theme.dim),
			),
		};
	}

	let text = match row {
		SettingsRow::Quality(field) if !field.applies(settings.speed.preset()) => {
			format!("{FAST_NEVER_MEASURES_VIDEO}; a value set here is kept for MODERATE and SLOW")
		}
		SettingsRow::Quality(_) => "⏎ type your own target · r back to the preset · applies to new work".to_owned(),
		_ => "Changes apply at once · the limit reaches running jobs where the system allows".to_owned(),
	};
	(text, fg(theme.dim))
}

pub fn confirm_delete(
	buf: &mut Buffer,
	screen: Rect,
	view: &ArchiveView,
	input: &Input,
	theme: &Theme,
	hits: &mut HitMap,
) {
	let ask = if view.settings.delete_originals {
		"Type DELETE to allow it for this session:"
	} else {
		"Type DELETE to turn it on:"
	};
	let area = delete_prompt(buf, screen, ask, input, theme);
	hits.push(area, Hit::Modal);
}

/// The dangerous-mode warning with the `DELETE` field; returns the modal's area.
pub fn delete_prompt(buf: &mut Buffer, screen: Rect, ask: &str, input: &Input, theme: &Theme) -> Rect {
	let area = screen.centered(
		Constraint::Length(screen.width.saturating_sub(10).min(72)),
		Constraint::Length(10),
	);
	let inner = modal(buf, area, screen, theme, "Delete originals?");
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
	area
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
