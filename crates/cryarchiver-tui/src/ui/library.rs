use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::scrollbar;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Table, Widget};

use super::widgets::{Chrome, bold, fg, gauge, pad_left, panel, put, to_u16};
use crate::app::tree;
use crate::app::{Hit, Ui};
use crate::format::{bytes, find_ignore_case, fit, fit_middle, percent, saving, short_count, thousands, width};
use crate::theme::{Theme, glyph, label};
use crate::view::{ArchiveView, Entry, FileStatus, FolderStats, Library};

const STATUS: usize = 11;
const PROGRESS: usize = 15;
const CODEC: usize = 6;
const SIZE: usize = 19;
const SAVED: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Columns {
	pub name: usize,
	pub codec: bool,
	pub size: bool,
	pub saved: bool,
}

impl Columns {
	pub fn for_width(inner: usize) -> Self {
		let codec = inner >= 80;
		let size = inner >= 100;
		let saved = inner >= 64;
		let fixed =
			STATUS + PROGRESS + usize::from(codec) * CODEC + usize::from(size) * SIZE + usize::from(saved) * SAVED;

		Self {
			name: inner.saturating_sub(fixed),
			codec,
			size,
			saved,
		}
	}

	fn widths(self) -> Vec<Constraint> {
		let mut widths = vec![to_length(self.name), to_length(STATUS), to_length(PROGRESS)];
		if self.codec {
			widths.push(to_length(CODEC));
		}
		if self.size {
			widths.push(to_length(SIZE));
		}
		if self.saved {
			widths.push(to_length(SAVED));
		}
		widths
	}
}

fn to_length(cells: usize) -> Constraint {
	Constraint::Length(to_u16(cells))
}

pub fn render(buf: &mut Buffer, area: Rect, view: &ArchiveView, ui: &mut Ui) {
	let theme = ui.theme;
	let title = library_title(ui);
	let chrome = Chrome {
		title: &title,
		right: None,
		focused: true,
	};
	let inner = panel(buf, area, &theme, &chrome);
	title_extras(buf, area, &title, view, ui);

	let body = Rect {
		y: inner.y + 1,
		height: inner.height.saturating_sub(1),
		..inner
	};
	ui.tree.set_page(usize::from(body.height));

	let columns = Columns::for_width(usize::from(inner.width));
	let rows = ui.tree.rows();
	if rows.is_empty() {
		let message = format!("No files match “{}”", ui.filter);
		put(buf, body.x + 1, body.y, &message, fg(theme.dim), body.right());
		return;
	}

	let start = ui.tree.scroll();
	let end = (start + usize::from(body.height)).min(rows.len());
	let selected = ui.tree.selected_index();
	let table_rows: Vec<Row> = (start..end)
		.map(|index| {
			let context = RowContext {
				library: &view.library,
				row: rows[index],
				selected: selected == Some(index),
				filter: &ui.filter,
				theme: &theme,
			};
			table_row(&context, columns)
		})
		.collect();

	let table = Table::new(table_rows, columns.widths())
		.header(header_row(columns, &theme))
		.column_spacing(0);
	Widget::render(table, inner, buf);

	for (offset, index) in (start..end).enumerate() {
		let row_area = Rect::new(body.x, body.y + to_u16(offset), body.width, 1);
		let arrow_x = body.x + rows[index].depth * 2;
		ui.hits.push(row_area, Hit::Row { index, arrow_x });
	}

	if rows.len() > usize::from(body.height) {
		scrollbar(buf, area, body, rows.len(), start, &theme);
	}
}

fn library_title(ui: &Ui) -> String {
	let mut title = if ui.layout.maximized {
		"Library · maximized".to_owned()
	} else {
		"Library".to_owned()
	};
	if !ui.filter.is_empty() {
		title = format!("{title} · filter “{}”", ui.filter);
	}
	title
}

