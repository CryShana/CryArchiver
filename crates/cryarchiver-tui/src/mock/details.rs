use std::time::Duration;

use super::sim::PROBES;
use super::{MediaKind, MockSource, Phase};
use crate::format::{as_f64, bytes, round_to_u64};
use crate::source::Target;
use crate::view::{
	Duplicates, FileDetails, FileId, FileState, FileStatus, IssueSummary, LiveProgress, PathLink, PlanLine, StageMark,
	StageProgress, WorkKind,
};

const STAGES: [&str; 10] = [
	"detect", "probe", "analyze", "plan", "search", "encode", "verify", "mux", "meta", "commit",
];

pub fn build(source: &MockSource, file: FileId) -> Option<FileDetails> {
	if usize::try_from(file.0).ok()? >= source.files.len() {
		return None;
	}

	let status = &source.files.get(file).status;
	let kind = source.kind(file);
	let showcase = source.showcase.get(&file);

	Some(FileDetails {
		source_path: source.source_path(file),
		destination_path: source.destination_path(file, status.state),
		source_info: showcase.map_or_else(|| generic_source_info(kind).to_owned(), |s| s.source_info.to_owned()),
		state: status.state,
		stages: stages(status.state),
		live: source.live(file),
		bytes_in: status.bytes_in,
		bytes_out: status.bytes_out,
		estimated_out: status
			.state
			.is_active()
			.then(|| round_to_u64(as_f64(status.bytes_in) * 0.33)),
		plan: plan(source, file, kind),
		quality: quality(source, file, status, kind),
		log_tail: source.log_tail(file, status),
		notes: Vec::new(),
		duplicates: source.duplicates(file),
		issue: source
			.view
			.issues
			.iter()
			.find(|issue| issue.file == Some(file))
			.map(|issue| IssueSummary {
				id: issue.id,
				kind: issue.kind,
				reason: issue.reason.clone(),
				resolution: issue.resolution.clone(),
				verbs: issue.verbs.clone(),
			}),
	})
}

impl MockSource {
	fn file_path(&self, file: FileId) -> String {
		let folder = self.view.library.relative_path(self.files.folder(file));
		let name = &self.files.get(file).name;
		if folder.is_empty() {
			name.clone()
		} else {
			format!("{folder}/{name}")
		}
	}

	pub(super) fn source_path(&self, file: FileId) -> String {
		join_windows(&self.view.header.source, &self.file_path(file))
	}

	pub(super) fn source_folder(&self, target: Target) -> String {
		self.folder_of(target, &self.view.header.source)
	}

	pub(super) fn output_folder(&self, target: Target) -> String {
		self.folder_of(target, &self.view.header.destination)
	}

	fn folder_of(&self, target: Target, root: &str) -> String {
		let folder = match target {
			Target::Folder(folder) => folder,
			Target::File(file) => self.files.folder(file),
		};
		join_windows(root, &self.view.library.relative_path(folder))
	}

	fn destination_path(&self, file: FileId, state: FileState) -> String {
		let folder = self.view.library.relative_path(self.files.folder(file));
		let name = self.files.get(file).name.as_str();
		let output_name = match state {
			FileState::Duplicate => format!("placeholder_{name}.txt"),
			FileState::KeptOriginal | FileState::Copied | FileState::SourceDamaged | FileState::Failed => {
				name.to_owned()
			}
			_ => with_extension(name, self.kind(file)),
		};

		let joined = if folder.is_empty() {
			output_name
		} else {
			format!("{folder}/{output_name}")
		};
		join_windows(&self.view.header.destination, &joined)
	}

	fn live(&self, file: FileId) -> Option<LiveProgress> {
		let job = self.jobs.iter().find(|job| job.file == file)?;
		let video = self.kind(file).is_video() && job.work == WorkKind::Encode;
		let (fraction, rate) = match job.phase {
			Phase::Search { .. } => return None,
			Phase::Encode { fraction } => (fraction, if video { 0.026 } else { 0.21 }),
			Phase::Verify { fraction } => (fraction.min(0.99), if video { 0.11 } else { 0.47 }),
		};
		let remaining_ticks = (1.0 - fraction) / rate;

		Some(LiveProgress {
			fraction,
			fps: video.then_some(job.fps),
			speed: video.then_some(job.fps / 30.0),
			eta: Some(Duration::from_millis(round_to_u64(remaining_ticks * 500.0))),
		})
	}

