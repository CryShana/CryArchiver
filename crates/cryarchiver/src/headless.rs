//! `cryarchiver run <project>`: a project run without the TUI (project layer spec §9 and §11.3). It prints a
//! progress line every few seconds and the log as it happens, then exits with 0 when the archive is clean, or 1
//! with open issues.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cryarchiver_engine::text::display_path;
use cryarchiver_project::view::{IssueView, Overall, RunState, Update};
use cryarchiver_project::{Command, Project};
use indicatif::{DecimalBytes, HumanDuration};

const PROGRESS_EVERY: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(250);
const LISTED_ISSUES: usize = 20;

#[derive(Debug, Default)]
struct Summary {
	run_state: Option<RunState>,
	overall: Overall,
	/// Every open issue, unacknowledged unknown destination files and folder cycles included.
	issues: Vec<IssueView>,
	stopped: bool,
}

impl Summary {
	fn apply(&mut self, update: Update, out: &mut impl Write) {
		match update {
			Update::Status(status) => self.run_state = Some(status.run_state),
			Update::Overall(overall) => self.overall = overall,
			Update::Issues(issues) => self.issues = issues,
			Update::Log(entry) => {
				let _ = writeln!(out, "{}  {}", clock(entry.time), entry.message);
			}
			Update::Notice(notice) => {
				let _ = writeln!(out, "{:?}: {}", notice.level, notice.text);
			}
			Update::Stopped => self.stopped = true,
			_ => {}
		}
	}

	fn finished(&self) -> bool {
		self.run_state == Some(RunState::Finished)
	}

	/// Source files the completion check found no archived file for, files with issues included.
	fn gaps(&self) -> u64 {
		self.overall.parity.map_or(0, |parity| {
			parity
				.source_files
				.saturating_sub(parity.archive_files + parity.excluded)
		})
	}

	fn clean(&self) -> bool {
		self.issues.is_empty() && self.gaps() == 0
	}

	fn progress_line(&self) -> String {
		let overall = &self.overall;
		let line = format!(
			"[{:>3}%] {}/{}{} files · {} active · {} issues · saved {}",
			(overall.progress * 100.0).floor(),
			overall.files_done,
			overall.files_total,
			if overall.scan_complete { "" } else { "+" },
			overall.files_active,
			self.issues.len(),
			DecimalBytes(overall.saved_bytes)
		);

		match overall.eta {
			Some(eta) => format!("{line} · ETA {}", HumanDuration(eta)),
			None => line,
		}
	}

	fn final_lines(&self, deleting: bool) -> Vec<String> {
		let mut lines = Vec::new();
		match self.overall.parity {
			Some(parity) if parity.excluded > 0 => lines.push(format!(
				"{} source files → {} archived + {} excluded",
				parity.source_files, parity.archive_files, parity.excluded
			)),
			Some(parity) => lines.push(format!(
				"{} source files → {} archived",
				parity.source_files, parity.archive_files
			)),
			None => lines.push("stopped before the archive was complete; run again to resume".into()),
		}

		if self.gaps() > 0 {
			lines.push(format!("{} source files have no archived file", self.gaps()));
		}

		if !self.issues.is_empty() {
			lines.push(format!(
				"{} open issues: open the project in the TUI to resolve them",
				self.issues.len()
			));
			for issue in self.issues.iter().take(LISTED_ISSUES) {
				lines.push(format!("  {}: {}", issue.location, issue.reason));
			}
			if self.issues.len() > LISTED_ISSUES {
				lines.push(format!("  … and {} more", self.issues.len() - LISTED_ISSUES));
			}
		}

		if deleting {
			lines.push(format!("{} originals deleted", self.overall.originals_deleted));
		}
		lines
	}
}

/// Refuses to start unless the project's delete-originals setting and the flag agree.
pub fn check_delete_confirmation(setting: bool, flag: bool) -> Result<(), &'static str> {
	match (setting, flag) {
		(true, false) => Err("this project deletes originals: repeat --delete-originals to run it"),
		(false, true) => Err("--delete-originals needs the project's setting on; turn it on in the TUI's settings"),
		_ => Ok(()),
	}
}

