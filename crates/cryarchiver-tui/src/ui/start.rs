use cryarchiver_engine::text::display_path;
use cryarchiver_project::settings::Speed;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use tui_input::Input;

use super::footer::{Chip, chip, draw_chips, render_toast};
use super::settings::delete_prompt;
use super::widgets::{Chrome, bold, fg, modal, panel, put, to_u16, wrap};
use super::{MIN_HEIGHT, MIN_WIDTH, small};
use crate::app::HitMap;
use crate::format::{bytes, fit, fit_middle, width};
use crate::recent::{CAPACITY, now_unix_ms};
use crate::start::form::{FormField, NewProjectForm};
use crate::start::validate::Check;
use crate::start::{Dialog, EntryState, RecentEntry, StartHit, StartMenu, opened_ago};
use crate::theme::Theme;

const BADGE: &str = " CryArchiver ";
const ENTRY_ROWS: u16 = 3;
const LABEL: u16 = 20;
const FORM_WIDTH: u16 = 120;
const SPEEDS: [(Speed, &str, &str); 3] = [
	(
		Speed::Slow,
		"SLOW",
		"the smallest files at the highest quality; takes the longest",
	),
	(Speed::Moderate, "MODERATE", "a balance of size, quality and time"),
	(Speed::Fast, "FAST", "the quickest; files come out somewhat larger"),
];

pub fn draw(frame: &mut Frame, menu: &mut StartMenu) {
	let area = frame.area();
	let buf = frame.buffer_mut();
	let theme = menu.theme;
	buf.set_style(area, theme.base());
	menu.hits.clear();

	if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
		small::too_small(buf, area, &theme);
		return;
	}

	let header = Rect { height: 1, ..area };
	let footer = Rect {
		y: area.bottom() - 1,
		height: 1,
		..area
	};
	let body = Rect {
		y: area.y + 1,
		height: area.height - 2,
		..area
	};

	let notice = menu.notice().cloned();
	let StartMenu {
		entries,
		selected,
		form,
		dialog,
		opening,
		hits,
		..
	} = menu;

	let subtitle = if form.is_some() { "New project" } else { "Start" };
	render_header(buf, header, subtitle, &theme);

	match form.as_ref() {
		Some(form) => render_form(buf, body, form, &theme, hits),
		None => render_recents(buf, body, entries, *selected, &theme, hits),
	}

	buf.set_style(footer, Style::new().bg(theme.panel));
	let right = match &notice {
		Some(notice) => render_toast(buf, footer, notice, &theme),
		None => footer.right(),
	};
	let chips = match dialog {
		Some(dialog) => dialog_chips(dialog),
		None => chips(form.as_ref()),
	};
	draw_chips(buf, footer, right, chips, &theme, hits, StartHit::Footer);

	if let Some(input) = form.as_ref().and_then(NewProjectForm::confirming) {
		let prompt = delete_prompt(buf, area, "Type DELETE to turn it on:", input, &theme);
		hits.push(prompt, StartHit::Modal);
	}

	if let Some(dialog) = dialog {
		let modal = render_dialog(buf, area, dialog, &theme);
		hits.push(modal, StartHit::Modal);
	}

	if let Some(name) = opening {
		render_opening(buf, area, name, &theme);
	}
}

fn dialog_chips(dialog: &Dialog) -> Vec<Chip> {
	match dialog {
		Dialog::Error { .. } => vec![
			chip("⏎", "close", Some(KeyCode::Enter)),
			chip("Esc", "close", Some(KeyCode::Esc)),
		],
		Dialog::Discard => vec![
			chip("⏎", "discard", Some(KeyCode::Enter)),
			chip("Esc", "keep editing", Some(KeyCode::Esc)),
		],
	}
}

