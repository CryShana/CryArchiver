use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Widget};

use super::widgets::{Chrome, bold, fg, panel, put, to_u16};
use crate::app::{Hit, HitMap};
use crate::format::{fit, width};
use crate::theme::{Theme, marker_glyph};
use crate::view::{Log, NoticeLevel, Notification};

pub fn render(buf: &mut Buffer, area: Rect, log: &Log, toast: Option<&Notification>, theme: &Theme, hits: &mut HitMap) {
	if area.height <= 1 {
		render_collapsed(buf, area, log, theme);
		hits.push(area, Hit::LogToggle);
		return;
	}

	let chrome = Chrome {
		title: "Log",
		right: toast.is_none().then_some(" l to collapse "),
		focused: false,
	};
	let inner = panel(buf, area, theme, &chrome);
	hits.push(Rect::new(area.x, area.y, area.width, 1), Hit::LogToggle);

	if let Some(toast) = toast {
		let text = fit(
			&format!(" ◆ {} ", toast.text),
			usize::from(area.width.saturating_sub(12)),
		);
		let x = area.right().saturating_sub(2 + to_u16(width(text.trim_end()) + 1));
		put(
			buf,
			x,
			area.y,
			&format!("{} ", text.trim_end()),
			bold(toast_color(toast.level, theme)),
			area.right() - 1,
		);
	}

	let message_width = usize::from(inner.width.saturating_sub(14));
	let items: Vec<ListItem> = log
		.newest_first()
		.take(usize::from(inner.height))
		.map(|line| {
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
