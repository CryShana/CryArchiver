//! Rebuilding the in-memory state from the project file, and crash recovery on open (project layer spec §5).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::sync::mpsc::Sender;

use cryarchiver_engine::commit::file_id;
use cryarchiver_engine::sink::ArchivalSink;
use cryarchiver_engine::workspace::sweep;
use tokio::sync::mpsc::unbounded_channel;

use super::owner::Owner;
use super::publish::ViewState;
use super::scanning::ScanState;
use super::{ProjectError, ProjectOptions};
use crate::issues::IssueIds;
use crate::record::{FileState, Identity};
use crate::settings::Settings;
use crate::store::{Loaded, Store};
use crate::tree::{FileId, FolderId, Tree};
use crate::view::Update;

impl Owner {
	pub(super) fn open(
		store: Store,
		settings: Settings,
		loaded: Loaded,
		options: ProjectOptions,
		updates: Sender<Update>,
	) -> Result<Self, ProjectError> {
		fs::create_dir_all(&settings.destination)?;
		let (messages, inbox) = unbounded_channel();

		let mut owner = Self {
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
			delete_confirmed: false,
			scan: ScanState::default(),
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
			self.track_issue(file);
		}

		self.last_attempts = loaded.last_attempts;
	}

	/// Work that was running is requeued; a commit whose rename happened becomes Done.
	fn recover(&mut self) {
		let files: Vec<FileId> = self.tree.file_ids().collect();
		for file in files {
			let recovered = match &self.tree.file(file).record.state {
				FileState::InProgress { .. } => Some(FileState::Queued),
				FileState::Committing { output, .. } => {
					let target = self.destination_path(&output.path);
					let placed = fs::metadata(target).is_ok_and(|meta| meta.len() == output.size);
					Some(if placed {
						FileState::Done(output.clone())
					} else {
						FileState::Queued
					})
				}
				_ => None,
			};

			if let Some(state) = recovered {
				self.set_state(file, state);
			}
		}
	}
}
