use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::tree::Selection;
use super::{Outcome, Overlay, Ui, ViewMode};
use crate::source::ArchiveSource;
use crate::view::{FileId, NodeId};

pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const WHEEL_ROWS: isize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
	Footer(KeyCode),
	Row { index: usize, arrow_x: u16 },
	LibraryTitle,
	Maximize,
	LogToggle,
	LogMaximize,
	LogNewest,
	LogLine(u64),
	LogPanel,
	Issue(usize),
	Menu,
	MenuItem(usize),
	Link { file: FileId, folder: NodeId },
	Modal,
	SettingsRow(usize),
}

impl Hit {
	fn on_log(self) -> bool {
		matches!(
			self,
			Self::LogToggle | Self::LogMaximize | Self::LogNewest | Self::LogLine(_) | Self::LogPanel
		)
	}
}

/// Clickable regions of the frame that is on screen. Later regions sit on top of earlier ones.
#[derive(Debug)]
pub struct HitMap<H = Hit>(Vec<(Rect, H)>);

impl<H> Default for HitMap<H> {
	fn default() -> Self {
		Self(Vec::new())
	}
}

impl<H: Copy> HitMap<H> {
	pub fn clear(&mut self) {
		self.0.clear();
	}

	pub fn push(&mut self, area: Rect, hit: H) {
		self.0.push((area, hit));
	}

	pub fn at(&self, position: Position) -> Option<H> {
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
						self.activate(source, items[index].0, target)
					}
					Some(Hit::Menu) => Outcome::Ignored,
					_ => {
						self.overlay = None;
						Outcome::Done
					}
				};
			}
			Some(Overlay::Settings(_)) => {
				return match hit {
					Some(Hit::SettingsRow(index)) => {
						self.overlay = Some(Overlay::Settings(index));
						Outcome::Done
					}
					Some(Hit::Footer(code)) => self.on_key(source, KeyEvent::new(code, KeyModifiers::NONE), false),
					Some(Hit::Modal) => Outcome::Ignored,
					_ => {
						self.overlay = None;
						Outcome::Done
					}
				};
			}
			Some(Overlay::ConfirmDelete(_) | Overlay::ConfirmRestart | Overlay::EditQuality { .. }) => {
				return Outcome::Ignored;
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
			Hit::Footer(code) => return self.on_key(source, KeyEvent::new(code, KeyModifiers::NONE), false),
			Hit::Row { index, arrow_x } => self.on_row_click(index, arrow_x, position, double),
			Hit::LibraryTitle if double => self.layout.maximized = !self.layout.maximized,
			Hit::Maximize => self.layout.maximized = !self.layout.maximized,
			Hit::LogToggle => self.layout.log_open = !self.layout.log_open,
			Hit::LogMaximize => {
				self.layout.log_maximized = !self.layout.log_maximized;
				self.layout.maximized = false;
			}
			Hit::LogNewest => self.log_scroll.jump_to_newest(),
			Hit::Issue(index) => {
				self.select_issue(index);
				let located = source
					.view()
					.issues
					.get(index)
					.and_then(|issue| issue.file.zip(issue.folder));
				if double && let Some((file, folder)) = located {
					self.open_details(source, folder, file);
				}
			}
			Hit::Link { file, folder } => {
				self.reveal_file(source, folder, file);
				self.mode = ViewMode::Library;
			}
			Hit::LibraryTitle
			| Hit::Menu
			| Hit::MenuItem(_)
			| Hit::Modal
			| Hit::SettingsRow(_)
			| Hit::LogLine(_)
			| Hit::LogPanel => {
				return Outcome::Ignored;
			}
		}
		Outcome::Done
	}

	fn on_row_click(&mut self, index: usize, arrow_x: u16, position: Position, double: bool) {
		self.tree.select_index(index);
		let on_arrow = (arrow_x..arrow_x + 2).contains(&position.x);

		match self.tree.selected() {
			Some(Selection::Folder(folder)) if (on_arrow || double) && self.filter.is_empty() => {
				self.tree.toggle(folder);
			}
			Some(Selection::File { .. }) if double => self.mode = ViewMode::Details,
			_ => {}
		}
	}

	fn on_right_click(&mut self, source: &dyn ArchiveSource, position: Position) -> Outcome {
		if matches!(self.overlay, Some(Overlay::ConfirmDelete(_) | Overlay::ConfirmRestart)) {
			return Outcome::Ignored;
		}

		self.overlay = None;
		match self.hits.at(position) {
			Some(Hit::LogLine(number)) => return self.copy_log_line(source, number),
			Some(hit) if hit.on_log() => return Outcome::Done,
			_ if self.layout.log_maximized => return Outcome::Done,
			Some(Hit::Row { index, .. }) => self.tree.select_index(index),
			Some(Hit::Issue(index)) => self.select_issue(index),
			_ => {}
		}

		self.open_menu(source, Some((position.x, position.y)))
	}

	fn copy_log_line(&mut self, source: &dyn ArchiveSource, number: u64) -> Outcome {
		let Some(line) = source.view().log.by_number(number) else {
			return Outcome::Done;
		};

		self.copied = Some(format!("{} {}", line.time, line.message));
		Outcome::Done
	}

	fn on_wheel(&mut self, source: &dyn ArchiveSource, position: Position, rows: isize) -> Outcome {
		if self.overlay.is_some() {
			return Outcome::Ignored;
		}

		if self.hits.at(position).is_some_and(Hit::on_log) {
			if !self.layout.log_open && !self.layout.log_maximized {
				return Outcome::Ignored;
			}

			self.log_scroll.scroll_by(&source.view().log, rows);
			return Outcome::Done;
		}

		if self.layout.log_maximized {
			return Outcome::Ignored;
		}

		if self.issues_area.is_some_and(|area| area.contains(position)) {
			let last = source.view().issues.len().saturating_sub(1);
			let index = self.issue_selected.saturating_add_signed(rows.signum()).min(last);
			self.select_issue(index);
			return Outcome::Done;
		}

		if self.mode == ViewMode::Library {
			self.tree.scroll_by(rows);
			return Outcome::Done;
		}
		Outcome::Ignored
	}
}
