use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::{Outcome, Overlay, Ui, ViewMode};
use crate::source::ArchiveSource;
use crate::view::NodeId;

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const WHEEL_ROWS: isize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
	Footer(KeyCode),
	Row { index: usize, arrow_x: u16 },
	LibraryTitle,
	Maximize,
	LogToggle,
	Issue(usize),
	Menu,
	MenuItem(usize),
	Link(NodeId),
}

/// Clickable regions of the frame that is on screen. Later regions sit on top of earlier ones.
#[derive(Debug, Default)]
pub struct HitMap(Vec<(Rect, Hit)>);

impl HitMap {
	pub fn clear(&mut self) {
		self.0.clear();
	}

	pub fn push(&mut self, area: Rect, hit: Hit) {
		self.0.push((area, hit));
	}

	pub fn at(&self, position: Position) -> Option<Hit> {
		self.0
			.iter()
			.rev()
			.find(|(area, _)| area.contains(position))
			.map(|&(_, hit)| hit)
	}
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Click {
	position: Position,
	at: Instant,
}

impl Ui {
	pub(super) fn on_mouse(&mut self, source: &dyn ArchiveSource, mouse: MouseEvent, now: Instant) -> Outcome {
		let position = Position::new(mouse.column, mouse.row);
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left) => {
				let double = self.register_click(position, now);
				self.on_click(source, position, double)
			}
			MouseEventKind::Down(MouseButton::Right) => self.on_right_click(source, position),
			MouseEventKind::ScrollDown => self.on_wheel(source, position, WHEEL_ROWS),
			MouseEventKind::ScrollUp => self.on_wheel(source, position, -WHEEL_ROWS),
			_ => Outcome::Ignored,
		}
	}

	fn register_click(&mut self, position: Position, now: Instant) -> bool {
		let double = self
			.last_click
			.is_some_and(|last| last.position == position && now.duration_since(last.at) <= DOUBLE_CLICK);

		self.last_click = if double {
			None
		} else {
			Some(Click { position, at: now })
		};
		double
	}

	fn on_click(&mut self, source: &dyn ArchiveSource, position: Position, double: bool) -> Outcome {
		let hit = self.hits.at(position);

		match &self.overlay {
			Some(Overlay::Menu(menu)) => {
				let (items, target) = (menu.items.clone(), menu.target);
				return match hit {
					Some(Hit::MenuItem(index)) => {
						self.overlay = None;
						self.activate(source, items[index], target)
					}
					Some(Hit::Menu) => Outcome::Ignored,
					_ => {
						self.overlay = None;
						Outcome::Done
					}
				};
			}
			Some(Overlay::Help) => {
				self.overlay = None;
				return Outcome::Done;
			}
			None => {}
		}

		let Some(hit) = hit else {
			return Outcome::Ignored;
		};
		self.on_hit(source, hit, position, double)
	}

	fn on_hit(&mut self, source: &dyn ArchiveSource, hit: Hit, position: Position, double: bool) -> Outcome {
		match hit {
			Hit::Footer(code) => return self.on_key(source, KeyEvent::new(code, KeyModifiers::NONE)),
			Hit::Row { index, arrow_x } => self.on_row_click(source, index, arrow_x, position, double),
			Hit::LibraryTitle if double => self.layout.maximized = !self.layout.maximized,
			Hit::Maximize => self.layout.maximized = !self.layout.maximized,
			Hit::LogToggle => self.layout.log_open = !self.layout.log_open,
			Hit::Issue(index) => {
				self.issue_selected = index;
				if double && let Some(issue) = source.view().issues.get(index) {
					self.open_details(source, issue.file);
				}
			}
			Hit::Link(file) => {
				self.reveal(source, file);
				self.mode = ViewMode::Library;
			}
			Hit::LibraryTitle | Hit::Menu | Hit::MenuItem(_) => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn on_row_click(
		&mut self,
		source: &dyn ArchiveSource,
		index: usize,
		arrow_x: u16,
		position: Position,
		double: bool,
	) {
		self.tree.select_index(index);
		let Some(selected) = self.tree.selected() else { return };
		let folder = source.view().library.is_folder(selected);

		let on_arrow = (arrow_x..arrow_x + 2).contains(&position.x);
		if folder && (on_arrow || double) {
			self.tree.toggle(selected);
		} else if double {
			self.mode = ViewMode::Details;
		}
	}

	fn on_right_click(&mut self, source: &dyn ArchiveSource, position: Position) -> Outcome {
		self.overlay = None;
		match self.hits.at(position) {
			Some(Hit::Row { index, .. }) => self.tree.select_index(index),
			Some(Hit::Issue(index)) => self.issue_selected = index,
			_ => {}
		}

		self.open_menu(source, Some((position.x, position.y)))
	}

	fn on_wheel(&mut self, source: &dyn ArchiveSource, position: Position, rows: isize) -> Outcome {
		if self.overlay.is_some() {
			return Outcome::Ignored;
		}

		if self.issues_area.is_some_and(|area| area.contains(position)) {
			let step = rows.signum();
			let last = source.view().issues.len().saturating_sub(1);
			self.issue_selected = self.issue_selected.saturating_add_signed(step).min(last);
			return Outcome::Done;
		}

		if self.mode == ViewMode::Library {
			self.tree.scroll_by(rows);
			return Outcome::Done;
		}
		Outcome::Ignored
	}
}
