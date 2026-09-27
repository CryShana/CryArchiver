//! Reference files in the run (project layer spec §8.1): a cue sheet or playlist waits for the files it names, a
//! sidecar subtitle for its media file; then it is copied with the new names written in. It never waits for its
//! folder, so a folder that keeps receiving files in watch mode doesn't hold it forever.

use std::collections::HashMap;
use std::ffi::OsString;

use super::jobs::{Made, Task};
use super::owner::Owner;
use crate::record::{CopyReason, FileState, How, Output};
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
		if let Some(kind @ (RefKind::CueSheet | RefKind::Playlist)) = references::kind_of(&self.tree.file(file).name) {
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
				let output = self.content_output(*target)?;
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
			same_file: None,
		})
	}

	/// Where a named file's content is archived: a placeholder stands for its canonical copy's output.
	fn content_output(&self, file: FileId) -> Option<&Output> {
		let output = self.tree.file(file).record.state.output()?;
		match &output.how {
			How::Placeholder { canonical, lost: false } => self.canonical_output(self.tree.file_id(canonical)?),
			How::Placeholder { lost: true, .. } => None,
			How::Encoded | How::Copied(_) => Some(output),
		}
	}

	/// A sidecar follows its media file's new stem, unless that is a placeholder; everything else keeps its name.
	pub(super) fn copied_output_name(&self, file: FileId) -> OsString {
		let entry = self.tree.file(file);
		let renamed = self.references.sidecar_of.get(&file).and_then(|&media| {
			let output = self
				.tree
				.file(media)
				.record
				.state
				.output()
				.filter(|output| !matches!(output.how, How::Placeholder { .. }))?;
			Some(references::sidecar_name(
				&entry.name,
				&self.tree.file(media).name,
				&output.path.name(),
			))
		});
		renamed.unwrap_or_else(|| cryarchiver_engine::naming::copied_name(&entry.name))
	}

	pub(super) fn forget_reference(&mut self, file: FileId) {
		self.forget_reference_content(file);
		self.references.sidecar_of.remove(&file);
	}

	/// An archived reference file needs its content and names no more.
	pub(super) fn forget_reference_content(&mut self, file: FileId) {
		self.references.contents.remove(&file);
		self.references.names.remove(&file);
	}
}

#[cfg(test)]
mod tests {
	use std::fs;

	use crate::keys::RelPath;
	use crate::project::scanning::tests::{key, list, owner};

	#[test]
	fn only_cue_sheets_and_playlists_keep_their_content() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		for name in ["movie.srt", "album.cue", "mix.m3u"] {
			fs::write(source.join(name), b"FILE \"a.flac\" WAVE\n").unwrap();
		}
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);

		for name in ["movie.srt", "album.cue", "mix.m3u"] {
			let file = owner.tree.file_id(&key(name)).unwrap();
			owner.remember_content(file, b"FILE \"a.flac\" WAVE\n".to_vec());
		}

		let mut kept: Vec<String> = owner
			.references
			.contents
			.keys()
			.map(|file| owner.tree.file(*file).key.display())
			.collect();
		kept.sort();
		assert_eq!(kept, ["album.cue", "mix.m3u"]);
	}
}
