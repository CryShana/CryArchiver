use std::collections::{BTreeMap, VecDeque};

use super::issues::{NewIssue, attempt};
use super::rng::Rng;
use super::store::FileStore;
use super::{Job, MediaKind, Phase, Showcase};
use crate::format::as_f64;
use crate::view::{FileId, FileState, FileStatus, IssueKind, Library, NodeId, WorkKind};

pub struct Built {
	pub library: Library,
	pub files: FileStore,
	pub showcase: BTreeMap<FileId, Showcase>,
	pub queue: VecDeque<FileId>,
	pub jobs: Vec<Job>,
	pub issues: Vec<NewIssue<'static>>,
	pub duplicate_of: BTreeMap<FileId, FileId>,
	pub scan_target: NodeId,
}

struct Builder {
	library: Library,
	files: FileStore,
	showcase: BTreeMap<FileId, Showcase>,
	queue: VecDeque<FileId>,
	jobs: Vec<Job>,
	issues: Vec<NewIssue<'static>>,
	duplicate_of: BTreeMap<FileId, FileId>,
	rng: Rng,
	last_done_photo: Option<FileId>,
}

pub struct Generated {
	pub count: u64,
	pub done: f64,
	pub kind: MediaKind,
	pub name: fn(u64) -> String,
}

pub fn build(rng: Rng) -> Built {
	let mut builder = Builder {
		library: Library::new("Family"),
		files: FileStore::default(),
		showcase: BTreeMap::new(),
		queue: VecDeque::new(),
		jobs: Vec::new(),
		issues: Vec::new(),
		duplicate_of: BTreeMap::new(),
		rng,
		last_done_photo: None,
	};
	let root = builder.library.root();

	let photos = builder.library.add_folder(root, "Photos");
	let videos = builder.library.add_folder(root, "Videos");
	let music = builder.library.add_folder(root, "Music");
	let screenshots = builder.library.add_folder(root, "Screenshots");
	let documents = builder.library.add_folder(root, "Documents");

	builder.photos(photos);
	builder.videos(videos);
	builder.music(music);
	let scan_target = builder.screenshots(screenshots);
	builder.documents(documents);

	Built {
		library: builder.library,
		files: builder.files,
		showcase: builder.showcase,
		queue: builder.queue,
		jobs: builder.jobs,
		issues: builder.issues,
		duplicate_of: builder.duplicate_of,
		scan_target,
	}
}

impl Builder {
	fn file(&mut self, folder: NodeId, name: &str, kind: MediaKind, state: FileState, bytes_in: u64) -> FileId {
		let bytes_out = match state {
			FileState::Done | FileState::Warning => Some(kind.output_size(bytes_in, &mut self.rng)),
			FileState::KeptOriginal | FileState::Copied => Some(bytes_in),
			FileState::Duplicate => Some(96),
			_ => None,
		};
		let status = FileStatus {
			state,
			progress: None,
			codec: kind.codec().map(str::to_owned),
			bytes_in,
			bytes_out,
		};

		let id = self.files.add(folder, name.to_owned(), status, kind);

		if state == FileState::Queued {
			self.queue.push_back(id);
		}

		if state == FileState::Done && kind == MediaKind::Photo {
			self.last_done_photo = Some(id);
		}
		id
	}

	fn active(&mut self, folder: NodeId, name: &str, kind: MediaKind, bytes_in: u64, phase: Phase) -> FileId {
		let (state, progress) = match phase {
			Phase::Search { .. } => (FileState::QualitySearch, None),
			Phase::Encode { fraction } => (FileState::Encoding, Some(fraction)),
			Phase::Verify { fraction } => (FileState::Verifying, Some(fraction)),
		};

		let id = self.file(folder, name, kind, state, bytes_in);
		self.files.status_mut(id).progress = progress;

		let fps = if kind == MediaKind::Video {
			60.0 + self.rng.range_f64(0.0, 90.0)
		} else {
			0.0
		};
		self.jobs.push(Job {
			file: id,
			work: WorkKind::Encode,
			phase,
			fps,
			over_limit: false,
		});
		id
	}

	fn show(&mut self, id: FileId, source_info: &'static str, plan: &'static [(&'static str, &'static str)]) {
		self.showcase.insert(
			id,
			Showcase {
				source_info,
				plan,
				quality: None,
				note: None,
			},
		);
	}