/// The whole message, wrapped to the dialog's width; returns the dialog's area.
fn render_dialog(buf: &mut Buffer, screen: Rect, dialog: &Dialog, theme: &Theme) -> Rect {
	let width = screen.width.saturating_sub(10).min(80);
	let cells = usize::from(width.saturating_sub(6));
	let (title, lines) = match dialog {
		Dialog::Error { title, text } => (title.as_str(), wrap(text, cells)),
		Dialog::Discard => (
			"Discard the new project?",
			wrap(
				"The values typed into the form are lost. Press ⏎ to discard them, or Esc to keep editing.",
				cells,
			),
		),
	};
	let max_lines = usize::from(screen.height.saturating_sub(8));
	let shown = lines.len().min(max_lines);

	let area = screen.centered(Constraint::Length(width), Constraint::Length(to_u16(shown) + 4));
	let inner = modal(buf, area, screen, theme, title);
	for (y, line) in (inner.y + 1..).zip(lines.iter().take(shown)) {
		put(buf, inner.x + 2, y, &fit(line, cells), fg(theme.text), inner.right());
	}
	area
}

fn render_header(buf: &mut Buffer, area: Rect, subtitle: &str, theme: &Theme) {
	buf.set_style(area, Style::new().bg(theme.panel));
	let x = put(
		buf,
		area.x,
		area.y,
		BADGE,
		bold(theme.background).bg(theme.accent),
		area.right(),
	);
	put(buf, x + 1, area.y, subtitle, bold(theme.bright), area.right());
}

fn chips(form: Option<&NewProjectForm>) -> Vec<Chip> {
	let Some(form) = form else {
		return vec![
			chip("↑↓", "move", None),
			chip("⏎", "open", Some(KeyCode::Enter)),
			chip("n", "new project", Some(KeyCode::Char('n'))),
			chip("x", "remove", Some(KeyCode::Char('x'))),
			chip("c", "clear missing", Some(KeyCode::Char('c'))),
			chip("F2", "mouse", Some(KeyCode::F(2))),
			chip("q", "quit", Some(KeyCode::Char('q'))),
		];
	};

	if form.confirming().is_some() {
		return vec![
			chip("⏎", "confirm", Some(KeyCode::Enter)),
			chip("Esc", "cancel", Some(KeyCode::Esc)),
		];
	}

	let enter = match form.focus() {
		FormField::DeleteOriginals => "toggle",
		FormField::Create => "create",
		FormField::Cancel => "cancel",
		FormField::Source | FormField::Destination | FormField::Name | FormField::Speed => "next field",
	};
	let mut chips = vec![
		chip("Tab", "next field", Some(KeyCode::Tab)),
		chip("⇧Tab", "previous", Some(KeyCode::BackTab)),
		chip("⏎", enter, Some(KeyCode::Enter)),
	];
	if form.focus() == FormField::Speed {
		chips.push(chip("←→", "speed", None));
	}
	chips.extend([chip("Esc", "back", Some(KeyCode::Esc)), chip("Ctrl+C", "quit", None)]);
	chips
}

fn render_recents(
	buf: &mut Buffer,
	body: Rect,
	entries: &[RecentEntry],
	selected: usize,
	theme: &Theme,
	hits: &mut HitMap<StartHit>,
) {
	let list = Rect {
		height: body.height - 3,
		..body
	};
	let count = format!("{} of {CAPACITY}", entries.len());
	let inner = panel(
		buf,
		list,
		theme,
		&Chrome {
			title: "Recent projects",
			right: Some(&count),
			focused: true,
		},
	);

	if entries.is_empty() {
		let lines = [
			"No recent projects yet.",
			"Press n to create one, or open a project file with: cryarchiver PATH.cryarchive",
		];
		for (y, line) in (inner.y + 1..inner.bottom()).zip(lines) {
			put(buf, inner.x + 2, y, line, fg(theme.dim), inner.right());
		}
	}

	let visible = usize::from((inner.height.saturating_sub(1) / ENTRY_ROWS).max(1));
	let first = selected.saturating_sub(visible - 1);
	let now = now_unix_ms();
	for (index, entry) in entries.iter().enumerate().skip(first).take(visible) {
		let y = inner.y + 1 + to_u16(index - first) * ENTRY_ROWS;
		let rows = Rect::new(inner.x, y, inner.width, 2);
		render_entry(buf, rows, entry, index == selected, now, theme);
		hits.push(rows, StartHit::Entry(index));
	}

	let new = Rect {
		y: list.bottom(),
		height: 3,
		..body
	};
	let inner = panel(
		buf,
		new,
		theme,
		&Chrome {
			title: "New project",
			right: None,
			focused: false,
		},
	);
	let x = put(
		buf,
		inner.x + 1,
		inner.y,
		" n ",
		bold(theme.accent).bg(theme.selection),
		inner.right(),
	);
	put(
		buf,
		x + 1,
		inner.y,
		"Archive a folder into another, with a project that resumes where it left off",
		fg(theme.text),
		inner.right(),
	);
	hits.push(new, StartHit::NewProject);
}