	fn log_tail(&self, file: FileId, status: &FileStatus) -> Vec<String> {
		let issue = self.view.issues.iter().find(|issue| issue.file == Some(file));
		if let Some(last) = issue
			.and_then(|issue| self.issue_logs.get(&issue.id))
			.and_then(|logs| logs.last())
		{
			return last.clone();
		}

		let job = self.jobs.iter().find(|job| job.file == file);
		match job.map(|job| (job.work, job.phase, job.fps)) {
			Some((WorkKind::Encode, Phase::Search { probe }, _)) => probe_lines(probe),
			Some((WorkKind::Encode, Phase::Encode { fraction }, fps)) => encode_lines(fraction, fps, status.bytes_in),
			Some((WorkKind::Encode, Phase::Verify { fraction }, _)) => vec![
				"metric pass: comparing against the source".to_owned(),
				format!("frames compared: {:.0} %", fraction.min(0.99) * 100.0),
			],
			Some((work, _, _)) => vec![format!("{} job running", work.label())],
			None => {
				let note = self.showcase.get(&file).and_then(|showcase| showcase.note);
				vec![
					"(no running process)".to_owned(),
					note.unwrap_or(finished_note(status.state)).to_owned(),
				]
			}
		}
	}

	fn duplicates(&self, file: FileId) -> Duplicates {
		if let Some(&canonical) = self.duplicate_of.get(&file) {
			return Duplicates::PlaceholderOf(self.link(canonical));
		}

		let copies: Vec<PathLink> = self
			.duplicate_of
			.iter()
			.filter(|&(_, &canonical)| canonical == file)
			.map(|(&copy, _)| self.link(copy))
			.collect();

		if copies.is_empty() {
			Duplicates::None
		} else {
			Duplicates::CanonicalOf(copies)
		}
	}

	fn link(&self, file: FileId) -> PathLink {
		PathLink {
			file,
			folder: self.files.folder(file),
			path: self.file_path(file),
		}
	}
}

fn plan(source: &MockSource, file: FileId, kind: MediaKind) -> Vec<PlanLine> {
	let line = |label: &str, value: &str| PlanLine {
		label: label.to_owned(),
		value: value.to_owned(),
	};

	let Some(showcase) = source.showcase.get(&file) else {
		return generic_plan(kind)
			.iter()
			.map(|&(label, value)| line(label, value))
			.collect();
	};

	let mut lines: Vec<PlanLine> = showcase.plan.iter().map(|&(label, value)| line(label, value)).collect();
	if let Some(note) = showcase.note {
		lines.push(line("Note", note));
	}
	lines
}

fn quality(source: &MockSource, file: FileId, status: &FileStatus, kind: MediaKind) -> Option<String> {
	if let Some(quality) = source.showcase.get(&file).and_then(|showcase| showcase.quality) {
		return Some(quality.to_owned());
	}

	let target = match kind {
		MediaKind::Video => "VMAF ≥ 95 · p5 ≥ 90",
		MediaKind::Audio | MediaKind::Document => return None,
		MediaKind::Photo | MediaKind::TallPhoto | MediaKind::Screenshot => "SSIMULACRA2 ≥ 85",
	};

	match status.state {
		FileState::Done if kind.is_video() => Some("VMAF 95.6 · p5 91.2".to_owned()),
		FileState::Done => Some("SSIMULACRA2 86.9".to_owned()),
		_ => Some(format!("target {target}")),
	}
}

