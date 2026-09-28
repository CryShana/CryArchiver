mod details;
mod footer;
mod header;
mod issues;
mod library;
mod log;
mod overlay;
mod panels;
mod settings;
mod small;
pub(crate) mod start;
mod widgets;

pub use self::issues::AttemptLog;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::tree::file_row;
use crate::app::{Lifecycle, Overlay, Ui, ViewMode};
use crate::source::ArchiveSource;
use crate::view::{ArchiveView, FileDetails, Notification};

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

/// What the app fetched from the source on demand for this frame, beside the view itself.
pub struct Fetched<'a> {
	pub details: Option<&'a FileDetails>,
	pub issue_log: Option<AttemptLog<'a>>,
	pub toast: Option<&'a Notification>,
}

pub fn draw(frame: &mut Frame, source: &dyn ArchiveSource, fetched: &Fetched, ui: &mut Ui) {
	let details = fetched.details;
	let view = source.view();
	let area = frame.area();
	let buf = frame.buffer_mut();
	buf.set_style(area, ui.theme.base());
	ui.hits.clear();
	ui.issues_area = None;
	ui.spinner_drawn = false;

	if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
		small::render(buf, area, view, &ui.theme);
		return;
	}

	let areas = Areas::new(area, ui.layout.maximized, ui.layout.log_open);
	header::render(buf, areas.header, view, &ui.theme);

	if let Some(top) = areas.top {
		ui.spinner_drawn = panels::render(buf, top, view, &ui.theme, &ui.spinner);
	}

	match ui.mode {
		ViewMode::Library => library::render(buf, areas.main, source, ui),
		ViewMode::Details => {
			let name = ui
				.tree
				.selected_index()
				.and_then(|index| ui.tree.rows().get(index))
				.and_then(|row| file_row(source, row.kind).map(|file| file.name.as_str()));
			details::render(
				buf,
				areas.main,
				name.unwrap_or_default(),
				details,
				&ui.theme,
				&mut ui.hits,
			);
		}
		ViewMode::Issues => issues::render(buf, areas.main, &view.issues, fetched.issue_log.as_ref(), ui),
	}

	if let Some(log) = areas.log {
		log::render(
			buf,
			log,
			&view.log,
			fetched.toast.filter(|_| ui.layout.log_open),
			&ui.theme,
			&mut ui.hits,
		);
	}

	let log_hidden = ui.layout.maximized || !ui.layout.log_open;
	footer::render(
		buf,
		areas.footer,
		view,
		details,
		fetched.toast.filter(|_| log_hidden),
		ui,
	);
	draw_overlay(buf, area, areas.main, view, ui);

	if ui.lifecycle == Lifecycle::Stopping {
		overlay::stopping(buf, area, &ui.theme);
	}
}

fn draw_overlay(buf: &mut ratatui::buffer::Buffer, screen: Rect, main: Rect, view: &ArchiveView, ui: &mut Ui) {
	match &ui.overlay {
		Some(Overlay::Help) => overlay::help(buf, screen, &ui.theme),
		Some(Overlay::Menu(menu)) => {
			let selected_row = ui.tree.selected_index().unwrap_or(0).saturating_sub(ui.tree.scroll());
			let row = main.y + 2 + widgets::to_u16(selected_row);
			let default_anchor = (screen.x + 24, row.min(main.bottom()));
			overlay::menu(buf, screen, menu, default_anchor, &ui.theme, &mut ui.hits);
		}
		Some(Overlay::Settings(selected)) => {
			settings::render(buf, screen, view, (*selected, None), &ui.theme, &mut ui.hits);
		}
		Some(Overlay::EditQuality { row, input }) => {
			settings::render(buf, screen, view, (*row, Some(input)), &ui.theme, &mut ui.hits);
		}
		Some(Overlay::ConfirmDelete(input)) => {
			settings::confirm_delete(buf, screen, view, input, &ui.theme, &mut ui.hits);
		}
		Some(Overlay::ConfirmRestart) => settings::confirm_restart(buf, screen, view, &ui.theme, &mut ui.hits),
		None => {}
	}
}