fn render_entry(buf: &mut Buffer, rows: Rect, entry: &RecentEntry, hot: bool, now: i64, theme: &Theme) {
	if hot {
		buf.set_style(rows, Style::new().bg(theme.selection));
	}

	let (y, right) = (rows.y, rows.right().saturating_sub(1));
	let summary = entry.summary();
	let name_style = match (summary.is_some(), hot) {
		(false, _) => fg(theme.dim),
		(true, true) => bold(theme.bright),
		(true, false) => fg(theme.bright),
	};

	let ago = format!("opened {}", opened_ago(now, entry.recent.opened_unix_ms));
	let time_x = right.saturating_sub(to_u16(width(&ago)));
	put(buf, time_x, y, &ago, fg(theme.dim), right);

	let name_right = time_x.saturating_sub(2);
	let mut x = put(buf, rows.x + 2, y, &entry.name, name_style, name_right);
	if summary.is_some_and(|summary| summary.delete_originals) {
		x = put(
			buf,
			x + 1,
			y,
			" ⚠ deletes originals ",
			bold(theme.bright).bg(theme.error),
			name_right,
		);
	}

	if summary.is_some_and(|summary| summary.needs_recovery) {
		put(
			buf,
			x + 1,
			y,
			"wasn't closed cleanly; opening it recovers it",
			fg(theme.warn),
			name_right,
		);
	}

	let cells = usize::from(right.saturating_sub(rows.x + 4));
	let path = display_path(&entry.recent.path);
	let (detail, style) = match &entry.state {
		EntryState::Ready(summary) => (
			format!(
				"{} → {}",
				display_path(&summary.source),
				display_path(&summary.destination)
			),
			fg(theme.text),
		),
		EntryState::Checking => (format!("checking… · {path}"), fg(theme.dim)),
		EntryState::Missing => (
			format!("the project file is missing · {path} · x removes it from the list"),
			fg(theme.dim),
		),
		EntryState::Unavailable => (format!("unavailable (drive or share offline) · {path}"), fg(theme.dim)),
		EntryState::Unreadable(reason) => (format!("{reason} · {path}"), fg(theme.warn)),
	};
	put(buf, rows.x + 2, y + 1, &fit_middle(&detail, cells), style, right);
}

fn render_form(buf: &mut Buffer, body: Rect, form: &NewProjectForm, theme: &Theme, hits: &mut HitMap<StartHit>) {
	let area = Rect {
		width: body.width.min(FORM_WIDTH),
		..body
	};
	let inner = panel(
		buf,
		area,
		theme,
		&Chrome {
			title: "New project",
			right: Some("every field is checked as you type"),
			focused: true,
		},
	);
	let layout = FormLayout {
		label_x: inner.x + 2,
		value_x: inner.x + 2 + LABEL,
		right: inner.right().saturating_sub(2),
		theme,
	};
	let validation = form.validation();
	let mut y = inner.y + 1;

	for (field, label, check) in [
		(FormField::Source, "Source folder", &validation.source),
		(FormField::Destination, "Destination folder", &validation.destination),
	] {
		layout.text_row(buf, y, form, field, label, hits);
		layout.check_row(buf, y + 1, check, form.value(field).is_empty());
		y += 2;
	}

	let free = validation
		.free_space
		.map_or_else(|| "—".to_owned(), |free| format!("{} free", bytes(free)));
	layout.label(buf, y, "Free space", false);
	put(buf, layout.value_x, y, &free, fg(theme.text), layout.right);
	y += 2;

	layout.text_row(buf, y, form, FormField::Name, "Project name", hits);
	layout.check_row(buf, y + 1, &validation.name, form.value(FormField::Name).is_empty());
	y += 2;

	layout.speed_row(buf, y, form, hits);
	y += 2;

	layout.delete_row(buf, y, form, hits);
	y += 3;

	let file = validation
		.project_file
		.as_deref()
		.map_or_else(|| "—".to_owned(), display_path);
	layout.label(buf, y, "Project file", false);
	let cells = usize::from(layout.right.saturating_sub(layout.value_x));
	put(
		buf,
		layout.value_x,
		y,
		&fit_middle(&file, cells),
		bold(theme.bright),
		layout.right,
	);
	y += 2;

	layout.buttons(buf, y, form, hits);
}