fn title_extras(buf: &mut Buffer, area: Rect, title: &str, view: &ArchiveView, ui: &mut Ui) {
	let theme = ui.theme;
	let plus = if view.overall.scan_complete { "" } else { "+" };
	let count = format!(" {}{plus} files ", thousands(view.overall.files_total));
	if width(title) + width(&count) + 12 < usize::from(area.width) {
		let x = area.right() - 5 - to_u16(width(&count));
		buf.set_string(x, area.y, &count, fg(theme.dim));
	}

	let toggle = if ui.layout.maximized { "[⤡]" } else { "[⤢]" };
	let toggle_x = area.right() - 5;
	buf.set_string(toggle_x, area.y, toggle, bold(theme.accent));

	ui.hits
		.push(Rect::new(area.x, area.y, area.width, 1), Hit::LibraryTitle);
	ui.hits.push(Rect::new(toggle_x, area.y, 3, 1), Hit::Maximize);
}

fn header_row(columns: Columns, theme: &Theme) -> Row<'static> {
	let mut labels = vec![
		fit("Name", columns.name),
		fit("Status", STATUS),
		fit("Progress", PROGRESS),
	];
	if columns.codec {
		labels.push(fit("Codec", CODEC));
	}
	if columns.size {
		labels.push(fit("Size", SIZE));
	}
	if columns.saved {
		labels.push(fit("Saved", SAVED));
	}
	Row::new(labels).style(bold(theme.dim))
}

struct RowContext<'a> {
	library: &'a Library,
	row: tree::Row,
	selected: bool,
	filter: &'a str,
	theme: &'a Theme,
}

fn table_row(context: &RowContext, columns: Columns) -> Row<'static> {
	let node = context.library.node(context.row.id);
	let mut cells = vec![Cell::from(name_cell(context, columns.name))];

	match &node.entry {
		Entry::Folder(stats) => cells.extend(folder_cells(stats, columns, context.theme)),
		Entry::File(status) => cells.extend(file_cells(status, columns, context.theme)),
	}

	let row = Row::new(cells);
	if context.selected {
		row.style(Style::new().bg(context.theme.selection))
	} else {
		row
	}
}

fn name_cell(context: &RowContext, cells: usize) -> Line<'static> {
	let theme = context.theme;
	let node = context.library.node(context.row.id);
	let indent = "  ".repeat(usize::from(context.row.depth));

	let (lead, lead_style) = match &node.entry {
		Entry::Folder(_) => (if context.row.open { "▾ " } else { "▸ " }.to_owned(), fg(theme.dim)),
		Entry::File(status) => {
			let color = theme.state(status.state);
			let style = if color == theme.dim { fg(color) } else { bold(color) };
			(format!("{} ", glyph(status.state)), style)
		}
	};

	let folder = matches!(node.entry, Entry::Folder(_));
	let has_issues = matches!(node.entry, Entry::Folder(stats) if stats.open_issues > 0);
	let marker = if has_issues { " !" } else { "" };
	let name_width = cells.saturating_sub(width(&indent) + width(&lead) + width(marker) + 1);

	let mut name_style = fg(if folder { theme.bright } else { theme.text });
	if folder || context.selected {
		name_style = name_style.add_modifier(Modifier::BOLD);
	}

	let shown = fit_middle(&node.name, name_width);
	let mut spans = vec![Span::raw(indent), Span::styled(lead, lead_style)];
	spans.extend(highlighted(shown.trim_end(), context.filter, name_style, theme));
	spans.push(Span::styled(marker, bold(theme.error)));
	Line::from(spans)
}

fn highlighted(shown: &str, filter: &str, style: Style, theme: &Theme) -> Vec<Span<'static>> {
	let Some(range) = find_ignore_case(shown, filter) else {
		return vec![Span::styled(shown.to_owned(), style)];
	};

	let matched = bold(theme.warn).add_modifier(Modifier::UNDERLINED);
	vec![
		Span::styled(shown[..range.start].to_owned(), style),
		Span::styled(shown[range.clone()].to_owned(), matched),
		Span::styled(shown[range.end..].to_owned(), style),
	]
}

