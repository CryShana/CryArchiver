use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
	Block, BorderType, Clear, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget,
};

use crate::format::{filled_cells, filled_eighths, fit, width};
use crate::theme::Theme;
use crate::view::{IssueVerbs, Verb};

const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// The keys that resolve issues. They stay the same for every kind; only the labels change.
pub const VERB_KEYS: [(Verb, char); 3] = [(Verb::Retry, 'r'), (Verb::Accept, 'c'), (Verb::Exclude, 'x')];

/// `r Retry · c Copy original · x Exclude`, limited to the verbs the issue allows.
pub fn verb_spans(verbs: &IssueVerbs, theme: &Theme) -> Vec<Span<'static>> {
	let mut spans = Vec::new();
	for (verb, key) in VERB_KEYS {
		let Some(label) = verbs.label(verb) else { continue };
		if !spans.is_empty() {
			spans.push(Span::styled(" · ", fg(theme.dim)));
		}
		spans.push(Span::styled(key.to_string(), bold(theme.accent)));
		spans.push(Span::styled(format!(" {label}"), fg(theme.dim)));
	}
	spans
}

pub fn verb_hint(verbs: &IssueVerbs) -> String {
	let parts: Vec<String> = VERB_KEYS
		.iter()
		.filter_map(|&(verb, key)| verbs.label(verb).map(|label| format!("{key} {label}")))
		.collect();
	format!(" {} ", parts.join(" · "))
}

pub fn fg(color: Color) -> Style {
	Style::new().fg(color)
}

pub fn bold(color: Color) -> Style {
	fg(color).add_modifier(Modifier::BOLD)
}

pub struct Chrome<'a> {
	pub title: &'a str,
	pub right: Option<&'a str>,
	pub focused: bool,
}

/// A rounded panel with its title one cell in from the corner. Returns the area inside the border.
pub fn panel(buf: &mut Buffer, area: Rect, theme: &Theme, chrome: &Chrome) -> Rect {
	let accent = if chrome.focused { theme.accent } else { theme.border };
	Block::bordered()
		.border_type(BorderType::Rounded)
		.border_style(fg(accent))
		.render(area, buf);

	let title_width = width(chrome.title);
	if title_width > 0 && area.width > 6 {
		let shown = fit(
			&format!(" {} ", chrome.title),
			(title_width + 2).min(usize::from(area.width) - 4),
		);
		let color = if chrome.focused { theme.accent } else { theme.bright };
		buf.set_string(area.x + 2, area.y, shown, bold(color));
	}

	if let Some(right) = chrome.right {
		let right_width = width(right);
		if right_width + title_width + 7 < usize::from(area.width) {
			let x = area.right() - 2 - to_u16(right_width);
			buf.set_string(x, area.y, right, fg(theme.dim));
		}
	}

	Block::bordered().inner(area)
}

/// Clears the area, paints it with the panel colour and draws the one-cell shadow below and to the right.
pub fn modal(buf: &mut Buffer, area: Rect, screen: Rect, theme: &Theme, title: &str) -> Rect {
	Clear.render(area, buf);
	buf.set_style(area, Style::new().bg(theme.panel).fg(theme.text));

	let shadow = Style::new().bg(theme.border);
	for y in area.y + 1..=area.bottom() {
		if area.right() < screen.right() && y < screen.bottom() {
			buf[(area.right(), y)].set_symbol(" ").set_style(shadow);
		}
	}
	for x in area.x + 1..=area.right() {
		if x < screen.right() && area.bottom() < screen.bottom() {
			buf[(x, area.bottom())].set_symbol(" ").set_style(shadow);
		}
	}

	panel(
		buf,
		area,
		theme,
		&Chrome {
			title,
			right: None,
			focused: true,
		},
	)
}

pub fn gauge(cells: usize, fraction: f64, color: Color, theme: &Theme) -> Vec<Span<'static>> {
	let filled = filled_cells(cells, fraction);
	vec![
		Span::styled("━".repeat(filled), fg(color)),
		Span::styled("─".repeat(cells - filled), fg(theme.border)),
	]
}

