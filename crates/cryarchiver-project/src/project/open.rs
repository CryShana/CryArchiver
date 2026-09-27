//! Rebuilding the in-memory state from the project file, and crash recovery on open (project layer spec §5).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use cryarchiver_engine::commit::file_id;
use cryarchiver_engine::sink::ArchivalSink;
use cryarchiver_engine::workspace::sweep;
use tokio::sync::mpsc::unbounded_channel;

use super::dependencies::ReferenceState;
use super::duplicates::DedupState;
use super::finish::FinishState;
use super::owner::Owner;
use super::pipeline::Pipeline;
use super::publish::ViewState;
use super::scanning::ScanState;
use super::watching::WatchState;
use super::{ProjectError, ProjectOptions};
use crate::dedup::full_hash;
use crate::issues::IssueIds;
use crate::keys::RelPath;
use crate::memory::MemoryBudget;
use crate::record::{FileState, Identity, Output};
use crate::schedule::GovernorConfig;
use crate::settings::Settings;
use crate::store::{Loaded, Store};
use crate::tree::{FileId, FolderId, Tree};
use crate::view::Update;
use crate::watch::Debouncer;

impl Owner {
	pub(super) fn open(
		project_file: &Path,
		store: Store,
		settings: Settings,
		loaded: Loaded,
		options: ProjectOptions,
		updates: Sender<Update>,
	) -> Result<Self, ProjectError> {
		fs::create_dir_all(&settings.destination)?;
		let (messages, inbox) = unbounded_channel();
		let unknown = loaded
			.unknown
			.iter()
			.map(|(path, record)| (path.clone(), record.acknowledged))
			.collect();
		let watching = WatchState::new(Debouncer::new(options.tuning.debounce), unknown);
		let memory = Arc::new(MemoryBudget::default());
		let image_threads = std::thread::available_parallelism().map_or(1, usize::from);
		let pipeline = Pipeline::new(
			&settings.destination,
			GovernorConfig {
				ramp: options.tuning.ramp,
				max_encodes: options.tuning.max_encodes,
			},
			options.backend.encoder(image_threads, memory.clone()),
			memory,
		);

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

	/// Work that was running is requeued; a commit whose rename happened becomes Done.
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