fn stages(state: FileState) -> Vec<StageMark> {
	let current = match state {
		FileState::Done
		| FileState::KeptOriginal
		| FileState::Copied
		| FileState::Duplicate
		| FileState::Warning
		| FileState::SourceGone
		| FileState::DeletePending
		| FileState::OriginalDeleted => STAGES.len(),
		FileState::QualitySearch => 4,
		FileState::Encoding | FileState::Failed => 5,
		FileState::Verifying => 6,
		FileState::SourceDamaged => 1,
		FileState::Waiting => 3,
		FileState::Queued | FileState::Excluded => 0,
	};
	let has_current =
		!matches!(state, FileState::Queued | FileState::Waiting | FileState::Excluded) && current < STAGES.len();

	(0..STAGES.len())
		.map(|index| StageMark {
			name: STAGES[index].to_owned(),
			progress: if index < current {
				StageProgress::Done
			} else if index == current && has_current {
				StageProgress::Current
			} else {
				StageProgress::Pending
			},
		})
		.collect()
}

fn probe_lines(probe: u32) -> Vec<String> {
	PROBES
		.iter()
		.take(probe as usize)
		.enumerate()
		.map(|(index, (crf, score))| format!("probe {}: CRF {crf:.2} → VMAF {score:.1}", index + 1))
		.collect()
}

fn encode_lines(fraction: f64, fps: f64, bytes_in: u64) -> Vec<String> {
	let seconds = round_to_u64(fraction * 52.0);
	let written = round_to_u64(as_f64(bytes_in) * fraction * 0.3);
	vec![
		format!(
			"frame={:>6} fps={fps:.0} q=31.2 size={} time=00:00:{seconds:02} speed={:.2}x",
			round_to_u64(fraction * 1560.0),
			bytes(written),
			fps / 30.0
		),
		"progress=continue".to_owned(),
		format!("out_time_us={}", seconds * 1_000_000),
	]
}

fn finished_note(state: FileState) -> &'static str {
	match state {
		FileState::Done => "finished · output verified and committed",
		FileState::KeptOriginal => "kept the original · the output was not smaller enough",
		FileState::Copied => "copied as-is",
		FileState::Duplicate => "placeholder written",
		FileState::Queued => "queued · not started yet",
		FileState::Excluded => "excluded from the archive by your choice",
		FileState::OriginalDeleted => "archive verified · original deleted",
		FileState::SourceGone => "the source was deleted · the archived copy stays",
		_ => "no output yet",
	}
}

fn generic_source_info(kind: MediaKind) -> &'static str {
	match kind {
		MediaKind::Photo => "JPEG 6000×4000 · EXIF",
		MediaKind::TallPhoto => "JPEG 1200×40000",
		MediaKind::Video => "H.264 3840×2160 59.94 · AAC 2.0",
		MediaKind::Audio => "FLAC 44.1 kHz 16-bit · stereo",
		MediaKind::Screenshot => "PNG 2560×1440 RGB",
		MediaKind::Document => "PDF document",
	}
}

fn generic_plan(kind: MediaKind) -> &'static [(&'static str, &'static str)] {
	match kind {
		MediaKind::Photo | MediaKind::Screenshot => &[("Plan", "AVIF 10-bit 4:4:4 · ravif speed 5")],
		MediaKind::TallPhoto => &[("Plan", "JXL lossy · distance 1.0 · effort 7")],
		MediaKind::Video => &[
			("Plan", "AV1 10-bit · SVT preset 5"),
			("Audio", "AAC 2.0 → Opus 128k"),
			("Group", "2160p · 60 · h264 · clean"),
		],
		MediaKind::Audio => &[("Plan", "Opus 128k · tags kept")],
		MediaKind::Document => &[("Plan", "not media · copied as-is")],
	}
}

fn with_extension(name: &str, kind: MediaKind) -> String {
	let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
	let extension = match kind {
		MediaKind::Photo | MediaKind::Screenshot => "avif",
		MediaKind::TallPhoto => "jxl",
		MediaKind::Video => "mkv",
		MediaKind::Audio => "opus",
		MediaKind::Document => return name.to_owned(),
	};
	format!("{stem}.{extension}")
}

fn join_windows(root: &str, relative: &str) -> String {
	if relative.is_empty() {
		return root.to_owned();
	}

	format!("{root}\\{}", relative.replace('/', "\\"))
}