	fn quality(&mut self, id: FileId, quality: &'static str) {
		if let Some(showcase) = self.showcase.get_mut(&id) {
			showcase.quality = Some(quality);
		}
	}

	fn note(&mut self, id: FileId, note: &'static str) {
		if let Some(showcase) = self.showcase.get_mut(&id) {
			showcase.note = Some(note);
		}
	}

	fn file_issue(
		&mut self,
		file: FileId,
		kind: IssueKind,
		reason: &'static str,
		attempts: Vec<super::issues::AttemptLog>,
	) {
		let folder = self.files.folder(file);
		self.issues.push(NewIssue {
			kind,
			file: Some(file),
			folder: Some(folder),
			name: self.files.get(file).name.clone(),
			location: self.library.relative_path(folder),
			reason,
			attempts,
		});
	}

	fn generate(&mut self, folder: NodeId, spec: &Generated) {
		for i in 0..spec.count {
			let name = (spec.name)(i);
			let bytes_in = spec.kind.input_size(&mut self.rng);
			let state = self.generated_state(spec, i);

			let id = self.file(folder, &name, spec.kind, state, bytes_in);
			if state == FileState::Duplicate
				&& let Some(canonical) = self.last_done_photo
			{
				self.duplicate_of.insert(id, canonical);
			}
		}
	}

	fn generated_state(&mut self, spec: &Generated, index: u64) -> FileState {
		let done_count = (as_f64(spec.count) * spec.done).round();
		if as_f64(index) >= done_count {
			return FileState::Queued;
		}

		let roll = self.rng.unit();
		match spec.kind {
			MediaKind::Document => FileState::Copied,
			MediaKind::Photo if roll < 0.015 && self.last_done_photo.is_some() => FileState::Duplicate,
			_ if roll < 0.035 => FileState::KeptOriginal,
			_ => FileState::Done,
		}
	}
}

impl Builder {
	fn photos(&mut self, photos: NodeId) {
		let y2019 = self.library.add_folder(photos, "2019");
		let y2020 = self.library.add_folder(photos, "2020");
		let y2021 = self.library.add_folder(photos, "2021");

		self.generate(
			y2019,
			&Generated {
				count: 3120,
				done: 1.0,
				kind: MediaKind::Photo,
				name: |i| format!("DSC_{:04}.JPG", 1000 + i),
			},
		);
		self.output_missing(y2019);

		let dune_folder = self.library.add_folder(y2020, "Dune");
		let everyday = self.library.add_folder(y2020, "Everyday");
		self.dune_folder(dune_folder);
		self.generate(
			everyday,
			&Generated {
				count: 4377,
				done: 0.72,
				kind: MediaKind::Photo,
				name: |i| format!("IMG_{:04}.JPG", 2000 + i),
			},
		);

		self.generate(
			y2021,
			&Generated {
				count: 4952,
				done: 0.51,
				kind: MediaKind::Photo,
				name: |i| format!("IMG_{:04}.HEIC", 6500 + i),
			},
		);
	}

	fn output_missing(&mut self, folder: NodeId) {
		let Some(file) = self.files.in_folder(folder).get(4).map(|row| row.id) else {
			return;
		};

		self.file_issue(
			file,
			IssueKind::OutputMissing,
			"Destination output missing: DSC_1004.avif was deleted outside CryArchiver",
			vec![attempt(&[
				"full reconcile: DSC_1004.avif not found",
				"recorded size 2.31 MB",
			])],
		);
	}

