use std::time::Duration;

use cryarchiver_engine::audio::plan::table_rate;
use cryarchiver_engine::images::plan::ssim_target;
use cryarchiver_engine::preset::{Overrides, Speed as PresetSpeed, VmafOverride};
use cryarchiver_engine::video::quality;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::{Lifecycle, Outcome, Overlay, Ui};
use crate::source::{Action, ArchiveSource, SettingsChange};
use crate::view::{ArchiveView, Settings, Speed};

pub const FAST_NEVER_MEASURES_VIDEO: &str = "FAST never measures video quality";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
	Speed,
	Quality(QualityField),
	ResetQuality,
	MinSavings,
	Watch,
	Limit,
	StabilityWindow,
	KeepAuxiliaryImages,
	DeleteOriginals,
	Rescan,
	RestartPendingJobs,
}

pub const SETTINGS_ROWS: [SettingsRow; 13] = [
	SettingsRow::Speed,
	SettingsRow::Quality(QualityField::Video),
	SettingsRow::Quality(QualityField::Image),
	SettingsRow::Quality(QualityField::Audio),
	SettingsRow::ResetQuality,
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

/// One of the quality targets a project can override on top of its speed preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityField {
	Video,
	Image,
	Audio,
}

pub const QUALITY_FIELDS: [QualityField; 3] = [QualityField::Video, QualityField::Image, QualityField::Audio];

impl QualityField {
	/// The engine measures this target at `speed`.
	pub fn applies(self, speed: PresetSpeed) -> bool {
		!(self == Self::Video && speed == PresetSpeed::Fast)
	}

	/// The preset's own value as typed text, or `None` where the preset doesn't measure it.
	pub fn preset_text(self, speed: PresetSpeed) -> Option<String> {
		if !self.applies(speed) {
			return None;
		}

		Some(match self {
			Self::Video => {
				let target = quality::target(speed, false, &Overrides::default());
				format!("{}/{}", target.mean, target.floor)
			}
			Self::Image => ssim_target(speed).to_string(),
			Self::Audio => table_rate(2, speed, &Overrides::default()).to_string(),
		})
	}

	/// The stored override as typed text.
	pub fn text(self, overrides: &Overrides) -> Option<String> {
		match self {
			Self::Video => overrides.video.map(|video| format!("{}/{}", video.mean, video.floor)),
			Self::Image => overrides.image.map(|image| image.to_string()),
			Self::Audio => overrides.audio_stereo_kbps.map(|kbps| kbps.to_string()),
		}
	}

	/// Typed text as it reads on screen, with its unit.
	pub fn shown(self, text: &str) -> String {
		match self {
			Self::Audio => format!("{text} kbps"),
			Self::Video | Self::Image => text.to_owned(),
		}
	}

	/// `overrides` with this field set from typed text, checked with [`Overrides::validate`]. Empty text clears the
	/// field, so it returns to the preset.
	pub fn with(self, overrides: Overrides, text: &str) -> Result<Overrides, String> {
		let text = text.trim();
		let mut changed = overrides;

		match self {
			Self::Video if text.is_empty() => changed.video = None,
			Self::Image if text.is_empty() => changed.image = None,
			Self::Audio if text.is_empty() => changed.audio_stereo_kbps = None,
			Self::Video => {
				let (mean, floor) = text
					.split_once('/')
					.ok_or_else(|| "type MEAN/FLOOR, such as 93/89".to_owned())?;
				changed.video = Some(VmafOverride {
					mean: number(mean)?,
					floor: number(floor)?,
				});
			}
			Self::Image => changed.image = Some(number(text)?),
			Self::Audio => changed.audio_stereo_kbps = Some(number(text)?),
		}

		changed.validate()?;
		Ok(changed)
	}
}

fn number<T: std::str::FromStr>(text: &str) -> Result<T, String> {
	let text = text.trim();
	text.parse().map_err(|_| format!("\"{text}\" is not a number"))
}