pub fn block_gauge(cells: usize, fraction: f64, color: Color, theme: &Theme) -> Span<'static> {
	let eighths = filled_eighths(cells, fraction);
	let (full, partial) = (eighths / 8, EIGHTHS[eighths % 8]);
	let mut bar = "█".repeat(full);
	bar.push_str(partial);
	bar.extend(std::iter::repeat_n(
		' ',
		cells - full - usize::from(!partial.is_empty()),
	));
	Span::styled(bar, Style::new().fg(color).bg(theme.panel))
}

/// Builds a line from parts, dropping every part from the first one that doesn't fit onward.
pub struct Parts<'a> {
	spans: Vec<Span<'a>>,
	used: usize,
	max: usize,
	full: bool,
}

impl<'a> Parts<'a> {
	pub fn new(max: usize) -> Self {
		Self {
			spans: Vec::new(),
			used: 0,
			max,
			full: false,
		}
	}

	pub fn push(&mut self, text: impl Into<Cow<'a, str>>, style: Style) -> &mut Self {
		self.group([(text.into(), style)])
	}

	/// Adds the pieces together or not at all, so a glyph never stays behind without its label.
	pub fn group<const N: usize>(&mut self, pieces: [(Cow<'a, str>, Style); N]) -> &mut Self {
		let group_width: usize = pieces.iter().map(|(text, _)| width(text)).sum();
		if self.full || self.used + group_width > self.max {
			self.full = true;
			return self;
		}

		self.used += group_width;
		self.spans
			.extend(pieces.into_iter().map(|(text, style)| Span::styled(text, style)));
		self
	}

	pub fn line(self) -> Line<'a> {
		Line::from(self.spans)
	}
}

/// Greedy word wrap by display width. A word wider than `cells` gets a line of its own.
pub fn wrap(text: &str, cells: usize) -> Vec<String> {
	let mut lines = Vec::new();
	let mut current = String::new();
	for word in text.split(' ').filter(|word| !word.is_empty()) {
		if !current.is_empty() && width(&current) + 1 + width(word) > cells {
			lines.push(std::mem::take(&mut current));
		}

		if !current.is_empty() {
			current.push(' ');
		}
		current.push_str(word);
	}

	if !current.is_empty() {
		lines.push(current);
	}
	lines
}

pub fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, right: u16) -> u16 {
	if x >= right {
		return x;
	}

	buf.set_stringn(x, y, text, usize::from(right - x), style).0
}

pub fn put_line(buf: &mut Buffer, x: u16, y: u16, line: &Line, right: u16) -> u16 {
	if x >= right {
		return x;
	}

	buf.set_line(x, y, line, right - x).0
}

pub fn to_u16(value: usize) -> u16 {
	u16::try_from(value).unwrap_or(u16::MAX)
}

pub fn pad_left(text: &str, cells: usize) -> String {
	let mut out = " ".repeat(cells.saturating_sub(width(text)));
	out.push_str(text);
	out
}

/// Draws a vertical scrollbar over the panel's right border, beside `body`, and returns its track.
pub fn scrollbar(buf: &mut Buffer, area: Rect, body: Rect, rows: usize, scroll: usize, theme: &Theme) -> Rect {
	let track = Rect::new(area.right() - 1, body.y, 1, body.height);
	let mut state = ScrollbarState::new(rows.saturating_sub(usize::from(body.height)))
		.position(scroll)
		.viewport_content_length(usize::from(body.height));

	Scrollbar::new(ScrollbarOrientation::VerticalRight)
		.symbols(symbols::scrollbar::Set {
			track: "│",
			thumb: "┃",
			begin: "",
			end: "",
		})
		.begin_symbol(None)
		.end_symbol(None)
		.track_style(fg(theme.border))
		.thumb_style(fg(theme.accent))
		.render(track, buf, &mut state);
	track
}
