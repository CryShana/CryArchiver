//! The new project form's state, apart from the terminal: the fields, the name that follows the source, the typed
//! `DELETE` and the checks.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use cryarchiver_engine::preset::{Overrides, Speed as PresetSpeed};
use cryarchiver_project::settings::{DELETE_CONFIRMATION, SettingsChange, Speed};
use tui_input::Input;

use super::validate::{Probe, Validation, probe_destination, validate};
use crate::app::{QUALITY_FIELDS, QualityField};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
	Source,
	Destination,
	Name,
	Speed,
	/// Opens and closes the section with the three quality fields.
	CustomQuality,
	VideoVmaf,
	ImageTarget,
	AudioKbps,
	DeleteOriginals,
	Create,
	Cancel,
}

pub const FORM_FIELDS: [FormField; 11] = [
	FormField::Source,
	FormField::Destination,
	FormField::Name,
	FormField::Speed,
	FormField::CustomQuality,
	FormField::VideoVmaf,
	FormField::ImageTarget,
	FormField::AudioKbps,
	FormField::DeleteOriginals,
	FormField::Create,
	FormField::Cancel,
];

impl FormField {
	pub const fn is_text(self) -> bool {
		matches!(
			self,
			Self::Source | Self::Destination | Self::Name | Self::VideoVmaf | Self::ImageTarget | Self::AudioKbps
		)
	}

	const fn quality(self) -> Option<QualityField> {
		match self {
			Self::VideoVmaf => Some(QualityField::Video),
			Self::ImageTarget => Some(QualityField::Image),
			Self::AudioKbps => Some(QualityField::Audio),
			_ => None,
		}
	}
}

/// What a quality field of the form gives the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualityCheck {
	/// Left empty, so the project uses the preset's value, shown as `preset 93/89`.
	Preset(String),
	Custom,
	/// FAST never measures video quality, so the value isn't stored.
	NotUsed,
	Invalid(String),
}

pub struct NewProjectForm {
	source: Input,
	destination: Input,
	name: Input,
	name_edited: bool,
	speed: Speed,
	/// In the order of [`QUALITY_FIELDS`].
	quality: [Input; 3],
	custom_quality_open: bool,
	/// The typed confirmation that turned deleting originals on.
	delete_confirmation: Option<String>,
	/// The `DELETE` prompt while it is open.
	confirming: Option<Input>,
	focus: FormField,
	validation: Validation,
	/// Anything was changed since the form opened.
	edited: bool,
	/// Why the last Create failed.
	error: Option<String>,
	/// A warning from the last Create that still succeeded.
	warning: Option<String>,
}

impl NewProjectForm {
	pub fn new() -> Self {
		Self {
			source: Input::default(),
			destination: Input::default(),
			name: Input::default(),
			name_edited: false,
			speed: Speed::default(),
			quality: Default::default(),
			custom_quality_open: false,
			delete_confirmation: None,
			confirming: None,
			focus: FormField::Source,
			validation: validate("", "", ""),
			edited: false,
			error: None,
			warning: None,
		}
	}

	pub fn edited(&self) -> bool {
		self.edited
	}

	pub fn take_warning(&mut self) -> Option<String> {
		self.warning.take()
	}

	pub fn value(&self, field: FormField) -> &str {
		self.input(field).map_or("", Input::value)
	}

	pub fn input(&self, field: FormField) -> Option<&Input> {
		match field {
			FormField::Source => Some(&self.source),
			FormField::Destination => Some(&self.destination),
			FormField::Name => Some(&self.name),
			_ => field.quality().map(|quality| &self.quality[quality_index(quality)]),
		}
	}

	/// Edits a text field. Editing the source renames the project too, until the name was edited itself.
	pub fn edit(&mut self, field: FormField, change: impl FnOnce(&mut Input)) {
		let input = match (field, field.quality()) {
			(FormField::Source, _) => &mut self.source,
			(FormField::Destination, _) => &mut self.destination,
			(FormField::Name, _) => &mut self.name,
			(_, Some(quality)) => &mut self.quality[quality_index(quality)],
			_ => return,
		};
		let before = input.value().to_owned();
		change(input);
		if input.value() == before {
			return;
		}

		self.edited = true;
		if field.quality().is_some() {
			self.error = None;
			return;
		}

		match field {
			FormField::Name => self.name_edited = !self.name.value().is_empty(),
			FormField::Source if !self.name_edited => {
				let folder_name = Path::new(self.source.value())
					.file_name()
					.map(|name| name.to_string_lossy().into_owned())
					.unwrap_or_default();
				self.name = Input::new(folder_name);
			}
			_ => {}
		}
		self.revalidate();
	}

