mod details;
mod footer;
mod header;
mod issues;
mod library;
mod log;
mod overlay;
mod panels;
mod small;
mod widgets;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::{Overlay, Ui, ViewMode};
use crate::view::{ArchiveView, FileDetails};

pub const MIN_WIDTH: u16 = 100;
pub const MIN_HEIGHT: u16 = 30;
const TOP_HEIGHT: u16 = 9;
const LOG_HEIGHT: u16 = 6;

struct Areas {
	header: Rect,
	top: Option<Rect>,
	main: Rect,
	log: Option<Rect>,
	footer: Rect,
}

impl Areas {
	fn new(area: Rect, maximized: bool, log_open: bool) -> Self {
		let header = Rect { height: 1, ..area };
		let footer = Rect {
			y: area.bottom() - 1,
			height: 1,
			..area
		};
		let between = Rect {
			y: area.y + 1,
			height: area.height - 2,
			..area
		};

		if maximized {
			return Self {
				header,
				top: None,
				main: between,
				log: None,
				footer,
			};
		}

		let log_height = if log_open { LOG_HEIGHT } else { 1 };
		let top = Rect {
			height: TOP_HEIGHT,
			..between
		};
		let log = Rect {
			y: between.bottom() - log_height,
			height: log_height,
			..between
		};
		let main = Rect {
			y: top.bottom(),
			height: between.height - TOP_HEIGHT - log_height,
			..between
		};

		Self {
			header,
			top: Some(top),
			main,
			log: Some(log),
			footer,
		}
	}
}

pub fn draw(frame: &mut Frame, view: &ArchiveView, details: Option<&FileDetails>, ui: &mut Ui) {
	let area = frame.area();
	let buf = frame.buffer_mut();
	buf.set_style(area, ui.theme.base());
	ui.hits.clear();
	ui.issues_area = None;

	if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
		small::render(buf, area, view, &ui.theme);
		return;
	}

	let areas = Areas::new(area, ui.layout.maximized, ui.layout.log_open);
	header::render(buf, areas.header, view, &ui.theme);

	if let Some(top) = areas.top {
		panels::render(buf, top, view, &ui.theme, &ui.spinner);
	}

	match ui.mode {
		ViewMode::Library => library::render(buf, areas.main, view, ui),
		ViewMode::Details => {
			let name = ui
				.tree
				.selected()
				.map(|id| view.library.node(id).name.as_str())
				.unwrap_or_default();
			details::render(buf, areas.main, name, details, &ui.theme, &mut ui.hits);
		}
		ViewMode::Issues => issues::render(buf, areas.main, &view.issues, ui),
	}

	if let Some(log) = areas.log {
		log::render(buf, log, &view.log, &ui.theme, &mut ui.hits);
	}

	footer::render(buf, areas.footer, view, details, ui);
	draw_overlay(buf, area, areas.main, ui);
}

fn draw_overlay(buf: &mut ratatui::buffer::Buffer, screen: Rect, main: Rect, ui: &mut Ui) {
	match &ui.overlay {
		Some(Overlay::Help) => overlay::help(buf, screen, &ui.theme),
		Some(Overlay::Menu(menu)) => {
			let selected_row = ui.tree.selected_index().unwrap_or(0).saturating_sub(ui.tree.scroll());
			let row = main.y + 2 + widgets::to_u16(selected_row);
			let default_anchor = (screen.x + 24, row.min(main.bottom()));
			overlay::menu(buf, screen, menu, default_anchor, &ui.theme, &mut ui.hits);
		}
		None => {}
	}
}