	fn dune_folder(&mut self, dune_folder: NodeId) {
		let done = self.file(
			dune_folder,
			"beach_0142.jpg",
			MediaKind::Photo,
			FileState::Done,
			12_300_000,
		);
		self.show(
			done,
			"JPEG 6000×4000 · EXIF + GPS",
			&[("Plan", "AVIF 10-bit 4:4:4 · ravif speed 5")],
		);
		self.quality(done, "SSIMULACRA2 86.4 (target 85)");

		let photo = Phase::Encode { fraction: 0.64 };
		let encoding = self.active(dune_folder, "beach_0143.jpg", MediaKind::Photo, 11_900_000, photo);
		self.show(
			encoding,
			"JPEG 6000×4000 · MPF preview dropped",
			&[("Plan", "AVIF 10-bit 4:4:4 · ravif speed 5")],
		);

		let movie = self.active(
			dune_folder,
			"birthday clip 2.mov",
			MediaKind::Video,
			56_800_000,
			Phase::Encode { fraction: 0.38 },
		);
		self.show(
			movie,
			"HEVC 10-bit 1920×1080 29.97 · HLG · Dolby Vision 8.4 · rotated −90°",
			&[
				("Plan", "AV1 10-bit · SVT preset 5 · CRF 31.25 · tune VQ · keyint 5s"),
				("Audio", "AAC 2.0 → Opus 128k"),
				("Extra", "3 mebx tracks → attachments · DV RPU dropped"),
				("Group", "1080p · 30 · hevc · HLG · light (cached CRF, 4 files)"),
			],
		);

		let tall = self.file(
			dune_folder,
			"comic-strip-tall.jpg",
			MediaKind::TallPhoto,
			FileState::Done,
			34_900_000,
		);
		self.show(
			tall,
			"JPEG 1293×48277 (too tall for AVIF)",
			&[("Plan", "JXL lossy · distance 1.0 · effort 7")],
		);
		self.quality(tall, "SSIMULACRA2 87.1");

		let big = self.file(
			dune_folder,
			"city-map-large.jpg",
			MediaKind::Photo,
			FileState::Queued,
			14_500_000,
		);
		self.show(
			big,
			"JPEG 10800×5400 · 58 MP",
			&[("Plan", "AVIF 10-bit · ravif speed 5 · ~2.4 GB RAM reserved")],
		);

		let kept = self.file(
			dune_folder,
			"screenshot-profiled.avif",
			MediaKind::Photo,
			FileState::KeptOriginal,
			890_000,
		);
		self.show(kept, "AVIF 1920×1080 · display ICC profile", &[]);
		self.note(kept, "saving 4 % is below the 10 % minimum");

		let damaged = self.file(
			dune_folder,
			"half-copied.jpg",
			MediaKind::Photo,
			FileState::SourceDamaged,
			1_260_000,
		);
		self.show(damaged, "JPEG 2546×1800", &[]);
		self.note(damaged, "no EOI marker");
		self.file_issue(
			damaged,
			IssueKind::SourceDamaged,
			"Source damaged: JPEG has no EOI marker",
			vec![attempt(&[
				"[mjpeg] EOI missing, emulating",
				"[mjpeg] overread 8",
				"decoded 900 of 1800 rows",
				"structural check: EOI not found after last SOS",
			])],
		);

		self.generate(
			dune_folder,
			&Generated {
				count: 24,
				done: 0.5,
				kind: MediaKind::Photo,
				name: |i| format!("beach_{:04}.jpg", 144 + i),
			},
		);
	}

	fn videos(&mut self, videos: NodeId) {
		let gopro = self.library.add_folder(videos, "GoPro");
		let concerts = self.library.add_folder(videos, "Concerts");
		let compressed = self.library.add_folder(videos, "already-compressed");

		let mountains = self.library.add_folder(gopro, "2018 Mountains");
		let raw = self.library.add_folder(gopro, "Raw");
		self.mountains(mountains);
		self.generate(
			raw,
			&Generated {
				count: 200,
				done: 0.45,
				kind: MediaKind::Video,
				name: |i| format!("GOPR{:04}.MP4", 200 + i),
			},
		);

		self.concerts(concerts);
		self.already_compressed(compressed);
		self.concealed_errors(videos);

		self.issues.push(NewIssue {
			kind: IssueKind::UnknownFile,
			file: None,
			folder: Some(videos),
			name: "desktop.ini".to_owned(),
			location: "Videos (destination)".to_owned(),
			reason: "Unknown file in the destination: CryArchiver didn't write it",
			attempts: Vec::new(),
		});
	}

	fn mountains(&mut self, mountains: NodeId) {
		let name = "2018 - mountain trail - GoPro.mkv";
		let encoding = self.active(
			mountains,
			name,
			MediaKind::Video,
			1_810_000_000,
			Phase::Encode { fraction: 0.71 },
		);
		self.show(
			encoding,
			"AV1 1920×1080 119.88 fps · 34 Mbps (inefficient)",
			&[
				("Plan", "AV1 10-bit · SVT preset 5 · CRF 30.75"),
				("Audio", "Opus 2.0 copied"),
				("Group", "1080p · 120 · av1 · clean"),
			],
		);

		let queued = self.file(
			mountains,
			"GOPR0042.MP4",
			MediaKind::Video,
			FileState::Queued,
			3_900_000_000,
		);
		self.show(
			queued,
			"H.264 3840×2160 59.94 · GPMF telemetry",
			&[("Plan", "AV1 10-bit · SVT preset 5 · telemetry → attachment")],
		);

		self.generate(
			mountains,
			&Generated {
				count: 5,
				done: 1.0,
				kind: MediaKind::Video,
				name: |i| format!("GOPR{:04}.MP4", 43 + i),
			},
		);
	}

