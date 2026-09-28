//! Rebuilding the in-memory state from the project file, and crash recovery on open (project layer spec §5).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use cryarchiver_engine::commit::file_id;
use cryarchiver_engine::quality::memory::SharedMemory;
use cryarchiver_engine::sink::ArchivalSink;
use cryarchiver_engine::text::display_path;
use cryarchiver_engine::workspace::{ARCHIVE_DIR, is_link, sweep};
use tokio::sync::mpsc::unbounded_channel;

use super::dangerous::DangerState;
use super::dependencies::ReferenceState;
use super::duplicates::DedupState;
use super::finish::FinishState;
use super::live::{self, LiveState};
use super::owner::Owner;
use super::pipeline::Pipeline;
use super::publish::ViewState;
use super::resolve::VerifyState;
use super::scanning::ScanState;
use super::watching::WatchState;
use super::{ProjectError, ProjectOptions};
use crate::dedup::full_hash;
use crate::issues::IssueIds;
use crate::keys::RelPath;
use crate::memory::MemoryBudget;
use crate::record::{FileState, Identity, Output};
use crate::schedule::GovernorConfig;
use crate::settings::{RootConflict, Settings, SettingsError, identity_conflict};
use crate::store::{Loaded, Store};
use crate::tree::{FileId, FolderId, Tree};
use crate::view::Update;
use crate::watch::Debouncer;

/// Every refusal an open makes from the project file's place and settings alone. They run on the settings read
/// without writing, before the file is opened for writing, and again on the settings that open loads.
pub(super) fn refuse_to_open(project_file: &Path, settings: &Settings) -> Result<(), ProjectError> {
	refuse_a_moved_project(project_file, &settings.destination)?;
	refuse_a_linked_work_area(&settings.destination)?;
	refuse_overlapping_roots(settings)
}

/// Every destination path is the stored one, so a project file copied or moved elsewhere would drive the archive it
/// came from; it is opened only from its own archive's folder, found from the file's real path.
fn refuse_a_moved_project(project_file: &Path, destination: &Path) -> Result<(), ProjectError> {
	let real = fs::canonicalize(project_file)?;
	let here = real.parent().unwrap_or(&real);
	if fs::canonicalize(destination).is_ok_and(|stored| stored == here) {
		return Ok(());
	}
	Err(ProjectError::Moved {
		destination: display_path(destination),
	})
}

/// A link where the project writes, stages or sweeps would lead those writes and deletes wherever it points: the
/// destination, its work area and the staging folder must each be a real folder, or not there yet.
fn refuse_a_linked_work_area(destination: &Path) -> Result<(), ProjectError> {
	let sink = ArchivalSink::new(destination);
	let folders = [
		destination.to_path_buf(),
		destination.join(ARCHIVE_DIR),
		sink.archive_work_root(),
		sink.staged_dir(),
	];

	for folder in folders {
		match fs::symlink_metadata(&folder) {
			Ok(meta) if is_link(&meta) => {
				return Err(ProjectError::Link {
					path: display_path(&folder),
				});
			}
			Ok(_) => {}
			Err(err) if err.kind() == io::ErrorKind::NotFound => break,
			Err(err) => return Err(err.into()),
		}
	}
	Ok(())
}

/// The roots were checked when the project was made; a source that can be reached is checked again, since a folder
/// moved or mounted since could put the destination inside it.
fn refuse_overlapping_roots(settings: &Settings) -> Result<(), ProjectError> {
	let Ok(source) = fs::canonicalize(&settings.source) else {
		return Ok(());
	};
	let destination = fs::canonicalize(&settings.destination)?;

	match RootConflict::between(&source, &destination).or_else(|| identity_conflict(&source, &destination)) {
		Some(conflict) => Err(SettingsError::Roots(conflict.message().into()).into()),
		None => Ok(()),
	}
}

