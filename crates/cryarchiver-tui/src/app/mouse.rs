use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::tree::Selection;
use super::{Outcome, Overlay, Ui, ViewMode};
use crate::source::ArchiveSource;
use crate::view::{FileId, NodeId, StateFilter};

pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const WHEEL_ROWS: isize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
	Footer(KeyCode),
	Row { index: usize, arrow_x: u16 },
	LibraryTitle,
	ProgressHeader,
	Outcome(StateFilter),
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
	Scrollbar(Bar),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
	Library,
	Log,
}

impl Hit {
	fn on_log(self) -> bool {
		matches!(
			self,
			Self::LogToggle
				| Self::LogMaximize
				| Self::LogNewest
				| Self::LogLine(_)
				| Self::LogPanel
				| Self::Scrollbar(Bar::Log)
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
		self.find(position).map(|(_, hit)| hit)
	}

	/// The region under `position` along with what it is.
	pub fn find(&self, position: Position) -> Option<(Rect, H)> {
		self.0.iter().rev().find(|(area, _)| area.contains(position)).copied()
	}
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Click {
	position: Position,
	at: Instant,
}

/// A scrollbar the left button went down on. It follows the pointer's row wherever the pointer goes until the button
/// comes up.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScrollDrag {
	bar: Bar,
	track: Rect,
}

impl Ui {
	pub(super) fn on_mouse(&mut self, source: &dyn ArchiveSource, mouse: MouseEvent, now: Instant) -> Outcome {
		let position = Position::new(mouse.column, mouse.row);
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left) => {
				self.scroll_drag = None;
				let double = self.register_click(position, now);
				self.on_click(source, position, double)
			}
			MouseEventKind::Drag(MouseButton::Left) => self.on_drag(source, position),
			MouseEventKind::Up(MouseButton::Left) => {
				self.scroll_drag = None;
				Outcome::Ignored
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
		let found = self.hits.find(position);
		let hit = found.map(|(_, hit)| hit);

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
			Some(Overlay::ConfirmQuit) => {
				return match hit {
					Some(Hit::Footer(code)) => self.on_key(source, KeyEvent::new(code, KeyModifiers::NONE), false),
					_ => Outcome::Ignored,
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

		match found {
			Some((track, Hit::Scrollbar(bar))) => {
				self.scroll_drag = Some(ScrollDrag { bar, track });
				self.drag_to(source, position.y);
				Outcome::Done
			}
			Some((_, hit)) => self.on_hit(source, hit, position, double),
			None => Outcome::Ignored,
		}
	}

	fn on_drag(&mut self, source: &dyn ArchiveSource, position: Position) -> Outcome {
		if self.scroll_drag.is_none() || self.overlay.is_some() {
			return Outcome::Ignored;
		}

		self.drag_to(source, position.y);
		Outcome::Done
	}

	fn drag_to(&mut self, source: &dyn ArchiveSource, row: u16) {
		let Some(drag) = self.scroll_drag else { return };

		match drag.bar {
			Bar::Library => {
				let top = scroll_for_pointer(drag.track, row, self.tree.deepest_scroll());
				self.tree.scroll_to(top);
			}
			Bar::Log => {
				let log = &source.view().log;
				let back = scroll_for_pointer(drag.track, row, self.log_scroll.deepest(log));
				self.log_scroll.scroll_to(log, back);
			}
		}
	}

	fn on_hit(&mut self, source: &dyn ArchiveSource, hit: Hit, position: Position, double: bool) -> Outcome {
		match hit {
			Hit::Footer(code) => return self.on_key(source, KeyEvent::new(code, KeyModifiers::NONE), false),
			Hit::Row { index, arrow_x } => self.on_row_click(index, arrow_x, position, double),
			Hit::LibraryTitle if double => self.toggle_maximized(),
			Hit::Maximize => self.toggle_maximized(),
			Hit::ProgressHeader => self.tree.cycle_sort(),
			Hit::Outcome(state) => {
				self.toggle_state_filter(&source.view().library, state);
				self.mode = ViewMode::Library;
			}
			Hit::LogToggle => self.layout.log_open = !self.layout.log_open,
			Hit::LogMaximize => self.toggle_log_maximized(),
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
			| Hit::LogPanel
			| Hit::Scrollbar(_) => {
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
		if matches!(
			self.overlay,
			Some(Overlay::ConfirmDelete(_) | Overlay::ConfirmRestart | Overlay::ConfirmQuit)
		) {
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

/// The scroll offset, out of `0..=deepest`, that a pointer on `row` asks of a scrollbar `track`: its top cell shows the
/// start, its bottom cell the end, and rows beyond the track count as its ends.
fn scroll_for_pointer(track: Rect, row: u16, deepest: usize) -> usize {
	let last = usize::from(track.height.saturating_sub(1));
	if last == 0 {
		return 0;
	}

	let offset = usize::from(row.saturating_sub(track.y)).min(last);
	(offset * deepest + last / 2) / last
}

#[cfg(test)]
mod tests {
	use super::*;

	const TRACK: Rect = Rect::new(119, 10, 1, 11);

	#[test]
	fn the_track_maps_rows_onto_the_whole_scroll_range() {
		assert_eq!(scroll_for_pointer(TRACK, 10, 100), 0, "top");
		assert_eq!(scroll_for_pointer(TRACK, 15, 100), 50, "middle");
		assert_eq!(scroll_for_pointer(TRACK, 20, 100), 100, "bottom");
		assert_eq!(scroll_for_pointer(TRACK, 11, 100), 10);
		assert_eq!(scroll_for_pointer(TRACK, 11, 4), 0, "rounds to the nearest offset");
		assert_eq!(scroll_for_pointer(TRACK, 12, 4), 1);
	}

	#[test]
	fn rows_beyond_the_track_count_as_its_ends() {
		assert_eq!(scroll_for_pointer(TRACK, 0, 100), 0);
		assert_eq!(scroll_for_pointer(TRACK, 9, 100), 0);
		assert_eq!(scroll_for_pointer(TRACK, 21, 100), 100);
		assert_eq!(scroll_for_pointer(TRACK, u16::MAX, 100), 100);
	}

	#[test]
	fn a_one_row_track_stays_at_the_top() {
		assert_eq!(scroll_for_pointer(Rect::new(0, 3, 1, 1), 3, 50), 0);
	}
}
