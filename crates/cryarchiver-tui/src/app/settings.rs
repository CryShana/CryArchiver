use std::time::Duration;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::{Lifecycle, Outcome, Overlay, Ui};
use crate::source::{Action, ArchiveSource, SettingsChange};
use crate::view::{ArchiveView, Speed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
	Speed,
	MinSavings,
	Watch,
	Limit,
	StabilityWindow,
	KeepAuxiliaryImages,
	DeleteOriginals,
	Rescan,
	RestartPendingJobs,
}

pub const SETTINGS_ROWS: [SettingsRow; 9] = [
	SettingsRow::Speed,
	SettingsRow::MinSavings,
	SettingsRow::Watch,
	SettingsRow::Limit,
	SettingsRow::StabilityWindow,
	SettingsRow::KeepAuxiliaryImages,
	SettingsRow::DeleteOriginals,
	SettingsRow::Rescan,
	SettingsRow::RestartPendingJobs,
];

const LIMIT_STEPS: [Option<u8>; 5] = [None, Some(25), Some(50), Some(75), Some(100)];

impl SettingsRow {
	pub const fn label(self) -> &'static str {
		match self {
			Self::Speed => "Speed",
			Self::MinSavings => "Minimum savings",
			Self::Watch => "Watch for new files",
			Self::Limit => "Performance limit",
			Self::StabilityWindow => "Stability window",
			Self::KeepAuxiliaryImages => "Keep photos' auxiliary images",
			Self::DeleteOriginals => "Delete originals",
			Self::Rescan => "Re-scan source now",
			Self::RestartPendingJobs => "Restart jobs over the limit",
		}
	}

	pub fn value(self, view: &ArchiveView) -> String {
		let settings = &view.settings;
		let on_off = |on: bool| if on { "on" } else { "off" }.to_owned();
		match self {
			Self::Speed => settings.speed.label().to_owned(),
			Self::MinSavings => format!("{} %", settings.min_savings),
			Self::Watch => on_off(settings.watch),
			Self::Limit => settings
				.performance_limit
				.map_or_else(|| "off".to_owned(), |cap| format!("{cap} % CPU · one job at a time")),
			Self::StabilityWindow => format!("{} s", settings.stability_window.as_secs()),
			Self::KeepAuxiliaryImages => on_off(settings.keep_auxiliary_images),
			Self::DeleteOriginals => {
				if settings.delete_originals {
					"ON · originals are deleted after a verified archive".to_owned()
				} else {
					"off".to_owned()
				}
			}
			Self::Rescan => "⏎".to_owned(),
			Self::RestartPendingJobs => {
				let pending = view.header.performance_limit.is_some_and(|limit| limit.pending);
				if pending {
					"⏎ · progress will be lost"
				} else {
					"nothing pending"
				}
				.to_owned()
			}
		}
	}
}

impl Ui {
	pub(super) fn on_settings_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent, selected: usize) -> Outcome {
		let row = SETTINGS_ROWS[selected.min(SETTINGS_ROWS.len() - 1)];
		match key.code {
			KeyCode::Up | KeyCode::Char('k') => self.overlay = Some(Overlay::Settings(selected.saturating_sub(1))),
			KeyCode::Down | KeyCode::Char('j') => {
				self.overlay = Some(Overlay::Settings((selected + 1).min(SETTINGS_ROWS.len() - 1)));
			}
			KeyCode::Left => self.adjust(source, row, -1),
			KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.adjust(source, row, 1),
			KeyCode::Esc | KeyCode::Char('s') => self.overlay = None,
			KeyCode::Char('q') => self.lifecycle = Lifecycle::Quitting,
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn adjust(&mut self, source: &dyn ArchiveSource, row: SettingsRow, step: i32) {
		let view = source.view();
		let settings = &view.settings;
		let change = match row {
			SettingsRow::Speed => SettingsChange::Speed(next_speed(settings.speed, step)),
			SettingsRow::MinSavings => {
				let percent = (i32::from(settings.min_savings) + step * 5).clamp(0, 90);
				SettingsChange::MinSavings(u8::try_from(percent).unwrap_or(10))
			}
			SettingsRow::Watch => SettingsChange::Watch(!settings.watch),
			SettingsRow::Limit => SettingsChange::PerformanceLimit(next_limit(settings.performance_limit, step)),
			SettingsRow::StabilityWindow => {
				let seconds = settings
					.stability_window
					.as_secs()
					.saturating_add_signed(i64::from(step) * 10);
				SettingsChange::StabilityWindow(Duration::from_secs(seconds.clamp(5, 300)))
			}
			SettingsRow::KeepAuxiliaryImages => SettingsChange::KeepAuxiliaryImages(!settings.keep_auxiliary_images),
			SettingsRow::DeleteOriginals if !settings.delete_originals => {
				self.overlay = Some(Overlay::ConfirmDelete(Input::default()));
				return;
			}
			SettingsRow::DeleteOriginals => SettingsChange::DeleteOriginals {
				enabled: false,
				confirmation: String::new(),
			},
			SettingsRow::Rescan => {
				self.actions.push(Action::Rescan);
				return;
			}
			SettingsRow::RestartPendingJobs => {
				if view.header.performance_limit.is_some_and(|limit| limit.pending) {
					self.overlay = Some(Overlay::ConfirmRestart);
				}
				return;
			}
		};
		self.actions.push(Action::ChangeSettings(change));
	}

	/// Typing goes into the confirmation; nothing else reacts, so a stray `q` can't quit mid-sentence.
	pub(super) fn on_confirm_delete_key(&mut self, key: KeyEvent) -> Outcome {
		let settings_row = SETTINGS_ROWS
			.iter()
			.position(|&row| row == SettingsRow::DeleteOriginals)
			.unwrap_or(0);
		let Some(Overlay::ConfirmDelete(input)) = &mut self.overlay else {
			return Outcome::Ignored;
		};

		match key.code {
			KeyCode::Esc => self.overlay = Some(Overlay::Settings(settings_row)),
			KeyCode::Enter => {
				let confirmation = input.value().to_owned();
				self.actions
					.push(Action::ChangeSettings(SettingsChange::DeleteOriginals {
						enabled: true,
						confirmation,
					}));
				self.overlay = Some(Overlay::Settings(settings_row));
			}
			_ => {
				input.handle_event(&Event::Key(key));
			}
		}
		Outcome::Done
	}

	pub(super) fn on_confirm_restart_key(&mut self, key: KeyEvent) -> Outcome {
		match key.code {
			KeyCode::Enter | KeyCode::Char('y') => {
				self.actions.push(Action::RestartPendingJobs);
				self.overlay = None;
			}
			KeyCode::Esc | KeyCode::Char('n') => self.overlay = None,
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}
}

fn next_speed(speed: Speed, step: i32) -> Speed {
	let speeds = [Speed::Slow, Speed::Moderate, Speed::Fast];
	let index = speeds.iter().position(|&candidate| candidate == speed).unwrap_or(1);
	speeds[cycle(index, speeds.len(), step)]
}

fn next_limit(limit: Option<u8>, step: i32) -> Option<u8> {
	let index = LIMIT_STEPS
		.iter()
		.position(|&candidate| candidate == limit)
		.unwrap_or(0);
	LIMIT_STEPS[cycle(index, LIMIT_STEPS.len(), step)]
}

fn cycle(index: usize, len: usize, step: i32) -> usize {
	if step < 0 {
		(index + len - 1) % len
	} else {
		(index + 1) % len
	}
}