impl SettingsRow {
	pub const fn label(self) -> &'static str {
		match self {
			Self::Speed => "Speed",
			Self::Quality(QualityField::Video) => "Video VMAF mean/floor",
			Self::Quality(QualityField::Image) => "Image SSIMULACRA2 target",
			Self::Quality(QualityField::Audio) => "Audio stereo bitrate",
			Self::ResetQuality => "Reset quality to the preset",
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
			Self::Quality(field) => quality_value(field, settings),
			Self::ResetQuality if settings.overrides.is_empty() => "nothing to reset".to_owned(),
			Self::ResetQuality => "⏎ · every quality value back to the preset".to_owned(),
			Self::MinSavings => format!("{} %", settings.min_savings),
			Self::Watch => on_off(settings.watch),
			Self::Limit => settings
				.performance_limit
				.map_or_else(|| "off".to_owned(), |cap| format!("{cap} % CPU · one job at a time")),
			Self::StabilityWindow => format!("{} s", settings.stability_window.as_secs()),
			Self::KeepAuxiliaryImages => on_off(settings.keep_auxiliary_images),
			Self::DeleteOriginals => {
				if settings.delete_originals && view.header.deleting_paused {
					"ON · paused this session: ⏎ type DELETE to allow · ← off".to_owned()
				} else if settings.delete_originals {
					"ON · originals are deleted after a verified archive".to_owned()
				} else {
					"off".to_owned()
				}
			}
			Self::Rescan => "⏎".to_owned(),
			Self::RestartPendingJobs => {
				let pending = view.limit().is_some_and(|limit| limit.pending);
				if pending {
					"⏎ · progress will be lost"
				} else {
					"nothing pending"
				}
				.to_owned()
			}
		}
	}

	/// The value is the preset's, or has nothing to act on, so it is drawn dimmed.
	pub fn dimmed(self, settings: &Settings) -> bool {
		match self {
			Self::Quality(field) => {
				!field.applies(settings.speed.preset()) || field.text(&settings.overrides).is_none()
			}
			Self::ResetQuality => settings.overrides.is_empty(),
			_ => false,
		}
	}
}

/// An unset override shows the preset's value, labelled as the preset, so the screen never shows a value the project
/// doesn't use.
fn quality_value(field: QualityField, settings: &Settings) -> String {
	let speed = settings.speed.preset();
	let stored = field.text(&settings.overrides).map(|text| field.shown(&text));

	match (field.preset_text(speed), stored) {
		(None, Some(stored)) => format!("{stored} · not used: {FAST_NEVER_MEASURES_VIDEO}"),
		(None, None) => FAST_NEVER_MEASURES_VIDEO.to_owned(),
		(Some(_), Some(stored)) => format!("custom · {stored}"),
		(Some(preset), None) => format!("preset · {}", field.shown(&preset)),
	}
}