	fn revalidate(&mut self) {
		self.error = None;
		self.validation = validate(self.source.value(), self.destination.value(), self.name.value());
	}

	pub fn focus(&self) -> FormField {
		self.focus
	}

	pub fn set_focus(&mut self, field: FormField) {
		self.focus = field;
	}

	/// Steps through the fields on screen; a closed "Custom quality…" section hides its fields.
	pub fn move_focus(&mut self, step: isize) {
		let visible: Vec<FormField> = FORM_FIELDS
			.into_iter()
			.filter(|field| self.custom_quality_open || field.quality().is_none())
			.collect();
		let index = visible.iter().position(|&field| field == self.focus).unwrap_or(0);
		let last = visible.len() - 1;
		self.focus = visible[index.saturating_add_signed(step).min(last)];
	}

	pub fn speed(&self) -> Speed {
		self.speed
	}

	pub fn custom_quality_open(&self) -> bool {
		self.custom_quality_open
	}

	/// Closing the section keeps what was typed in it: the project still gets those values.
	pub fn toggle_custom_quality(&mut self) {
		self.custom_quality_open = !self.custom_quality_open;
		if !self.custom_quality_open && self.focus.quality().is_some() {
			self.focus = FormField::CustomQuality;
		}
	}

	/// `None` for a field that isn't a quality field.
	pub fn quality_check(&self, field: FormField) -> Option<QualityCheck> {
		let quality = field.quality()?;
		let speed = PresetSpeed::from(self.speed);
		let typed = self.quality[quality_index(quality)].value();

		let Some(preset) = quality.preset_text(speed) else {
			return Some(QualityCheck::NotUsed);
		};
		Some(match quality.with(Overrides::default(), typed) {
			Err(reason) => QualityCheck::Invalid(reason),
			Ok(overrides) if overrides.is_empty() => QualityCheck::Preset(format!("preset {}", quality.shown(&preset))),
			Ok(_) => QualityCheck::Custom,
		})
	}

	/// The overrides the project is created with, or the first reason a quality field can't be used. A video value
	/// isn't stored at FAST, which never measures video quality.
	pub fn overrides(&self) -> Result<Overrides, String> {
		let speed = PresetSpeed::from(self.speed);
		QUALITY_FIELDS
			.into_iter()
			.zip(&self.quality)
			.filter(|(field, _)| field.applies(speed))
			.try_fold(Overrides::default(), |overrides, (field, input)| {
				field.with(overrides, input.value())
			})
	}

	pub fn set_speed(&mut self, speed: Speed) {
		self.edited |= speed != self.speed;
		self.speed = speed;
	}

	pub fn delete_originals(&self) -> bool {
		self.delete_confirmation.is_some()
	}

	/// Turning deletion on only opens the prompt; it is on once the prompt is confirmed with `DELETE`.
	pub fn toggle_delete(&mut self) {
		if self.delete_confirmation.take().is_none() {
			self.confirming = Some(Input::default());
		} else {
			self.edited = true;
		}
	}

	pub fn confirming(&self) -> Option<&Input> {
		self.confirming.as_ref()
	}

	pub fn confirming_mut(&mut self) -> Option<&mut Input> {
		self.confirming.as_mut()
	}

	/// Returns whether the typed text was `DELETE`; the prompt closes either way.
	pub fn confirm_delete(&mut self) -> bool {
		let typed = self.confirming.take().map(|input| input.value().to_owned());
		let confirmed = typed.as_deref() == Some(DELETE_CONFIRMATION);
		if confirmed {
			self.delete_confirmation = typed;
			self.edited = true;
		}
		confirmed
	}

	pub fn cancel_delete(&mut self) {
		self.confirming = None;
	}

	pub fn validation(&self) -> &Validation {
		&self.validation
	}

	pub fn can_create(&self) -> bool {
		self.validation.passes() && self.confirming.is_none() && self.overrides().is_ok()
	}

