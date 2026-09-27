//! The new project form's state, apart from the terminal: the fields, the name that follows the source, the typed
//! `DELETE` and the checks.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use cryarchiver_project::settings::{DELETE_CONFIRMATION, SettingsChange, Speed};
use tui_input::Input;

use super::validate::{Validation, validate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
	Source,
	Destination,
	Name,
	Speed,
	DeleteOriginals,
	Create,
	Cancel,
}

pub const FORM_FIELDS: [FormField; 7] = [
	FormField::Source,
	FormField::Destination,
	FormField::Name,
	FormField::Speed,
	FormField::DeleteOriginals,
	FormField::Create,
	FormField::Cancel,
];

impl FormField {
	pub const fn is_text(self) -> bool {
		matches!(self, Self::Source | Self::Destination | Self::Name)
	}
}

pub struct NewProjectForm {
	source: Input,
	destination: Input,
	name: Input,
	name_edited: bool,
	speed: Speed,
	/// The typed confirmation that turned deleting originals on.
	delete_confirmation: Option<String>,
	/// The `DELETE` prompt while it is open.
	confirming: Option<Input>,
	focus: FormField,
	validation: Validation,
	/// Why the last Create failed.
	error: Option<String>,
}

impl NewProjectForm {
	pub fn new() -> Self {
		Self {
			source: Input::default(),
			destination: Input::default(),
			name: Input::default(),
			name_edited: false,
			speed: Speed::default(),
			delete_confirmation: None,
			confirming: None,
			focus: FormField::Source,
			validation: validate("", "", ""),
			error: None,
		}
	}

	pub fn value(&self, field: FormField) -> &str {
		self.input(field).map_or("", Input::value)
	}

	pub fn input(&self, field: FormField) -> Option<&Input> {
		match field {
			FormField::Source => Some(&self.source),
			FormField::Destination => Some(&self.destination),
			FormField::Name => Some(&self.name),
			_ => None,
		}
	}

	/// Edits a text field. Editing the source renames the project too, until the name was edited itself.
	pub fn edit(&mut self, field: FormField, change: impl FnOnce(&mut Input)) {
		let input = match field {
			FormField::Source => &mut self.source,
			FormField::Destination => &mut self.destination,
			FormField::Name => &mut self.name,
			_ => return,
		};
		let before = input.value().to_owned();
		change(input);
		if input.value() == before {
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

	pub fn move_focus(&mut self, step: isize) {
		let index = FORM_FIELDS.iter().position(|&field| field == self.focus).unwrap_or(0);
		let last = FORM_FIELDS.len() - 1;
		self.focus = FORM_FIELDS[index.saturating_add_signed(step).min(last)];
	}

	pub fn speed(&self) -> Speed {
		self.speed
	}

	pub fn set_speed(&mut self, speed: Speed) {
		self.speed = speed;
	}

	pub fn delete_originals(&self) -> bool {
		self.delete_confirmation.is_some()
	}

	/// Turning deletion on only opens the prompt; it is on once the prompt is confirmed with `DELETE`.
	pub fn toggle_delete(&mut self) {
		if self.delete_confirmation.take().is_none() {
			self.confirming = Some(Input::default());
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
		self.validation.passes() && self.confirming.is_none()
	}

	pub fn error(&self) -> Option<&str> {
		self.error.as_deref()
	}

	/// Creates the project through the project layer, which checks everything again. Deleting originals goes through
	/// the settings' own `DELETE` gate; it enables the setting only, and the session that opens the project still
	/// asks for `DELETE` before anything is deleted.
	pub fn create(&mut self) -> Option<PathBuf> {
		self.revalidate();
		if !self.can_create() {
			return None;
		}

		let (speed, confirmation) = (self.speed, self.delete_confirmation.clone());
		let created = cryarchiver_project::create(
			Path::new(self.source.value()),
			Path::new(self.destination.value()),
			OsStr::new(self.name.value()),
			|settings| {
				settings.speed = speed;
				if let Some(confirmation) = confirmation {
					settings
						.apply(SettingsChange::DeleteOriginals {
							enabled: true,
							confirmation,
						})
						.expect("the form keeps only a confirmation that reads DELETE");
				}
			},
		);

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

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::MAIN_SEPARATOR;

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
		let meta = cryarchiver_project::store::peek(&path).unwrap();
		assert_eq!(meta.settings.speed, Speed::Fast);
		assert!(meta.settings.delete_originals);
		assert!(form.error().is_none());
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