impl Ui {
	pub(super) fn on_settings_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent, selected: usize) -> Outcome {
		let selected = selected.min(SETTINGS_ROWS.len() - 1);
		let row = SETTINGS_ROWS[selected];
		match key.code {
			KeyCode::Up | KeyCode::Char('k') => self.overlay = Some(Overlay::Settings(selected.saturating_sub(1))),
			KeyCode::Down | KeyCode::Char('j') => {
				self.overlay = Some(Overlay::Settings((selected + 1).min(SETTINGS_ROWS.len() - 1)));
			}
			KeyCode::Left => self.adjust(source, selected, -1),
			KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.adjust(source, selected, 1),
			KeyCode::Char('r') | KeyCode::Delete => self.reset_quality(source, row),
			KeyCode::Esc | KeyCode::Char('s') => self.overlay = None,
			KeyCode::Char('q') => self.lifecycle = Lifecycle::Quitting,
			KeyCode::F(2) => self.mouse_capture = !self.mouse_capture,
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn adjust(&mut self, source: &dyn ArchiveSource, selected: usize, step: i32) {
		let view = source.view();
		let settings = &view.settings;
		let change = match SETTINGS_ROWS[selected] {
			SettingsRow::Speed => SettingsChange::Speed(next_speed(settings.speed, step)),
			SettingsRow::Quality(field) => {
				if step > 0 && field.applies(settings.speed.preset()) {
					let typed = field.text(&settings.overrides).unwrap_or_default();
					self.overlay = Some(Overlay::EditQuality {
						row: selected,
						input: Input::new(typed),
					});
				}
				return;
			}
			SettingsRow::ResetQuality if settings.overrides.is_empty() || step < 0 => return,
			SettingsRow::ResetQuality => SettingsChange::Overrides(Overrides::default()),
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
			SettingsRow::DeleteOriginals if !settings.delete_originals || (view.header.deleting_paused && step > 0) => {
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
				if view.limit().is_some_and(|limit| limit.pending) {
					self.overlay = Some(Overlay::ConfirmRestart);
				}
				return;
			}
		};
		self.actions.push(Action::ChangeSettings(change));
	}

	/// Clears one override. A row the speed doesn't measure stays as it is; "Reset quality to the preset" clears it.
	fn reset_quality(&mut self, source: &dyn ArchiveSource, row: SettingsRow) {
		let settings = &source.view().settings;
		let SettingsRow::Quality(field) = row else { return };
		if !field.applies(settings.speed.preset()) || field.text(&settings.overrides).is_none() {
			return;
		}

		if let Ok(cleared) = field.with(settings.overrides, "") {
			self.actions
				.push(Action::ChangeSettings(SettingsChange::Overrides(cleared)));
		}
	}

	/// Typing goes into the value; ⏎ applies it only when it passes the checks, whose reason shows as it is typed.
	pub(super) fn on_edit_quality_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let Some(Overlay::EditQuality { row, input }) = &mut self.overlay else {
			return Outcome::Ignored;
		};
		let row = *row;
		let SettingsRow::Quality(field) = SETTINGS_ROWS[row] else {
			self.overlay = Some(Overlay::Settings(row));
			return Outcome::Done;
		};

		match key.code {
			KeyCode::Esc => self.overlay = Some(Overlay::Settings(row)),
			KeyCode::Enter => {
				let stored = source.view().settings.overrides;
				if let Ok(changed) = field.with(stored, input.value()) {
					if changed != stored {
						self.actions
							.push(Action::ChangeSettings(SettingsChange::Overrides(changed)));
					}
					self.overlay = Some(Overlay::Settings(row));
				}
			}
			_ => {
				input.handle_event(&Event::Key(key));
			}
		}
		Outcome::Done
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_preset_values_are_the_engines() {
		assert_eq!(
			QualityField::Video.preset_text(PresetSpeed::Moderate).as_deref(),
			Some("93/89")
		);
		assert_eq!(
			QualityField::Video.preset_text(PresetSpeed::Slow).as_deref(),
			Some("95/91")
		);
		assert_eq!(QualityField::Video.preset_text(PresetSpeed::Fast), None);
		assert_eq!(
			QualityField::Image.preset_text(PresetSpeed::Fast).as_deref(),
			Some("80")
		);
		assert_eq!(
			QualityField::Audio.preset_text(PresetSpeed::Slow).as_deref(),
			Some("160")
		);
	}

	#[test]
	fn typed_values_become_overrides_or_the_reason_they_cannot() {
		let stored = Overrides {
			image: Some(80.0),
			..Overrides::default()
		};

		assert_eq!(
			QualityField::Video.with(stored, " 92.5 / 88 "),
			Ok(Overrides {
				video: Some(VmafOverride {
					mean: 92.5,
					floor: 88.0
				}),
				..stored
			})
		);
		assert_eq!(QualityField::Image.with(stored, ""), Ok(Overrides::default()));
		assert_eq!(
			QualityField::Video.with(stored, "93"),
			Err("type MEAN/FLOOR, such as 93/89".to_owned())
		);
		assert_eq!(
			QualityField::Audio.with(stored, "12a"),
			Err("\"12a\" is not a number".to_owned())
		);
		assert!(
			QualityField::Image
				.with(stored, "nan")
				.unwrap_err()
				.contains("between 50 and 95")
		);
	}
}
