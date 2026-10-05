use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Widget};

use super::widgets::{Chrome, bold, fg, panel, put, to_u16};
use crate::app::{Hit, Ui};
use crate::format::{fit, width};
use crate::theme::{Theme, marker_glyph};
use crate::view::{Log, NoticeLevel, Notification};

pub fn render(buf: &mut Buffer, area: Rect, log: &Log, toast: Option<&Notification>, ui: &mut Ui) {
	let theme = ui.theme;
	if area.height <= 1 {
		render_collapsed(buf, area, log, &theme);
		ui.hits.push(area, Hit::LogToggle);
		return;
	}

	let maximized = ui.layout.log_maximized;
	let title = if maximized { "Log · maximized" } else { "Log" };
	let chrome = Chrome {
		title,
		right: None,
		focused: maximized,
	};
	let inner = panel(buf, area, &theme, &chrome);
	ui.log_scroll.set_page(usize::from(inner.height), log);

	ui.hits.push(area, Hit::LogPanel);
	if !maximized {
		ui.hits.push(Rect::new(area.x, area.y, area.width, 1), Hit::LogToggle);
	}

	title_extras(buf, area, title, log, toast, ui);

	let message_width = usize::from(inner.width.saturating_sub(14));
	let shown: Vec<_> = log
		.numbered()
		.skip(ui.log_scroll.newer(log))
		.take(usize::from(inner.height))
		.collect();
	let items: Vec<ListItem> = shown
		.iter()
		.map(|(_, line)| {
			ListItem::new(Line::from(vec![
				Span::styled(format!(" {} ", line.time), fg(theme.dim)),
				Span::styled(
					format!("{} ", marker_glyph(line.marker)),
					bold(theme.marker(line.marker)),
				),
				Span::styled(fit(&line.message, message_width), fg(theme.text)),
			]))
		})
		.collect();
	List::new(items).render(inner, buf);

	for (row, &(number, _)) in shown.iter().enumerate() {
		let line_area = Rect::new(inner.x, inner.y + to_u16(row), inner.width, 1);
		ui.hits.push(line_area, Hit::LogLine(number));
	}
}

/// The maximize toggle sits where the Library has its own; the "newer" hint follows the title, and the toast or the
/// collapse hint take the room left before the toggle.
fn title_extras(buf: &mut Buffer, area: Rect, title: &str, log: &Log, toast: Option<&Notification>, ui: &mut Ui) {
	let theme = ui.theme;
	let toggle = if ui.layout.log_maximized { "[⤡]" } else { "[⤢]" };
	let toggle_x = area.right() - 5;
	buf.set_string(toggle_x, area.y, toggle, bold(theme.accent));
	ui.hits.push(Rect::new(toggle_x, area.y, 3, 1), Hit::LogMaximize);

	let mut left = area.x + 2 + to_u16(width(title) + 2);
	let newer = ui.log_scroll.newer(log);
	if newer > 0 {
		let hint = format!(" ↑ {newer} newer ");
		let end = put(buf, left + 1, area.y, &hint, bold(theme.accent), toggle_x - 1);
		ui.hits.push(
			Rect::new(left + 1, area.y, end.saturating_sub(left + 1), 1),
			Hit::LogNewest,
		);
		left = end;
	}

	let room = usize::from(toggle_x.saturating_sub(left + 2));
	let right = match toast {
		Some(toast) => {
			let text = fit(&format!(" ◆ {}", toast.text), room.saturating_sub(1));
			Some((format!("{} ", text.trim_end()), bold(toast_color(toast.level, &theme))))
		}
		None if !ui.layout.log_maximized => Some((" l to collapse ".to_owned(), fg(theme.dim))),
		None => None,
	};

	if let Some((text, style)) = right.filter(|(text, _)| width(text) <= room) {
		let x = toggle_x - 1 - to_u16(width(&text));
		put(buf, x, area.y, &text, style, toggle_x - 1);
	}
}

pub fn toast_color(level: NoticeLevel, theme: &Theme) -> Color {
	match level {
		NoticeLevel::Info => theme.accent,
		NoticeLevel::Warning => theme.warn,
		NoticeLevel::Error => theme.error,
	}
}

fn render_collapsed(buf: &mut Buffer, area: Rect, log: &Log, theme: &Theme) {
	buf.set_style(area, Style::new().bg(theme.panel));
	let (y, right) = (area.y, area.right());
	let hint = "l to expand ";
	let hint_x = right.saturating_sub(to_u16(hint.len()) + 2);

	let mut x = put(buf, area.x + 1, y, "▸ Log", bold(theme.bright), hint_x);
	if let Some(latest) = log.latest() {
		x = put(
			buf,
			x + 2,
			y,
			marker_glyph(latest.marker),
			fg(theme.marker(latest.marker)),
			hint_x,
		);
		let room = usize::from(hint_x.saturating_sub(x + 3));
		put(buf, x + 1, y, &fit(&latest.message, room), fg(theme.dim), hint_x);
	}

	put(buf, hint_x, y, hint, fg(theme.dim), right);
}