	pub fn error(&self) -> Option<&str> {
		self.error.as_deref()
	}

	/// Checks everything again on the final values, then probes the destination, the one check that writes, and
	/// creates the project through the project layer, which checks the roots once more. Deleting originals goes
	/// through the settings' own `DELETE` gate; it enables the setting only, and the session that opens the project
	/// still asks for `DELETE` before anything is deleted.
	pub fn create(&mut self) -> Option<PathBuf> {
		self.revalidate();
		if !self.can_create() {
			return None;
		}

		let (source, destination) = (Path::new(self.source.value()), Path::new(self.destination.value()));
		match probe_destination(source, destination) {
			Probe::Writable => {}
			Probe::Leftover(warning) => self.warning = Some(warning),
			Probe::Failed(reason) => {
				self.error = Some(format!("the destination can't be written: {reason}"));
				return None;
			}
		}

		let (speed, confirmation) = (self.speed, self.delete_confirmation.clone());
		let overrides = self.overrides().unwrap_or_default();
		let created = cryarchiver_project::create(source, destination, OsStr::new(self.name.value()), |settings| {
			settings.speed = speed;
			settings.overrides = overrides;
			if let Some(confirmation) = confirmation {
				settings
					.apply(SettingsChange::DeleteOriginals {
						enabled: true,
						confirmation,
					})
					.expect("the form keeps only a confirmation that reads DELETE");
			}
		});

		match created {
			Ok(path) => Some(path),
			Err(err) => {
				self.error = Some(err.to_string());
				None
			}
		}
	}
}

impl Default for NewProjectForm {
	fn default() -> Self {
		Self::new()
	}
}

