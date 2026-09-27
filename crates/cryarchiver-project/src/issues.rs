//! The issue kinds and their verbs (project layer spec §10.1). Every kind uses the same three verbs; each declares
//! which apply and what the front end calls them.

use std::collections::HashMap;

use crate::keys::RelPath;
use crate::record::IssueKind;
use crate::tree::{FileId, FolderId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Problem {
	SourceDamaged,
	EncodeFailed,
	SourceUnreadable,
	OutputMissing,
	UnknownFile,
	DirectoryCycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
	Retry,
	Accept,
	Exclude,
}

/// A `Some` label is the kind's own name for the verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verbs {
	pub retry: Option<&'static str>,
	pub accept: Option<&'static str>,
	pub exclude: bool,
}

impl Problem {
	pub fn of(kind: IssueKind) -> Self {
		match kind {
			IssueKind::Unrecoverable { damaged: true } => Self::SourceDamaged,
			IssueKind::Unrecoverable { damaged: false } => Self::EncodeFailed,
			IssueKind::SourceUnreadable => Self::SourceUnreadable,
			IssueKind::OutputMissing => Self::OutputMissing,
		}
	}

	pub fn verbs(self) -> Verbs {
		match self {
			Self::SourceDamaged | Self::EncodeFailed => Verbs {
				retry: Some("Retry"),
				accept: Some("Copy original"),
				exclude: true,
			},
			Self::SourceUnreadable => Verbs {
				retry: Some("Retry"),
				accept: None,
				exclude: true,
			},
			Self::OutputMissing => Verbs {
				retry: Some("Redo"),
				accept: Some("Accept as is"),
				exclude: false,
			},
			Self::UnknownFile | Self::DirectoryCycle => Verbs {
				retry: None,
				accept: Some("Acknowledge"),
				exclude: false,
			},
		}
	}

	pub fn allows(self, verb: Verb) -> bool {
		let verbs = self.verbs();

		match verb {
			Verb::Retry => verbs.retry.is_some(),
			Verb::Accept => verbs.accept.is_some(),
			Verb::Exclude => verbs.exclude,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IssueId(pub u64);

/// What an issue is about: a source file, a file in the destination the project didn't write, or a folder link
/// that leads back into its own tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IssueKey {
	File(FileId),
	Unknown(RelPath),
	Cycle(FolderId),
}

/// Hands out issue ids for the session, so the same issue keeps its id while it stays open.
#[derive(Debug, Default)]
pub struct IssueIds {
	ids: HashMap<IssueKey, IssueId>,
	keys: HashMap<IssueId, IssueKey>,
	next: u64,
}

impl IssueIds {
	pub fn id(&mut self, key: &IssueKey) -> IssueId {
		if let Some(id) = self.ids.get(key) {
			return *id;
		}

		self.next += 1;
		let id = IssueId(self.next);
		self.ids.insert(key.clone(), id);
		self.keys.insert(id, key.clone());
		id
	}

	pub fn key(&self, id: IssueId) -> Option<&IssueKey> {
		self.keys.get(&id)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn each_kind_offers_its_own_verbs() {
		assert_eq!(
			Problem::SourceDamaged.verbs(),
			Verbs {
				retry: Some("Retry"),
				accept: Some("Copy original"),
				exclude: true
			}
		);
		assert!(!Problem::SourceUnreadable.allows(Verb::Accept));
		assert_eq!(Problem::OutputMissing.verbs().retry, Some("Redo"));
		assert_eq!(Problem::OutputMissing.verbs().accept, Some("Accept as is"));
		assert!(!Problem::OutputMissing.allows(Verb::Exclude));
		for acknowledged in [Problem::UnknownFile, Problem::DirectoryCycle] {
			assert_eq!(acknowledged.verbs().accept, Some("Acknowledge"));
			assert!(!acknowledged.allows(Verb::Retry));
			assert!(!acknowledged.allows(Verb::Exclude));
		}
	}

	#[test]
	fn stored_kinds_map_to_problems() {
		assert_eq!(
			Problem::of(IssueKind::Unrecoverable { damaged: true }),
			Problem::SourceDamaged
		);
		assert_eq!(
			Problem::of(IssueKind::Unrecoverable { damaged: false }),
			Problem::EncodeFailed
		);
		assert_eq!(Problem::of(IssueKind::OutputMissing), Problem::OutputMissing);
	}

	#[test]
	fn an_issue_keeps_its_id() {
		let mut ids = IssueIds::default();
		let file = IssueKey::File(FileId(4));

		let first = ids.id(&file);
		let other = ids.id(&IssueKey::Cycle(FolderId(2)));

		assert_eq!(ids.id(&file), first);
		assert_ne!(first, other);
		assert_eq!(ids.key(first), Some(&file));
	}
}
