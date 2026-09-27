//! Reference files in the run (project layer spec §8.1): a cue sheet or playlist waits for the files it names, a
//! sidecar subtitle for its media file; then it is copied with the new names written in. It never waits for its
//! folder, so a folder that keeps receiving files in watch mode doesn't hold it forever.

use std::collections::HashMap;
use std::ffi::OsString;

use super::jobs::{Made, Task};
use super::owner::Owner;
use crate::record::{CopyReason, FileState};
use crate::references::{self, RefKind, Reference};
use crate::tree::FileId;

#[derive(Debug, Default)]
pub(crate) struct ReferenceState {
	contents: HashMap<FileId, (RefKind, Vec<u8>)>,
	names: HashMap<FileId, Vec<(Reference, FileId)>>,
	sidecar_of: HashMap<FileId, FileId>,
}

impl Owner {
	pub(super) fn remember_content(&mut self, file: FileId, content: Vec<u8>) {
		if let Some(kind) = references::kind_of(&self.tree.file(file).name) {
			self.references.contents.insert(file, (kind, content));
		}
	}

	/// Returns whether the file is a reference file; it is then queued, or waits for the files it names.
	pub(super) fn gate_references(&mut self, file: FileId) -> bool {
		let entry = self.tree.file(file);
		let Some(kind) = references::kind_of(&entry.name) else {
			return false;
		};

		let targets: Vec<FileId> = match kind {
			RefKind::CueSheet | RefKind::Playlist => {
				let Some((_, content)) = self.references.contents.get(&file) else {
					return false;
				};
				let named: Vec<(Reference, FileId)> =
					references::references(kind, &self.tree.folder(entry.folder).key, content)
						.into_iter()
						.filter_map(|reference| {
							let target = self.tree.file_id(&reference.target)?;
							(target != file && !self.tree.file(target).removed).then_some((reference, target))
						})
						.collect();
				let targets = named.iter().map(|(_, target)| *target).collect();
				self.references.names.insert(file, named);
				targets
			}
			RefKind::Sidecar => {
				let folder = self.tree.folder(entry.folder);
				let siblings: Vec<OsString> = folder
					.files
					.iter()
					.map(|&sibling| self.tree.file(sibling))
					.filter(|sibling| !sibling.removed)
					.map(|sibling| sibling.name.clone())
					.collect();
				let Some(media) = references::sidecar_target(&entry.name, siblings.iter().map(OsString::as_os_str))
				else {
					return false;
				};
				let Some(target) = self.tree.file_id(&folder.key.join(&media)) else {
					return false;
				};
				self.references.sidecar_of.insert(file, target);
				vec![target]
			}
		};

		let pending: Vec<FileId> = targets
			.into_iter()
			.filter(|target| !self.tree.file(*target).record.state.is_settled())
			.collect();
		self.set_state(file, FileState::Queued);
		if pending.is_empty() {
			self.enqueue(file);
		} else {
			self.pipeline.waits.wait(file, pending);
		}
		true
	}

	pub(super) fn is_reference(&self, file: FileId) -> bool {
		self.references.names.contains_key(&file) || self.references.sidecar_of.contains_key(&file)
	}

	/// A cue sheet or playlist whose named files changed names is written anew; anything else is copied as it is.
	pub(super) fn reference_task(&self, file: FileId) -> Option<Task> {
		let named = self.references.names.get(&file)?;
		let (kind, content) = self.references.contents.get(&file)?;
		let folder = self.destination_folder(self.tree.file(file).folder);

		let renames: Vec<(Vec<u8>, Vec<u8>)> = named
			.iter()
			.filter_map(|(reference, target)| {
				let output = self.tree.file(*target).record.state.output()?;
				let relative = output.path.relative_from(&folder);
				let text = references::reference_text(&reference.text, &relative);
				(text != reference.text).then(|| (reference.text.clone(), text))
			})
			.collect();
		if renames.is_empty() {
			return None;
		}

		Some(Task::Write {
			content: references::rewrite(*kind, content, &renames),
			made: Made::Copied(CopyReason::Rewritten),
		})
	}

	/// A sidecar follows its media file's new stem; everything else keeps its name.
	pub(super) fn copied_output_name(&self, file: FileId) -> OsString {
		let entry = self.tree.file(file);
		let renamed = self.references.sidecar_of.get(&file).and_then(|&media| {
			let output = self.tree.file(media).record.state.output()?;
			Some(references::sidecar_name(
				&entry.name,
				&self.tree.file(media).name,
				&output.path.name(),
			))
		});
		renamed.unwrap_or_else(|| cryarchiver_engine::naming::copied_name(&entry.name))
	}

	pub(super) fn forget_reference(&mut self, file: FileId) {
		self.references.contents.remove(&file);
		self.references.names.remove(&file);
		self.references.sidecar_of.remove(&file);
	}
}
