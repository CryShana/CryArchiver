use crate::view::{AttemptLog, FileId, Issue, IssueId, IssueKind, IssueVerbs, NodeId};

pub struct NewIssue<'a> {
	pub kind: IssueKind,
	pub file: Option<FileId>,
	pub folder: Option<NodeId>,
	pub name: String,
	pub location: String,
	pub reason: &'a str,
	pub attempts: Vec<AttemptLog>,
}

impl NewIssue<'_> {
	pub fn into_issue(self, id: IssueId) -> Issue {
		Issue {
			id,
			kind: self.kind,
			file: self.file,
			folder: self.folder,
			name: self.name,
			location: self.location,
			reason: self.reason.to_owned(),
			resolution: resolution(self.kind).to_owned(),
			verbs: verbs(self.kind),
			attempts: self.attempts,
		}
	}
}

/// The verb table of the project layer spec §10.1.
pub fn verbs(kind: IssueKind) -> IssueVerbs {
	let label = |text: &str| Some(text.to_owned());
	match kind {
		IssueKind::SourceDamaged | IssueKind::EncodeFailed => IssueVerbs {
			retry: label("Retry"),
			accept: label("Copy original"),
			exclude: true,
		},
		IssueKind::SourceUnreadable => IssueVerbs {
			retry: label("Retry"),
			accept: None,
			exclude: true,
		},
		IssueKind::OutputMissing => IssueVerbs {
			retry: label("Redo"),
			accept: label("Accept as is"),
			exclude: false,
		},
		IssueKind::UnknownFile | IssueKind::DirectoryCycle => IssueVerbs {
			retry: None,
			accept: label("Acknowledge"),
			exclude: false,
		},
	}
}

fn resolution(kind: IssueKind) -> &'static str {
	match kind {
		IssueKind::SourceDamaged | IssueKind::EncodeFailed => "Waiting for your decision",
		IssueKind::SourceUnreadable => "Fix the permission or the link, then retry",
		IssueKind::OutputMissing => "Never redone without asking",
		IssueKind::UnknownFile => "Never deleted",
		IssueKind::DirectoryCycle => "Not followed; reported once",
	}
}

pub fn attempt(number: u32, lines: &[&str]) -> AttemptLog {
	AttemptLog {
		attempt: number,
		lines: lines.iter().map(|&line| line.to_owned()).collect(),
	}
}