fn quality_index(field: QualityField) -> usize {
	QUALITY_FIELDS
		.iter()
		.position(|&candidate| candidate == field)
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::MAIN_SEPARATOR;

	use cryarchiver_engine::preset::VmafOverride;

	use super::*;

	fn typed(form: &mut NewProjectForm, field: FormField, text: &str) {
		form.edit(field, |input| {
			for c in text.chars() {
				input.handle(tui_input::InputRequest::InsertChar(c));
			}
		});
	}

	fn replace(form: &mut NewProjectForm, field: FormField, text: &str) {
		form.edit(field, |input| *input = Input::new(text.into()));
	}

	struct Setup {
		dir: tempfile::TempDir,
		source: String,
		destination: String,
	}

	fn setup() -> Setup {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("Holiday");
		fs::create_dir(&source).unwrap();
		let destination = dir.path().join("archive");
		Setup {
			source: source.display().to_string(),
			destination: destination.display().to_string(),
			dir,
		}
	}

	fn filled(setup: &Setup) -> NewProjectForm {
		let mut form = NewProjectForm::new();
		replace(&mut form, FormField::Source, &setup.source);
		replace(&mut form, FormField::Destination, &setup.destination);
		form
	}

	#[test]
	fn the_name_follows_the_source_until_it_is_edited() {
		let mut form = NewProjectForm::new();

		replace(&mut form, FormField::Source, &format!("D{MAIN_SEPARATOR}Photos"));
		assert_eq!(form.value(FormField::Name), "Photos");
		typed(&mut form, FormField::Source, "2");
		assert_eq!(form.value(FormField::Name), "Photos2");

		typed(&mut form, FormField::Name, " old");
		replace(&mut form, FormField::Source, &format!("D{MAIN_SEPARATOR}Music"));
		assert_eq!(form.value(FormField::Name), "Photos2 old");

		replace(&mut form, FormField::Name, "");
		replace(&mut form, FormField::Source, &format!("D{MAIN_SEPARATOR}Video"));
		assert_eq!(form.value(FormField::Name), "Video");
	}

	#[test]
	fn create_is_enabled_only_when_every_check_passes() {
		let setup = setup();
		let mut form = NewProjectForm::new();
		assert!(!form.can_create());

		replace(&mut form, FormField::Source, &setup.source);
		assert!(!form.can_create());

		replace(&mut form, FormField::Destination, &setup.destination);
		assert!(form.can_create(), "{:?}", form.validation());

		replace(&mut form, FormField::Name, "CON");
		assert!(!form.can_create());
	}

	#[test]
	fn deleting_originals_needs_delete_typed() {
		let setup = setup();
		let mut form = filled(&setup);

		form.toggle_delete();
		assert!(!form.delete_originals());
		assert!(!form.can_create());
		form.confirming_mut()
			.unwrap()
			.handle(tui_input::InputRequest::InsertChar('d'));
		assert!(!form.confirm_delete());
		assert!(!form.delete_originals());
		assert!(form.confirming().is_none());

		form.toggle_delete();
		for c in "DELETE".chars() {
			form.confirming_mut()
				.unwrap()
				.handle(tui_input::InputRequest::InsertChar(c));
		}
		assert!(form.confirm_delete());
		assert!(form.delete_originals());
		assert!(form.can_create());

		form.toggle_delete();
		assert!(!form.delete_originals());
		assert!(form.confirming().is_none());
	}

	#[test]
	fn create_writes_the_project_with_the_chosen_settings() {
		let setup = setup();
		let mut form = filled(&setup);
		form.set_speed(Speed::Fast);
		form.toggle_delete();
		for c in "DELETE".chars() {
			form.confirming_mut()
				.unwrap()
				.handle(tui_input::InputRequest::InsertChar(c));
		}
		form.confirm_delete();

		let path = form.create().unwrap();

		assert_eq!(path, setup.dir.path().join("archive").join("Holiday.cryarchive"));
		let meta = cryarchiver_project::store::peek(&path).unwrap().meta;
		assert_eq!(meta.settings.speed, Speed::Fast);
		assert!(meta.settings.delete_originals);
		assert!(form.error().is_none());
	}

	#[test]
	fn the_shown_project_file_is_the_one_created() {
		let setup = setup();
		let mut form = filled(&setup);
		let shown = form.validation().project_file.clone().unwrap();

		let created = form.create().unwrap();

		assert_eq!(shown, created);
	}

	#[test]
	fn filling_the_form_writes_nothing_until_create() {
		let setup = setup();
		let folder = |dir: &Path| {
			let mut names: Vec<_> = fs::read_dir(dir)
				.unwrap()
				.map(|entry| entry.unwrap().file_name())
				.collect();
			names.sort();
			(names, fs::metadata(dir).unwrap().modified().unwrap())
		};
		let before = folder(setup.dir.path());

		let mut form = filled(&setup);
		typed(&mut form, FormField::Name, " 2026");
		form.set_speed(Speed::Slow);
		form.toggle_custom_quality();
		typed(&mut form, FormField::ImageTarget, "88");
		assert!(form.can_create());
		assert_eq!(folder(setup.dir.path()), before);

		let path = form.create().unwrap();
		assert!(form.take_warning().is_none());
		assert_eq!(folder(setup.dir.path()).0, ["Holiday", "archive"]);
		assert_eq!(folder(path.parent().unwrap()).0, ["Holiday 2026.cryarchive"]);
	}

	#[test]
	fn a_form_counts_as_edited_once_anything_changes() {
		let mut form = NewProjectForm::new();
		assert!(!form.edited());

		form.set_speed(Speed::Moderate);
		form.toggle_delete();
		form.cancel_delete();
		assert!(!form.edited());

		form.set_speed(Speed::Fast);
		assert!(form.edited());

		let mut form = NewProjectForm::new();
		typed(&mut form, FormField::Source, "D");
		assert!(form.edited());
	}

	#[test]
	fn custom_quality_round_trips_into_the_created_project() {
		let setup = setup();
		let mut form = filled(&setup);
		form.toggle_custom_quality();
		replace(&mut form, FormField::VideoVmaf, "90/85");
		replace(&mut form, FormField::ImageTarget, "80");
		replace(&mut form, FormField::AudioKbps, "160");
		assert!(form.can_create(), "{:?}", form.overrides());

		let path = form.create().unwrap();

		let meta = cryarchiver_project::store::peek(&path).unwrap().meta;
		assert_eq!(
			meta.settings.overrides,
			Overrides {
				video: Some(VmafOverride {
					mean: 90.0,
					floor: 85.0,
				}),
				image: Some(80.0),
				audio_stereo_kbps: Some(160),
			}
		);
	}

	#[test]
	fn empty_quality_fields_keep_the_preset() {
		let setup = setup();
		let mut form = filled(&setup);
		form.set_speed(Speed::Slow);

		assert_eq!(
			form.quality_check(FormField::VideoVmaf),
			Some(QualityCheck::Preset("preset 95/91".into()))
		);
		assert_eq!(
			form.quality_check(FormField::ImageTarget),
			Some(QualityCheck::Preset("preset 90".into()))
		);
		assert_eq!(
			form.quality_check(FormField::AudioKbps),
			Some(QualityCheck::Preset("preset 160 kbps".into()))
		);
		assert_eq!(form.quality_check(FormField::Name), None);

		let path = form.create().unwrap();
		assert!(
			cryarchiver_project::store::peek(&path)
				.unwrap()
				.meta
				.settings
				.overrides
				.is_empty()
		);
	}

	#[test]
	fn quality_fields_are_checked_as_they_are_typed() {
		let setup = setup();
		let mut form = filled(&setup);
		form.toggle_custom_quality();

		typed(&mut form, FormField::ImageTarget, "120");
		assert!(
			matches!(form.quality_check(FormField::ImageTarget), Some(QualityCheck::Invalid(reason)) if reason.contains("between 50 and 95"))
		);
		assert!(!form.can_create());
		assert!(form.create().is_none());
		assert!(!setup.dir.path().join("archive").exists());

		replace(&mut form, FormField::ImageTarget, "80");
		assert_eq!(form.quality_check(FormField::ImageTarget), Some(QualityCheck::Custom));
		typed(&mut form, FormField::VideoVmaf, "93");
		assert!(
			matches!(form.quality_check(FormField::VideoVmaf), Some(QualityCheck::Invalid(reason)) if reason.contains("MEAN/FLOOR"))
		);
		typed(&mut form, FormField::VideoVmaf, "/95");
		assert!(
			matches!(form.quality_check(FormField::VideoVmaf), Some(QualityCheck::Invalid(reason)) if reason.contains("the mean (93)"))
		);
		replace(&mut form, FormField::VideoVmaf, "");
		typed(&mut form, FormField::AudioKbps, "100");
		assert!(
			matches!(form.quality_check(FormField::AudioKbps), Some(QualityCheck::Invalid(reason)) if reason.contains("steps of 8"))
		);

		form.toggle_custom_quality();
		assert!(!form.can_create(), "a collapsed section still counts");
		replace(&mut form, FormField::AudioKbps, "96");
		assert!(form.can_create());
	}

	#[test]
	fn at_fast_the_video_field_is_not_stored() {
		let setup = setup();
		let mut form = filled(&setup);
		form.toggle_custom_quality();
		replace(&mut form, FormField::VideoVmaf, "90/85");
		replace(&mut form, FormField::ImageTarget, "80");
		form.set_speed(Speed::Fast);

		assert_eq!(form.quality_check(FormField::VideoVmaf), Some(QualityCheck::NotUsed));
		let path = form.create().unwrap();

		let meta = cryarchiver_project::store::peek(&path).unwrap().meta;
		assert_eq!(
			meta.settings.overrides,
			Overrides {
				image: Some(80.0),
				..Overrides::default()
			}
		);
	}

	#[test]
	fn a_collapsed_section_is_skipped_by_the_focus() {
		let mut form = NewProjectForm::new();
		form.set_focus(FormField::CustomQuality);

		form.move_focus(1);
		assert_eq!(form.focus(), FormField::DeleteOriginals);

		form.toggle_custom_quality();
		form.set_focus(FormField::CustomQuality);
		form.move_focus(1);
		assert_eq!(form.focus(), FormField::VideoVmaf);
		form.move_focus(3);
		assert_eq!(form.focus(), FormField::DeleteOriginals);
		form.move_focus(-1);
		assert_eq!(form.focus(), FormField::AudioKbps);

		form.set_focus(FormField::ImageTarget);
		form.toggle_custom_quality();
		assert_eq!(form.focus(), FormField::CustomQuality);
		form.move_focus(-1);
		assert_eq!(form.focus(), FormField::Speed);
	}

	#[test]
	fn create_checks_again_first() {
		let setup = setup();
		let mut form = filled(&setup);
		fs::remove_dir(&setup.source).unwrap();

		assert!(form.create().is_none());
		assert!(!form.validation().source.is_valid());
		assert!(!setup.dir.path().join("archive").exists());
	}
}