pub fn run(project: &mut Project, delete_originals: bool, interrupted: &Arc<AtomicBool>) -> u8 {
	let mut out = io::stdout();
	if let Err(problem) = check_delete_confirmation(project.settings().delete_originals, delete_originals) {
		eprintln!("error: {problem}");
		return 2;
	}

	if delete_originals {
		eprintln!(
			"warning: DELETING ORIGINALS in {}: each original is deleted permanently, with no Recycle Bin, once its archived copy checks out",
			display_path(&project.settings().source)
		);
	}

	project.send(Command::Start {
		delete_confirmed: delete_originals,
	});

	let mut summary = Summary::default();
	let mut last_progress = Instant::now();
	let stopped_by_user = loop {
		if interrupted.load(Ordering::SeqCst) {
			let _ = writeln!(out, "stopping: running jobs are requeued and resume on the next run");
			break true;
		}

		if let Some(update) = project.wait_update(POLL) {
			summary.apply(update, &mut out);
		}
		if summary.finished() || summary.stopped {
			break false;
		}

		if last_progress.elapsed() >= PROGRESS_EVERY {
			last_progress = Instant::now();
			let _ = writeln!(out, "{}", summary.progress_line());
		}
	};

	let finished = summary.finished();
	let watching = summary.run_state == Some(RunState::Watching);
	project.shutdown();
	while let Some(update) = project.try_update() {
		summary.apply(update, &mut out);
	}
	for line in summary.final_lines(delete_originals) {
		let _ = writeln!(out, "{line}");
	}

	let clean = summary.clean() && (finished || (stopped_by_user && watching));
	u8::from(!clean)
}

fn clock(seconds: u32) -> String {
	format!(
		"{:02}:{:02}:{:02}",
		seconds / 3600 % 24,
		seconds / 60 % 60,
		seconds % 60
	)
}

#[cfg(test)]
mod tests {
	use cryarchiver_project::issues::{IssueId, Problem};
	use cryarchiver_project::view::Parity;

	use super::*;

	fn unknown_file(location: &str) -> IssueView {
		IssueView {
			id: IssueId(1),
			problem: Problem::UnknownFile,
			file: None,
			folder: None,
			name: location.into(),
			location: location.into(),
			reason: "a file in the destination that the project didn't write".into(),
			resolution: String::new(),
			verbs: Problem::UnknownFile.verbs(),
			attempts: 0,
		}
	}

	#[test]
	fn the_delete_flag_and_the_setting_must_agree() {
		assert!(check_delete_confirmation(true, false).is_err());
		assert!(check_delete_confirmation(false, true).is_err());
		assert!(check_delete_confirmation(true, true).is_ok());
		assert!(check_delete_confirmation(false, false).is_ok());
	}

	#[test]
	fn the_summary_names_parity_issues_and_deletions() {
		let summary = Summary {
			overall: Overall {
				parity: Some(Parity {
					source_files: 10,
					archive_files: 8,
					excluded: 2,
				}),
				originals_deleted: 8,
				..Overall::default()
			},
			issues: vec![unknown_file("old/notes.txt")],
			..Summary::default()
		};

		assert!(!summary.clean());
		assert_eq!(
			summary.final_lines(true),
			[
				"10 source files → 8 archived + 2 excluded",
				"1 open issues: open the project in the TUI to resolve them",
				"  old/notes.txt: a file in the destination that the project didn't write",
				"8 originals deleted",
			]
		);
	}

	#[test]
	fn source_files_without_an_archived_file_are_not_clean() {
		let summary = Summary {
			overall: Overall {
				parity: Some(Parity {
					source_files: 3,
					archive_files: 2,
					excluded: 0,
				}),
				..Overall::default()
			},
			..Summary::default()
		};

		assert!(!summary.clean());
		assert_eq!(
			summary.final_lines(false),
			["3 source files → 2 archived", "1 source files have no archived file"]
		);
	}

	#[test]
	fn the_progress_line_shows_an_open_total_while_scanning() {
		let summary = Summary {
			overall: Overall {
				files_done: 5,
				files_total: 20,
				progress: 0.259,
				..Overall::default()
			},
			..Summary::default()
		};

		assert_eq!(
			summary.progress_line(),
			"[ 25%] 5/20+ files · 0 active · 0 issues · saved 0 B"
		);
	}
}