fn folder_cells(stats: &FolderStats, columns: Columns, theme: &Theme) -> Vec<Cell<'static>> {
	let counts = format!("{}/{}", short_count(stats.files_done), short_count(stats.files_total));
	let counts_cell = Line::styled(fit(&counts, STATUS), fg(theme.dim));

	let gauge_color = if stats.complete { theme.done } else { theme.accent };
	let mut progress = gauge(PROGRESS - 7, stats.progress, gauge_color, theme);
	progress.push(Span::styled(
		format!(" {} ", percent(stats.progress, stats.complete)),
		fg(theme.text),
	));

	let mut cells = vec![Cell::from(counts_cell), Cell::from(Line::from(progress))];
	if columns.codec {
		cells.push(Cell::from(""));
	}
	if columns.size {
		let size = format!("{} → {}", bytes(stats.bytes_in), bytes(stats.bytes_out));
		cells.push(Cell::from(Line::styled(fit(&size, SIZE), fg(theme.dim))));
	}
	if columns.saved {
		let saved = saving(stats.bytes_in, stats.bytes_out);
		let color = if stats.bytes_out < stats.bytes_in {
			theme.done
		} else {
			theme.dim
		};
		cells.push(Cell::from(Line::styled(pad_left(&saved, SAVED - 1), fg(color))));
	}
	cells
}

fn file_cells(status: &FileStatus, columns: Columns, theme: &Theme) -> Vec<Cell<'static>> {
	let color = theme.state(status.state);
	let mut cells = vec![
		Cell::from(Line::styled(fit(label(status.state), STATUS), fg(color))),
		Cell::from(file_progress(status, theme)),
	];

	if columns.codec {
		let codec = status.codec.as_deref().unwrap_or("");
		cells.push(Cell::from(Line::styled(fit(codec, CODEC), fg(theme.dim))));
	}
	if columns.size {
		let size = match status.bytes_out {
			Some(out) => format!("{} → {}", bytes(status.bytes_in), bytes(out)),
			None => bytes(status.bytes_in),
		};
		cells.push(Cell::from(Line::styled(fit(&size, SIZE), fg(theme.dim))));
	}
	if columns.saved {
		let saved = status
			.bytes_out
			.map(|out| saving(status.bytes_in, out))
			.unwrap_or_default();
		cells.push(Cell::from(Line::styled(pad_left(&saved, SAVED - 1), fg(theme.done))));
	}
	cells
}

fn file_progress(status: &FileStatus, theme: &Theme) -> Line<'static> {
	let color = theme.state(status.state);
	match (status.state, status.progress) {
		(state, Some(fraction)) if state.is_active() => {
			let mut spans = gauge(PROGRESS - 7, fraction, color, theme);
			spans.push(Span::styled(format!(" {} ", percent(fraction, false)), fg(theme.text)));
			Line::from(spans)
		}
		(crate::view::FileState::QualitySearch, _) => Line::styled(fit("probing CRF…", PROGRESS), fg(theme.search)),
		(crate::view::FileState::Done, _) => {
			let mut spans = gauge(PROGRESS - 7, 1.0, theme.done, theme);
			spans.push(Span::styled(format!(" {} ", percent(1.0, true)), fg(theme.text)));
			Line::from(spans)
		}
		(crate::view::FileState::Queued, _) => Line::raw(""),
		_ => Line::styled(fit("—", PROGRESS), fg(theme.dim)),
	}
}

fn scrollbar(buf: &mut Buffer, area: Rect, body: Rect, rows: usize, scroll: usize, theme: &Theme) {
	let track = Rect::new(area.right() - 1, body.y, 1, body.height);
	let mut state = ScrollbarState::new(rows.saturating_sub(usize::from(body.height)))
		.position(scroll)
		.viewport_content_length(usize::from(body.height));

	Scrollbar::new(ScrollbarOrientation::VerticalRight)
		.symbols(scrollbar::Set {
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
}
