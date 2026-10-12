//! Reference files in the run (project layer spec §8.1): a cue sheet or playlist waits for the files it names, a
//! sidecar subtitle for its media file; then it is copied with the new names written in. It never waits for its
//! folder, so a folder that keeps receiving files in watch mode doesn't hold it forever.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use super::jobs::{Made, Task};
use super::owner::{Message, Owner};
use crate::keys::RelPath;
use crate::record::{CopyReason, FileState, How, Output};
use crate::references::{self, MAX_REFERENCE_BYTES, RefKind, Reference};
use crate::tree::FileId;
use crate::view::LogKind;

#[derive(Debug, Default)]
pub(crate) struct ReferenceState {
	contents: HashMap<FileId, (RefKind, Vec<u8>)>,
	names: HashMap<FileId, Vec<(Reference, FileId)>>,
	sidecar_of: HashMap<FileId, FileId>,
	/// Retried kept originals and where their kept copy was, also when a placeholder adopted it.
	retried: HashMap<FileId, RelPath>,
	/// Retried files placed under a new name, whose archived reference files are looked for on the next tick.
	renamed: Vec<FileId>,
}

/// A cue sheet or playlist that may name a renamed file, and where to read it.
struct Candidate {
	file: FileId,
	kind: RefKind,
	source: PathBuf,
	folder: RelPath,
}

impl Owner {
	pub(super) fn remember_content(&mut self, file: FileId, content: Vec<u8>) {
		if let Some(kind @ (RefKind::CueSheet | RefKind::Playlist)) = references::kind_of(&self.tree.file(file).name) {
			self.references.contents.insert(file, (kind, content));
		}
	}

	/// Returns whether the file is a reference file; it is then queued, or waits for the files it names. A named file
	/// that already waits for this one, such as a playlist naming it back, isn't waited for: the two would wait forever.
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
			.filter(|&target| !self.pipeline.waits.waits_for(target, file))
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

	pub(super) fn remember_retried(&mut self, file: FileId, kept: RelPath) {
		self.references.retried.insert(file, kept);
	}

	pub(super) fn forget_retried(&mut self, file: FileId) {
		self.references.retried.remove(&file);
	}

	/// A retried file's new output is in place: when its name changed, the archived reference files that name the file
	/// are written again.
	pub(super) fn follow_new_name(&mut self, file: FileId, placed: &RelPath) {
		let previous = self.references.retried.remove(&file);
		let kept = self
			.tree
			.file(file)
			.record
			.retrying
			.as_ref()
			.map(|kept| kept.path.clone());
		if kept.or(previous).is_some_and(|previous| previous != *placed) {
			self.references.renamed.push(file);
		}
	}

	/// Sidecars are found by name. Cue sheets and playlists are read off the project task, and the ones that name a
	/// renamed file come back as [`Message::ReferencesNaming`].
	pub(super) fn reread_renamed_references(&mut self) {
		if self.references.renamed.is_empty() {
			return;
		}

		let renamed = std::mem::take(&mut self.references.renamed);
		for &target in &renamed {
			for sidecar in self.sidecars_of(target) {
				self.reread_reference(sidecar);
			}
		}

		let targets: HashSet<RelPath> = renamed.iter().map(|&file| self.tree.file(file).key.clone()).collect();
		let candidates: Vec<Candidate> = self
			.tree
			.file_ids()
			.filter_map(|file| {
				let entry = self.tree.file(file);
				let kind = references::kind_of(&entry.name)?;
				let archived = matches!(entry.record.state, FileState::Done(_)) && !entry.removed;
				(archived && kind != RefKind::Sidecar).then(|| Candidate {
					file,
					kind,
					source: self.source_path(&entry.key),
					folder: self.tree.folder(entry.folder).key.clone(),
				})
			})
			.collect();
		if candidates.is_empty() {
			return;
		}

		let messages = self.messages.clone();
		tokio::task::spawn_blocking(move || {
			let naming = candidates
				.into_iter()
				.filter(|candidate| names_any(candidate, &targets))
				.map(|candidate| candidate.file)
				.collect();
			let _ = messages.send(Message::ReferencesNaming(naming));
		});
	}