struct FormLayout<'a> {
	label_x: u16,
	value_x: u16,
	right: u16,
	theme: &'a Theme,
}

impl FormLayout<'_> {
	fn label(&self, buf: &mut Buffer, y: u16, text: &str, focused: bool) {
		let theme = self.theme;
		let style = if focused { bold(theme.accent) } else { fg(theme.text) };
		put(buf, self.label_x, y, text, style, self.value_x);
	}

	fn text_row(
		&self,
		buf: &mut Buffer,
		y: u16,
		form: &NewProjectForm,
		field: FormField,
		label: &str,
		hits: &mut HitMap<StartHit>,
	) {
		let theme = self.theme;
		let focused = form.focus() == field && form.confirming().is_none();
		self.label(buf, y, label, focused);

		let area = Rect::new(self.value_x - 1, y, self.right.saturating_sub(self.value_x - 1), 1);
		let background = if focused { theme.selection } else { theme.panel };
		buf.set_style(area, Style::new().bg(background));
		if let Some(input) = form.input(field) {
			text_field(buf, area, input, focused, theme);
		}
		hits.push(
			Rect::new(self.label_x, y, area.right() - self.label_x, 1),
			StartHit::Field(field),
		);
	}

	fn check_row(&self, buf: &mut Buffer, y: u16, check: &Check, empty: bool) {
		let theme = self.theme;
		let (text, style) = match check {
			Check::Valid => ("✓".to_owned(), fg(theme.done)),
			Check::Warning(reason) => (format!("⚠ {reason}"), fg(theme.warn)),
			Check::Pending(reason) => (format!("… {reason}"), fg(theme.dim)),
			Check::Invalid(reason) if empty => (reason.clone(), fg(theme.dim)),
			Check::Invalid(reason) => (format!("✗ {reason}"), fg(theme.error)),
		};
		let cells = usize::from(self.right.saturating_sub(self.value_x));
		put(buf, self.value_x, y, &fit(&text, cells), style, self.right);
	}

	fn speed_row(&self, buf: &mut Buffer, y: u16, form: &NewProjectForm, hits: &mut HitMap<StartHit>) {
		let theme = self.theme;
		let focused = form.focus() == FormField::Speed && form.confirming().is_none();
		self.label(buf, y, "Speed", focused);

		let mut x = self.value_x;
		for (speed, label, _) in SPEEDS {
			let chosen = form.speed() == speed;
			let text = format!("{} {label}", if chosen { "◉" } else { "○" });
			let style = match (chosen, focused) {
				(true, true) => bold(theme.background).bg(theme.accent),
				(true, false) => bold(theme.accent),
				(false, _) => fg(theme.text),
			};
			let start = x;
			x = put(buf, x, y, &text, style, self.right);
			hits.push(Rect::new(start, y, x - start, 1), StartHit::Speed(speed));
			x += 3;
		}

		let explanation = SPEEDS
			.iter()
			.find(|(speed, ..)| *speed == form.speed())
			.map_or("", |(.., explanation)| explanation);
		put(buf, self.value_x, y + 1, explanation, fg(theme.dim), self.right);
	}

	fn delete_row(&self, buf: &mut Buffer, y: u16, form: &NewProjectForm, hits: &mut HitMap<StartHit>) {
		let theme = self.theme;
		let focused = form.focus() == FormField::DeleteOriginals && form.confirming().is_none();
		self.label(buf, y, "Delete originals", focused);

		let (text, style) = if form.delete_originals() {
			(
				"[✓] ON · each original is deleted once its archived copy is verified",
				bold(theme.error),
			)
		} else {
			("[ ] off · the source is never modified", fg(theme.text))
		};
		let style = if focused { style.bg(theme.selection) } else { style };
		let end = put(buf, self.value_x, y, text, style, self.right);
		hits.push(
			Rect::new(self.label_x, y, end - self.label_x, 1),
			StartHit::Field(FormField::DeleteOriginals),
		);

		if form.delete_originals() {
			let warning = "⚠ Deleting is permanent, with no Recycle Bin. Each session still asks for DELETE first.";
			let cells = usize::from(self.right.saturating_sub(self.value_x));
			put(
				buf,
				self.value_x,
				y + 1,
				&fit(warning, cells),
				fg(theme.error),
				self.right,
			);
		}
	}

	fn buttons(&self, buf: &mut Buffer, y: u16, form: &NewProjectForm, hits: &mut HitMap<StartHit>) {
		let theme = self.theme;
		let enabled = form.can_create();
		let mut x = self.value_x;
		for (field, text) in [(FormField::Create, "[ Create ]"), (FormField::Cancel, "[ Cancel ]")] {
			let focused = form.focus() == field && form.confirming().is_none();
			let usable = enabled || field == FormField::Cancel;
			let style = match (usable, focused) {
				(true, true) => bold(theme.background).bg(theme.accent),
				(true, false) => bold(theme.accent),
				(false, true) => fg(theme.dim).bg(theme.selection),
				(false, false) => fg(theme.dim),
			};
			let start = x;
			x = put(buf, x, y, text, style, self.right);
			hits.push(Rect::new(start, y, x - start, 1), StartHit::Field(field));
			x += 2;
		}

		let (reason, style) = match form.error() {
			Some(error) => (format!("✗ {error}"), fg(theme.error)),
			None if !enabled => ("Create is ready once every check passes".to_owned(), fg(theme.dim)),
			None => (String::new(), fg(theme.dim)),
		};
		let cells = usize::from(self.right.saturating_sub(x + 1));
		put(buf, x + 1, y, &fit(&reason, cells), style, self.right);
	}
}