impl Owner {
	pub(super) fn open(
		project_file: &Path,
		store: Store,
		settings: Settings,
		mut loaded: Loaded,
		options: ProjectOptions,
		updates: Sender<Update>,
	) -> Result<Self, ProjectError> {
		refuse_to_open(project_file, &settings)?;
		fs::create_dir_all(&settings.destination)?;
		let (messages, inbox) = unbounded_channel();
		let unknown = loaded
			.unknown
			.iter()
			.map(|(path, record)| (path.clone(), record.acknowledged))
			.collect();
		let watching = WatchState::new(Debouncer::new(options.tuning.debounce), unknown);
		let memory = Arc::new(MemoryBudget::default());
		let quality = Arc::new(SharedMemory::with_entries(std::mem::take(&mut loaded.quality)));
		let applied = live::apply_limit(settings.performance_limit);
		let image_threads = live::image_threads(settings.performance_limit, applied);
		let mut pipeline = Pipeline::new(
			&settings.source,
			&settings.destination,
			GovernorConfig {
				ramp: options.tuning.ramp,
				max_encodes: options.tuning.max_encodes,
			},
			options.backend.encoder(image_threads, memory.clone(), quality.clone()),
			image_threads,
			memory,
			quality,
		);
		pipeline.governor.set_limit(settings.performance_limit.is_some());

		let mut owner = Self {
			project_file: project_file.to_path_buf(),
			destination_identity: file_id(&settings.destination).ok().map(Identity::from),
			enclosing_identities: settings
				.source
				.ancestors()
				.skip(1)
				.filter_map(|folder| file_id(folder).ok().map(Identity::from))
				.collect(),
			settings,
			store,
			tree: Tree::new(),
			backend: options.backend,
			tuning: options.tuning,
			updates,
			messages,
			inbox: Some(inbox),
			started: false,
			paused: true,
			stopping: false,
			delete_confirmed: false,
			fault: options.fault,
			crashed: false,
			scan: ScanState::default(),
			pipeline,
			dedup: DedupState::default(),
			references: ReferenceState::default(),
			finish: FinishState::default(),
			watching,
			danger: DangerState::default(),
			live: LiveState::new(applied),
			verify: VerifyState::default(),
			view: ViewState::default(),
			issue_ids: IssueIds::default(),
			issue_files: BTreeSet::new(),
			last_attempts: HashMap::new(),
			sample: None,
			failure: None,
		};
		owner.load(loaded);
		owner.recover();

		sweep(&ArchivalSink::new(&owner.settings.destination).archive_work_root());
		owner.store.flush()?;
		Ok(owner)
	}

	fn load(&mut self, loaded: Loaded) {
		let mut folders = loaded.folders;
		folders.sort_by(|a, b| a.0.depth().cmp(&b.0.depth()).then_with(|| a.0.cmp(&b.0)));
		for (key, record) in folders {
			let id = self.tree.ensure_folder(&key);
			self.tree.set_scanned(id, record.scanned);
			self.tree.folder_mut(id).record = record;
		}

		let mut files = loaded.files;
		files.sort_by(|a, b| a.0.cmp(&b.0));
		for (key, record) in files {
			let folder = key
				.parent()
				.map_or(FolderId::ROOT, |parent| self.tree.ensure_folder(&parent));
			let file = self.tree.add_file(folder, &key.name(), record);
			self.note_placeholder(file);
			self.track_issue(file);
			if let Some(output) = self
				.tree
				.file(file)
				.record
				.state
				.output()
				.map(|output| output.path.clone())
			{
				self.claim(file, &output);
			}
		}

		self.last_attempts = loaded.last_attempts;
	}

	/// Work that was running is requeued; a commit whose rename happened becomes Done, and so does a delete that didn't
	/// happen.
	fn recover(&mut self) {
		let files: Vec<FileId> = self.tree.file_ids().collect();
		for file in files {
			let record = &self.tree.file(file).record;
			match &record.state {
				FileState::InProgress { .. } => self.set_state(file, FileState::Queued),
				FileState::Committing { output, .. } => {
					let (output, replaces) = (output.clone(), record.replaces.clone());
					self.recover_commit(file, output, replaces.as_ref());
				}
				FileState::DeletePending(_) => self.recover_delete(file),
				_ => {}
			}
		}
	}

	/// A re-archive into its own earlier output's place can find the old output there, with the same size, so only
	/// the new content's hash proves the rename happened. A file someone else put at the named path isn't ours either:
	/// a recorded hash must match. An earlier output stays while a record depends on it.
	fn recover_commit(&mut self, file: FileId, output: Output, replaces: Option<&RelPath>) {
		let target = self.destination_path(&output.path);
		let hash_matches = |hash| full_hash(&target).is_ok_and(|found| found == hash);
		let placed = match replaces {
			Some(previous) if *previous == output.path => output.hash.is_some_and(hash_matches),
			_ => {
				fs::metadata(&target).is_ok_and(|meta| meta.len() == output.size)
					&& output.hash.is_none_or(hash_matches)
			}
		};
		if !placed {
			self.release_intent_claim(file, &output.path);
			self.set_state(file, FileState::Queued);
			return;
		}

		if let Some(previous) = replaces.filter(|_| !self.has_dependents(file)) {
			self.remove_replaced_output(previous, &output.path);
		}

		let mut record = self.tree.file(file).record.clone();
		record.replaces = None;
		self.tree.set_record(file, record);
		self.set_state(file, FileState::Done(output));
	}
}