	fn sidecars_of(&self, media: FileId) -> Vec<FileId> {
		let entry = self.tree.file(media);
		let folder = self.tree.folder(entry.folder);
		let siblings: Vec<OsString> = folder
			.files
			.iter()
			.map(|&sibling| self.tree.file(sibling))
			.filter(|sibling| !sibling.removed)
			.map(|sibling| sibling.name.clone())
			.collect();

		folder
			.files
			.iter()
			.copied()
			.filter(|&file| {
				let name = &self.tree.file(file).name;
				references::kind_of(name) == Some(RefKind::Sidecar)
					&& references::sidecar_target(name, siblings.iter().map(OsString::as_os_str)).as_ref()
						== Some(&entry.name)
			})
			.collect()
	}

	pub(super) fn on_references_naming(&mut self, files: Vec<FileId>) {
		for file in files {
			self.reread_reference(file);
		}
	}

	/// An archived reference file is read again and archived anew, replacing its archived copy once the new one is in
	/// place.
	fn reread_reference(&mut self, file: FileId) {
		let entry = self.tree.file(file);
		let FileState::Done(output) = &entry.record.state else {
			return;
		};
		if entry.removed || self.pipeline.jobs.contains_key(&file) {
			return;
		}

		let name = entry.key.display();
		let mut record = entry.record.clone();
		record.replaces = Some(output.path.clone());
		self.tree.set_record(file, record);
		self.unqueue(file);
		self.set_state(file, FileState::Discovered);
		self.observe(file);
		self.log(
			LogKind::Action,
			format!("{name}: written again, as a file it names was archived under a new name"),
		);
	}

	pub(super) fn forget_reference(&mut self, file: FileId) {
		self.forget_reference_content(file);
		self.references.sidecar_of.remove(&file);
		self.references.retried.remove(&file);
	}

	/// An archived reference file needs its content and names no more.
	pub(super) fn forget_reference_content(&mut self, file: FileId) {
		self.references.contents.remove(&file);
		self.references.names.remove(&file);
	}
}

/// Whether the cue sheet or playlist, as its source reads now, names one of `targets`.
fn names_any(candidate: &Candidate, targets: &HashSet<RelPath>) -> bool {
	let small = fs::metadata(&candidate.source).is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_REFERENCE_BYTES);
	let Some(content) = small.then(|| fs::read(&candidate.source).ok()).flatten() else {
		return false;
	};

	references::references(candidate.kind, &candidate.folder, &content)
		.iter()
		.any(|reference| targets.contains(&reference.target))
}

#[cfg(test)]
mod tests {
	use std::fs;

	use crate::keys::RelPath;
	use crate::project::scanning::tests::{key, list, owner};
	use crate::record::{CopyReason, FileState, How, Output};

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

	#[test]
	fn the_sidecars_of_a_file_archived_under_a_new_name_are_written_again() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		for name in ["movie.mp4", "movie.en.srt", "other.mp4", "other.srt"] {
			fs::write(source.join(name), b"x").unwrap();
		}
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		let id = |owner: &super::Owner, name: &str| owner.tree.file_id(&key(name)).unwrap();
		let copied = |name: &str| Output {
			how: How::Copied(CopyReason::NotMedia),
			path: key(name),
			size: 1,
			hash: None,
		};
		for name in ["movie.en.srt", "other.srt"] {
			let file = id(&owner, name);
			owner.set_state(file, FileState::Done(copied(name)));
		}

		owner.references.renamed.push(id(&owner, "movie.mp4"));
		owner.reread_renamed_references();

		let sidecar = &owner.tree.file(id(&owner, "movie.en.srt")).record;
		assert_eq!(sidecar.state, FileState::Discovered);
		assert_eq!(sidecar.replaces, Some(key("movie.en.srt")));
		let other = &owner.tree.file(id(&owner, "other.srt")).record;
		assert_eq!(other.state, FileState::Done(copied("other.srt")));
		assert!(owner.references.renamed.is_empty());
	}
}