/// The value, scrolled so the cursor stays in view while focused; the cursor is the reversed cell.
fn text_field(buf: &mut Buffer, area: Rect, input: &Input, focused: bool, theme: &Theme) {
	let cells = usize::from(area.width.saturating_sub(2));
	let x = area.x + 1;
	let style = fg(theme.bright);

	if !focused {
		put(buf, x, area.y, &fit_middle(input.value(), cells), style, area.right());
		return;
	}

	let scroll = input.visual_scroll(cells.saturating_sub(1));
	let chars: Vec<char> = input.value().chars().collect();
	let mut start = 0;
	let mut skipped = 0;
	while skipped < scroll && start < chars.len() {
		skipped += width(chars[start].encode_utf8(&mut [0; 4]));
		start += 1;
	}

	let cursor = input.cursor().clamp(start, chars.len());
	let before: String = chars[start..cursor].iter().collect();
	let after: String = chars.get(cursor + 1..).unwrap_or_default().iter().collect();
	let under = chars.get(cursor).map_or_else(|| "█".to_owned(), char::to_string);
	let cursor_style = if cursor < chars.len() {
		style.add_modifier(Modifier::REVERSED)
	} else {
		style
	};

	let mut end = put(buf, x, area.y, &before, style, area.right());
	end = put(buf, end, area.y, &under, cursor_style, area.right());
	put(buf, end, area.y, &after, style, area.right());
}

fn render_opening(buf: &mut Buffer, screen: Rect, name: &str, theme: &Theme) {
	let area = screen.centered(
		Constraint::Length(screen.width.saturating_sub(10).min(60)),
		Constraint::Length(5),
	);
	let inner = modal(buf, area, screen, theme, "Opening");
	let cells = usize::from(inner.width.saturating_sub(4));
	put(
		buf,
		inner.x + 2,
		inner.y + 1,
		&fit(&format!("Opening {name}…"), cells),
		bold(theme.bright),
		inner.right(),
	);
	put(
		buf,
		inner.x + 2,
		inner.y + 2,
		"Checking ffmpeg and loading the project",
		fg(theme.dim),
		inner.right(),
	);
}
