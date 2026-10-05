use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::scrollbar;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Table, Widget};

use super::widgets::{Chrome, bold, fg, gauge, pad_left, panel, put, to_u16};
use crate::app::tree::{self, Fold, ProgressSort, RowKind};
use crate::app::{Hit, Ui};
use crate::format::{bytes, find_ignore_case, fit, fit_middle, percent, saving, short_count, thousands, width};
use crate::source::ArchiveSource;
use crate::theme::{Theme, glyph, label};
use crate::view::{ArchiveView, FileState, FileStatus, FolderStats};

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

pub fn render(buf: &mut Buffer, area: Rect, source: &dyn ArchiveSource, ui: &mut Ui) {
	let view = source.view();
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
		let message = match ui.tree.matching_files() {
			None => "searching…".to_owned(),
			Some(_) => format!("No files match “{}”", ui.filter),
		};
		put(buf, body.x + 1, body.y, &message, fg(theme.dim), body.right());
		return;
	}

	let start = ui.tree.scroll();
	let end = (start + usize::from(body.height)).min(rows.len());
	let selected = ui.tree.selected_index();
	let table_rows: Vec<Row> = (start..end)
		.map(|index| {
			let context = RowContext {
				source,
				row: rows[index],
				selected: selected == Some(index),
				filter: &ui.filter,
				theme: &theme,
			};
			table_row(&context, columns)
		})
		.collect();

	let table = Table::new(table_rows, columns.widths())
		.header(header_row(columns, ui.tree.sort(), &theme))
		.column_spacing(0);
	Widget::render(table, inner, buf);

	let progress_x = inner.x + to_u16(columns.name + STATUS);
	ui.hits.push(
		Rect::new(progress_x, inner.y, to_u16(PROGRESS), 1).intersection(inner),
		Hit::ProgressHeader,
	);

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

fn header_row(columns: Columns, sort: ProgressSort, theme: &Theme) -> Row<'static> {
	let progress = match sort {
		ProgressSort::Off => Cell::from(fit("Progress", PROGRESS)),
		ProgressSort::Descending => Cell::from(fit("Progress ▼", PROGRESS)).style(bold(theme.accent)),
		ProgressSort::Ascending => Cell::from(fit("Progress ▲", PROGRESS)).style(bold(theme.accent)),
	};
	let mut cells = vec![
		Cell::from(fit("Name", columns.name)),
		Cell::from(fit("Status", STATUS)),
		progress,
	];
	if columns.codec {
		cells.push(Cell::from(fit("Codec", CODEC)));
	}
	if columns.size {
		cells.push(Cell::from(fit("Size", SIZE)));
	}
	if columns.saved {
		cells.push(Cell::from(fit("Saved", SAVED)));
	}
	Row::new(cells).style(bold(theme.dim))
}

struct RowContext<'a> {
	source: &'a dyn ArchiveSource,
	row: tree::Row,
	selected: bool,
	filter: &'a str,
	theme: &'a Theme,
}

fn table_row(context: &RowContext, columns: Columns) -> Row<'static> {
	let theme = context.theme;
	let mut cells = vec![Cell::from(name_cell(context, columns.name))];

	match context.row.kind {
		RowKind::Folder(folder, _) => {
			let stats = context.source.view().library.stats(folder);
			cells.extend(folder_cells(stats, columns, theme));
		}
		kind => {
			if let Some(file) = tree::file_row(context.source, kind) {
				cells.extend(file_cells(&file.status, columns, theme));
			}
		}
	}

	let row = Row::new(cells);
	if context.selected {
		row.style(Style::new().bg(theme.selection))
	} else {
		row
	}
}

struct Lead {
	text: String,
	style: Style,
	name: String,
	folder: bool,
	issues: bool,
}

fn lead(context: &RowContext) -> Lead {
	let theme = context.theme;
	match context.row.kind {
		RowKind::Folder(folder, fold) => {
			let node = context.source.view().library.node(folder);
			let arrow = match fold {
				Fold::Closed => "▸ ",
				Fold::FoldersOnly => "▿ ",
				Fold::Open => "▾ ",
			};
			Lead {
				text: arrow.to_owned(),
				style: fg(theme.dim),
				name: node.name.clone(),
				folder: true,
				issues: node.stats.open_issues > 0,
			}
		}
		RowKind::Loading(_) => Lead {
			text: "… ".to_owned(),
			style: fg(theme.dim),
			name: "loading…".to_owned(),
			folder: false,
			issues: false,
		},
		kind @ RowKind::File { .. } => {
			let file = tree::file_row(context.source, kind);
			let state = file.map_or(FileState::Queued, |file| file.status.state);
			let color = theme.state(state);
			Lead {
				text: format!("{} ", glyph(state)),
				style: if color == theme.dim { fg(color) } else { bold(color) },
				name: file.map(|file| file.name.clone()).unwrap_or_default(),
				folder: false,
				issues: false,
			}
		}
	}
}

fn name_cell(context: &RowContext, cells: usize) -> Line<'static> {
	let theme = context.theme;
	let indent = "  ".repeat(usize::from(context.row.depth));
	let lead = lead(context);

	let marker = if lead.issues { " !" } else { "" };
	let name_width = cells.saturating_sub(width(&indent) + width(&lead.text) + width(marker) + 1);

	let loading = matches!(context.row.kind, RowKind::Loading(_));
	let mut name_style = fg(match (lead.folder, loading) {
		(true, _) => theme.bright,
		(false, true) => theme.dim,
		(false, false) => theme.text,
	});
	if lead.folder || context.selected {
		name_style = name_style.add_modifier(Modifier::BOLD);
	}

	let shown = fit_middle(&lead.name, name_width);
	let mut spans = vec![Span::raw(indent), Span::styled(lead.text, lead.style)];
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