	fn concerts(&mut self, concerts: NodeId) {
		let name = "concert_1080i.mkv";
		let searching = self.active(
			concerts,
			name,
			MediaKind::Video,
			135_000_000,
			Phase::Search { probe: 3 },
		);
		self.show(
			searching,
			"MPEG-2 1920×1080i TFF 29.97 · AC3 5.1(side)",
			&[
				("Plan", "bwdif → 59.94p · AV1 10-bit · SVT preset 5"),
				("Audio", "AC3 5.1(side) → Opus 5.1 256k (relabelled)"),
				("Group", "1080p · 60 · mpeg2 · interlaced · light (new group)"),
			],
		);

		let failed = self.file(
			concerts,
			"concert_1998.rm",
			MediaKind::Video,
			FileState::Failed,
			412_000_000,
		);
		self.show(
			failed,
			"RealVideo 4 352×288 25 fps · Cook audio",
			&[("Plan", "AV1 10-bit · SVT preset 5")],
		);
		self.note(failed, "encode failed the same way 3 times");
		let failure = [
			"[rv40] Invalid data found when processing input",
			"Error while decoding stream #0:0",
		];
		self.file_issue(
			failed,
			IssueKind::EncodeFailed,
			"Encode failed the same way 3 times",
			vec![
				attempt(&[failure[0], failure[1], "Conversion failed!", "exit code 1"]),
				attempt(&[failure[0], "Conversion failed!", "exit code 1 (retry after 30 s)"]),
				attempt(&[
					failure[0],
					failure[1],
					"Conversion failed!",
					"exit code 1 (attempt 3/3)",
				]),
			],
		);

		self.generate(
			concerts,
			&Generated {
				count: 10,
				done: 0.3,
				kind: MediaKind::Video,
				name: |i| format!("live_{:02}.mkv", 1 + i),
			},
		);
	}

	fn already_compressed(&mut self, compressed: NodeId) {
		let name = "short-film.webm";
		let kept = self.file(compressed, name, MediaKind::Video, FileState::KeptOriginal, 502_000_000);
		self.show(kept, "AV1 1920×960 24 fps · 4.4 Mbps", &[]);
		self.note(kept, "already efficient: predicted saving 3 %");

		for i in 0..8 {
			let name = format!("trailer_{:02}.webm", 1 + i);
			let bytes_in = MediaKind::Video.input_size(&mut self.rng);
			self.file(compressed, &name, MediaKind::Video, FileState::KeptOriginal, bytes_in);
		}
	}

	fn concealed_errors(&mut self, videos: NodeId) {
		let name = "glitched-recording.mp4";
		let warned = self.file(videos, name, MediaKind::Video, FileState::Warning, 18_400_000);
		self.files.status_mut(warned).bytes_out = Some(6_100_000);
		self.show(
			warned,
			"H.264 1280×720 29.97 · 64 KiB of garbage in the middle",
			&[("Plan", "AV1 10-bit · SVT preset 5 · CRF 32.00")],
		);
		self.quality(warned, "VMAF 94.8 · p5 88.0");
		self.note(warned, "212 decoder errors concealed · glitches carried over");
	}

