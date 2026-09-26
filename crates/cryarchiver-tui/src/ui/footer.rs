use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tui_input::Input;

use super::widgets::{VERB_KEYS, bold, fg, put, to_u16};
use crate::app::{Hit, Overlay, Ui, ViewMode};
use crate::format::width;
use crate::view::{ArchiveView, Duplicates, FileDetails, IssueVerbs, RunState};

struct Chip {
	key: Cow<'static, str>,
	label: Cow<'static, str>,
	code: Option<KeyCode>,
}

fn chip(key: &'static str, label: &'static str, code: Option<KeyCode>) -> Chip {
	Chip {
		key: Cow::Borrowed(key),
		label: Cow::Borrowed(label),
		code,
	}
}

pub fn render(buf: &mut Buffer, area: Rect, view: &ArchiveView, details: Option<&FileDetails>, ui: &mut Ui) {
	buf.set_style(area, Style::new().bg(ui.theme.panel));

	if let Some(input) = &ui.search {
		render_search(buf, area, input, ui.tree.matching_files(), ui);
		return;
	}

	let right = area.right();
	let mut x = area.x + 1;
	for chip in chips(view, details, ui) {
		let needed = to_u16(width(&chip.key) + width(&chip.label) + 4);
		if x + needed > right {
			break;
		}

		let start = x;
		let key_style = bold(ui.theme.accent).bg(ui.theme.selection);
		x = put(buf, x, area.y, &format!(" {} ", chip.key), key_style, right);
		x = put(buf, x + 1, area.y, &chip.label, fg(ui.theme.dim), right) + 2;

		if let Some(code) = chip.code {
			ui.hits
				.push(Rect::new(start, area.y, x - start - 1, 1), Hit::Footer(code));
		}
	}
}

fn render_search(buf: &mut Buffer, area: Rect, input: &Input, matching: Option<usize>, ui: &Ui) {
	let theme = &ui.theme;
	let (y, right) = (area.y, area.right());
	let mut x = put(buf, area.x, y, " / ", bold(theme.background).bg(theme.accent), right);
	x += 1;

	let value = input.value();
	let cursor = value
		.char_indices()
		.nth(input.cursor())
		.map_or(value.len(), |(index, _)| index);
	let (before, after) = value.split_at(cursor);
	let mut after_chars = after.chars();
	let under_cursor = after_chars.next().map_or_else(|| "█".to_owned(), |c| c.to_string());

	x = put(buf, x, y, before, fg(theme.bright), right);
	let cursor_style = if after.is_empty() {
		fg(theme.bright)
	} else {
		fg(theme.bright).add_modifier(Modifier::REVERSED)
	};
	x = put(buf, x, y, &under_cursor, cursor_style, right);
	x = put(buf, x, y, after_chars.as_str(), fg(theme.bright), right);

	let count = match matching {
		Some(1) => "1 matching file".to_owned(),
		Some(count) => format!("{count} matching files"),
		None => "searching…".to_owned(),
	};
	let hint = format!("{count} · ⏎ keep filter · Esc clear");
	put(buf, x + 2, y, &hint, fg(theme.dim), right);
}

fn chips(view: &ArchiveView, details: Option<&FileDetails>, ui: &Ui) -> Vec<Chip> {
	match (&ui.overlay, ui.mode) {
		(Some(Overlay::Menu(_)), _) => vec![
			chip("↑↓", "move", None),
			chip("⏎", "choose", Some(KeyCode::Enter)),
			chip("Esc", "close", Some(KeyCode::Esc)),
		],
		(Some(Overlay::Settings(_)), _) => vec![
			chip("↑↓", "move", None),
			chip("←→", "change", None),
			chip("⏎", "toggle", Some(KeyCode::Enter)),
			chip("Esc", "close", Some(KeyCode::Esc)),
		],
		(Some(Overlay::ConfirmDelete(_)), _) => vec![
			chip("⏎", "confirm", Some(KeyCode::Enter)),
			chip("Esc", "cancel", Some(KeyCode::Esc)),
		],
		(Some(Overlay::ConfirmRestart), _) => vec![
			chip("⏎", "restart now", Some(KeyCode::Enter)),
			chip("Esc", "cancel", Some(KeyCode::Esc)),
		],
		(Some(Overlay::Help), _) => vec![chip("Esc", "close", Some(KeyCode::Esc))],
		(None, ViewMode::Library) => library_chips(view, ui),
		(None, ViewMode::Details) => details_chips(details),
		(None, ViewMode::Issues) => issues_chips(view, ui),
	}
}

/// One chip per verb the issue allows, labelled for its kind; the key stays r, c or x.
fn verb_chips(verbs: &IssueVerbs) -> Vec<Chip> {
	VERB_KEYS
		.iter()
		.filter_map(|&(verb, key)| {
			verbs.label(verb).map(|label| Chip {
				key: Cow::Owned(key.to_string()),
				label: Cow::Owned(label.to_lowercase()),
				code: Some(KeyCode::Char(key)),
			})
		})
		.collect()
}

fn library_chips(view: &ArchiveView, ui: &Ui) -> Vec<Chip> {
	let mut chips = vec![
		chip("↑↓", "move", None),
		chip("←→", "fold", None),
		chip("⏎", "details", Some(KeyCode::Enter)),
		chip("/", "search", Some(KeyCode::Char('/'))),
		chip(
			"z",
			if ui.layout.maximized { "restore" } else { "maximize" },
			Some(KeyCode::Char('z')),
		),
	];

	if !ui.layout.maximized {
		chips.push(chip("l", "log", Some(KeyCode::Char('l'))));
	}

	let pause = if view.header.run_state == RunState::Paused {
		"resume"
	} else {
		"pause"
	};
	chips.extend([
		chip("i", "issues", Some(KeyCode::Char('i'))),
		chip("m", "menu", Some(KeyCode::Char('m'))),
		chip("p", pause, Some(KeyCode::Char('p'))),
		chip("e", "limit", Some(KeyCode::Char('e'))),
		chip("s", "settings", Some(KeyCode::Char('s'))),
		chip("?", "help", Some(KeyCode::Char('?'))),
		chip("q", "quit", Some(KeyCode::Char('q'))),
	]);
	chips
}

fn issues_chips(view: &ArchiveView, ui: &Ui) -> Vec<Chip> {
	let mut chips = vec![chip("↑↓", "move", None), chip("⏎", "details", Some(KeyCode::Enter))];
	if let Some(issue) = view.issues.get(ui.issue_selected) {
		chips.extend(verb_chips(&issue.verbs));
	}

	chips.extend([
		chip("Esc", "back", Some(KeyCode::Esc)),
		chip("?", "help", Some(KeyCode::Char('?'))),
		chip("q", "quit", Some(KeyCode::Char('q'))),
	]);
	chips
}

fn details_chips(details: Option<&FileDetails>) -> Vec<Chip> {
	let mut chips = vec![
		chip("↑↓", "prev/next file", None),
		chip("⏎", "back", Some(KeyCode::Enter)),
	];

	if details.is_some_and(|details| details.duplicates != Duplicates::None) {
		chips.push(chip("g", "go to duplicate", Some(KeyCode::Char('g'))));
	}

	if let Some(issue) = details.and_then(|details| details.issue.as_ref()) {
		chips.extend(verb_chips(&issue.verbs));
	}

	chips.extend([
		chip("m", "menu", Some(KeyCode::Char('m'))),
		chip("?", "help", Some(KeyCode::Char('?'))),
		chip("q", "quit", Some(KeyCode::Char('q'))),
	]);
	chips
}