	fn music(&mut self, music: NodeId) {
		let ost = self.library.add_folder(music, "Anime OST");
		let live = self.library.add_folder(music, "Live");

		let first = self.file(ost, "01.さくら.mp3", MediaKind::Audio, FileState::Done, 1_470_000);
		self.show(
			first,
			"MP3 342 kbps · cover 492×704",
			&[("Plan", "Opus 128k · cover + ID3 tags kept")],
		);

		let second = self.file(ost, "02. 夜明け.mp3", MediaKind::Audio, FileState::Done, 3_670_000);
		self.show(
			second,
			"MP3 384 kbps · cover 849×1200",
			&[("Plan", "Opus 128k · cover + ID3 tags kept")],
		);

		let name = "battle-theme-11.mp3";
		let duplicate = self.file(ost, name, MediaKind::Audio, FileState::Duplicate, 1_150_000);
		self.generate(
			ost,
			&Generated {
				count: 81,
				done: 0.96,
				kind: MediaKind::Audio,
				name: |i| format!("{:02}. track.mp3", 3 + i),
			},
		);
		self.generate(
			live,
			&Generated {
				count: 2124,
				done: 0.78,
				kind: MediaKind::Audio,
				name: |i| format!("set_{}_{:03}.flac", 2015 + i / 300, i % 300 + 1),
			},
		);

		let copy = "battle-theme-11 (copy).mp3";
		let second = self.file(ost, copy, MediaKind::Audio, FileState::Duplicate, 1_150_000);

		let canonical = self.file(music, name, MediaKind::Audio, FileState::Done, 1_150_000);
		self.duplicate_of.insert(duplicate, canonical);
		self.duplicate_of.insert(second, canonical);
	}

	fn screenshots(&mut self, screenshots: NodeId) -> NodeId {
		let game = self.library.add_folder(screenshots, "Cyberpunk 2077");
		let desktop = self.library.add_folder(screenshots, "Desktop");

		let done = self.file(
			game,
			"game-preview.png",
			MediaKind::Screenshot,
			FileState::Done,
			4_500_000,
		);
		self.show(
			done,
			"PNG 1883×1028 RGBA (alpha all 255)",
			&[("Plan", "opaque AVIF · alpha dropped")],
		);
		self.quality(done, "SSIMULACRA2 88.0");

		let verify = Phase::Verify { fraction: 0.9 };
		let name = "photo-mode-001.png";
		let verifying = self.active(game, name, MediaKind::Screenshot, 4_500_000, verify);
		self.show(verifying, "PNG 2560×1440 RGBA", &[("Plan", "AVIF 10-bit with alpha")]);

		self.generate(
			game,
			&Generated {
				count: 38,
				done: 0.6,
				kind: MediaKind::Screenshot,
				name: |i| format!("photo-mode-{:03}.png", 2 + i),
			},
		);
		self.generate(
			desktop,
			&Generated {
				count: super::SCAN_BATCH_INDEX,
				done: 0.21,
				kind: MediaKind::Screenshot,
				name: screenshot_name,
			},
		);

		self.issues.push(NewIssue {
			kind: IssueKind::DirectoryCycle,
			file: None,
			folder: Some(desktop),
			name: "Desktop/loop".to_owned(),
			location: "Screenshots/Desktop".to_owned(),
			reason: "Directory cycle: the junction loop points back at Screenshots",
			attempts: Vec::new(),
		});
		desktop
	}

	fn documents(&mut self, documents: NodeId) {
		let inbox = self.library.add_folder(documents, "Inbox");

		self.file(
			documents,
			"taxes_2021.pdf",
			MediaKind::Document,
			FileState::Copied,
			1_200_000,
		);
		self.generate(
			documents,
			&Generated {
				count: 462,
				done: 1.0,
				kind: MediaKind::Document,
				name: |i| format!("scan_{:03}.pdf", 1 + i),
			},
		);

		let locked = self.file(
			documents,
			"locked_contract.pdf",
			MediaKind::Document,
			FileState::Failed,
			2_400_000,
		);
		self.file_issue(
			locked,
			IssueKind::SourceUnreadable,
			"Source unreadable: access is denied",
			vec![attempt(&[
				"open D:\\Archive\\Family\\Documents\\locked_contract.pdf",
				"os error 5: access is denied",
			])],
		);

		self.generate(
			inbox,
			&Generated {
				count: 40,
				done: 0.0,
				kind: MediaKind::Document,
				name: |i| format!("letter_{:02}.docx", 1 + i),
			},
		);
	}
}

pub fn screenshot_name(index: u64) -> String {
	let day = index / 96;
	let (year, day_of_year) = (2019 + day / 360, day % 360);
	let minutes = index % 96 * 15;
	format!(
		"Screenshot {year}-{:02}-{:02} {:02}{:02}{:02}.png",
		day_of_year / 30 + 1,
		day_of_year % 30 + 1,
		minutes / 60,
		minutes % 60,
		index * 37 % 60
	)
}
