use std::collections::{BTreeMap, VecDeque};

use super::rng::Rng;
use super::{Job, MediaKind, Phase, Showcase};
use crate::view::{FileState, FileStatus, Issue, Library, NodeId};

pub struct Built {
	pub library: Library,
	pub kinds: Vec<Option<MediaKind>>,
	pub showcase: BTreeMap<NodeId, Showcase>,
	pub queue: VecDeque<NodeId>,
	pub jobs: Vec<Job>,
	pub issues: Vec<Issue>,
	pub duplicate_of: BTreeMap<NodeId, NodeId>,
	pub scan_target: NodeId,
}

struct Builder {
	library: Library,
	kinds: Vec<Option<MediaKind>>,
	showcase: BTreeMap<NodeId, Showcase>,
	queue: VecDeque<NodeId>,
	jobs: Vec<Job>,
	duplicate_of: BTreeMap<NodeId, NodeId>,
	rng: Rng,
	last_done_photo: Option<NodeId>,
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
		kinds: vec![None],
		showcase: BTreeMap::new(),
		queue: VecDeque::new(),
		jobs: Vec::new(),
		duplicate_of: BTreeMap::new(),
		rng,
		last_done_photo: None,
	};
	let root = builder.library.root();

	let photos = builder.folder(root, "Photos");
	let videos = builder.folder(root, "Videos");
	let music = builder.folder(root, "Music");
	let screenshots = builder.folder(root, "Screenshots");
	let documents = builder.folder(root, "Documents");

	let mut issues = Vec::new();
	builder.photos(photos, &mut issues);
	builder.videos(videos, &mut issues);
	builder.music(music);
	let scan_target = builder.screenshots(screenshots);
	builder.documents(documents);

	Built {
		library: builder.library,
		kinds: builder.kinds,
		showcase: builder.showcase,
		queue: builder.queue,
		jobs: builder.jobs,
		issues,
		duplicate_of: builder.duplicate_of,
		scan_target,
	}
}

impl Builder {
	fn folder(&mut self, parent: NodeId, name: &str) -> NodeId {
		let id = self.library.add_folder(parent, name);
		self.kinds.push(None);
		id
	}

	fn file(&mut self, parent: NodeId, name: &str, kind: MediaKind, state: FileState, bytes_in: u64) -> NodeId {
		let bytes_out = match state {
			FileState::Done => Some(kind.output_size(bytes_in, &mut self.rng)),
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

		let id = self.library.add_file(parent, name, status);
		self.kinds.push(Some(kind));

		if state == FileState::Queued {
			self.queue.push_back(id);
		}

		if state == FileState::Done && kind == MediaKind::Photo {
			self.last_done_photo = Some(id);
		}
		id
	}

	fn active(&mut self, parent: NodeId, name: &str, kind: MediaKind, bytes_in: u64, phase: Phase) -> NodeId {
		let (state, progress) = match phase {
			Phase::Search { .. } => (FileState::QualitySearch, None),
			Phase::Encode { fraction } => (FileState::Encoding, Some(fraction)),
			Phase::Verify { fraction } => (FileState::Verifying, Some(fraction)),
		};

		let id = self.file(parent, name, kind, state, bytes_in);
		if let Some(status) = self.library.file_mut(id) {
			status.progress = progress;
		}

		let fps = if kind == MediaKind::Video {
			60.0 + self.rng.range_f64(0.0, 90.0)
		} else {
			0.0
		};
		self.jobs.push(Job { file: id, phase, fps });
		id
	}

	fn showcase(&mut self, id: NodeId, showcase: Showcase) {
		self.showcase.insert(id, showcase);
	}

	fn generate(&mut self, parent: NodeId, spec: &Generated) {
		for i in 0..spec.count {
			let name = (spec.name)(i);
			let bytes_in = spec.kind.input_size(&mut self.rng);
			let state = self.generated_state(spec, i);

			let id = self.file(parent, &name, spec.kind, state, bytes_in);
			if state == FileState::Duplicate
				&& let Some(canonical) = self.last_done_photo
			{
				self.duplicate_of.insert(id, canonical);
			}
		}
	}

	fn generated_state(&mut self, spec: &Generated, index: u64) -> FileState {
		let done_count = (crate::format::as_f64(spec.count) * spec.done).round();
		if crate::format::as_f64(index) >= done_count {
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
	fn photos(&mut self, photos: NodeId, issues: &mut Vec<Issue>) {
		let y2019 = self.folder(photos, "2019");
		let y2020 = self.folder(photos, "2020");
		let y2021 = self.folder(photos, "2021");

		self.generate(
			y2019,
			&Generated {
				count: 3120,
				done: 1.0,
				kind: MediaKind::Photo,
				name: |i| format!("DSC_{:04}.JPG", 1000 + i),
			},
		);

		let bled = self.folder(y2020, "Bled");
		let everyday = self.folder(y2020, "Everyday");
		self.bled(bled, issues);
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

	fn bled(&mut self, bled: NodeId, issues: &mut Vec<Issue>) {
		let done = self.file(bled, "DSC_0001.JPG", MediaKind::Photo, FileState::Done, 12_300_000);
		self.showcase(
			done,
			Showcase {
				source_info: "JPEG 6000×4000 · Nikon D7100 · EXIF + GPS",
				plan: &[("Plan", "AVIF 10-bit 4:4:4 · ravif speed 5")],
				quality: Some("SSIMULACRA2 86.4 (target 85)"),
				note: None,
			},
		);

		let encoding = self.active(
			bled,
			"DSC_0002.JPG",
			MediaKind::Photo,
			11_900_000,
			Phase::Encode { fraction: 0.64 },
		);
		self.showcase(
			encoding,
			Showcase {
				source_info: "JPEG 6000×4000 · Nikon D7100 · MPF preview dropped",
				plan: &[("Plan", "AVIF 10-bit 4:4:4 · ravif speed 5")],
				quality: None,
				note: None,
			},
		);

		let movie = self.active(
			bled,
			"IMG_0001 2.MOV",
			MediaKind::Video,
			56_800_000,
			Phase::Encode { fraction: 0.38 },
		);
		self.showcase(
			movie,
			Showcase {
				source_info: "HEVC 10-bit 1920×1080 29.97 · HLG · Dolby Vision 8.4 · rotated −90°",
				plan: &[
					("Plan", "AV1 10-bit · SVT preset 5 · CRF 31.25 · tune VQ · keyint 5s"),
					("Audio", "AAC 2.0 → Opus 128k"),
					("Extra", "3 mebx tracks → attachments · DV RPU dropped"),
					("Group", "1080p · 30 · hevc · HLG · light (cached CRF, 4 files)"),
				],
				quality: None,
				note: None,
			},
		);

		let tall = self.file(
			bled,
			"comic-veryTallImage.jpg",
			MediaKind::TallPhoto,
			FileState::Done,
			34_900_000,
		);
		self.showcase(
			tall,
			Showcase {
				source_info: "JPEG 1293×48277 (too tall for AVIF)",
				plan: &[("Plan", "JXL lossy · distance 1.0 · effort 7")],
				quality: Some("SSIMULACRA2 87.1"),
				note: None,
			},
		);

		let big = self.file(bled, "large-map.jpg", MediaKind::Photo, FileState::Queued, 14_500_000);
		self.showcase(
			big,
			Showcase {
				source_info: "JPEG 10800×5400 · 58 MP",
				plan: &[("Plan", "AVIF 10-bit · ravif speed 5 · ~2.4 GB RAM reserved")],
				quality: None,
				note: None,
			},
		);

		let kept = self.file(
			bled,
			"screenshot_with_ICC.avif",
			MediaKind::Photo,
			FileState::KeptOriginal,
			890_000,
		);
		self.showcase(
			kept,
			Showcase {
				source_info: "AVIF 1637×954 · ICC AW3423DWF",
				plan: &[],
				quality: None,
				note: Some("saving 4 % is below the 10 % minimum"),
			},
		);

		self.bled_damaged(bled, issues);
		self.generate(
			bled,
			&Generated {
				count: 24,
				done: 0.5,
				kind: MediaKind::Photo,
				name: |i| format!("DSC_{:04}.JPG", 4690 + i),
			},
		);
	}

	fn bled_damaged(&mut self, bled: NodeId, issues: &mut Vec<Issue>) {
		let damaged = self.file(
			bled,
			"truncated_half.jpg",
			MediaKind::Photo,
			FileState::SourceDamaged,
			1_260_000,
		);
		self.showcase(
			damaged,
			Showcase {
				source_info: "JPEG 2546×1800",
				plan: &[],
				quality: None,
				note: Some("no EOI marker · waiting for your decision"),
			},
		);
		issues.push(Issue {
			file: damaged,
			state: FileState::SourceDamaged,
			name: "truncated_half.jpg".to_owned(),
			folder: self.library.relative_path(bled),
			reason: "Source damaged: JPEG has no EOI marker".to_owned(),
			resolution: "Waiting for your decision".to_owned(),
			log: lines(&[
				"[mjpeg] EOI missing, emulating",
				"[mjpeg] overread 8",
				"decoded 900 of 1800 rows",
				"structural check: EOI not found after last SOS",
			]),
		});
	}

	fn videos(&mut self, videos: NodeId, issues: &mut Vec<Issue>) {
		let gopro = self.folder(videos, "GoPro");
		let concerts = self.folder(videos, "Concerts");
		let compressed = self.folder(videos, "already-compressed");

		let holiday = self.folder(gopro, "2018 Holiday");
		let raw = self.folder(gopro, "Raw");
		self.holiday(holiday, issues);
		self.generate(
			raw,
			&Generated {
				count: 200,
				done: 0.45,
				kind: MediaKind::Video,
				name: |i| format!("GOPR{:04}.MP4", 200 + i),
			},
		);

		self.concerts(concerts, issues);
		self.already_compressed(compressed);
		self.concealed_errors(videos, issues);
	}

	fn holiday(&mut self, holiday: NodeId, issues: &mut Vec<Issue>) {
		let name = "2018 - Holiday - GoPro clip.mkv";
		let encoding = self.active(
			holiday,
			name,
			MediaKind::Video,
			1_810_000_000,
			Phase::Encode { fraction: 0.71 },
		);
		self.showcase(
			encoding,
			Showcase {
				source_info: "AV1 1920×1080 119.88 fps · 34 Mbps (inefficient)",
				plan: &[
					("Plan", "AV1 10-bit · SVT preset 5 · CRF 30.75"),
					("Audio", "Opus 2.0 copied"),
					("Group", "1080p · 120 · av1 · clean"),
				],
				quality: None,
				note: None,
			},
		);

		let queued = self.file(
			holiday,
			"GOPR0042.MP4",
			MediaKind::Video,
			FileState::Queued,
			3_900_000_000,
		);
		self.showcase(
			queued,
			Showcase {
				source_info: "H.264 3840×2160 59.94 · GPMF telemetry",
				plan: &[("Plan", "AV1 10-bit · SVT preset 5 · telemetry → attachment")],
				quality: None,
				note: None,
			},
		);

		let waiting = self.file(
			holiday,
			"GOPR0107.MP4",
			MediaKind::Video,
			FileState::Waiting,
			3_900_000_000,
		);
		self.showcase(
			waiting,
			Showcase {
				source_info: "H.264 3840×2160 59.94",
				plan: &[("Plan", "AV1 10-bit · SVT preset 5")],
				quality: None,
				note: Some("needs 4.3 GB (110 %), 3.9 GB free · retries when space frees"),
			},
		);
		issues.push(Issue {
			file: waiting,
			state: FileState::Waiting,
			name: "GOPR0107.MP4".to_owned(),
			folder: self.library.relative_path(holiday),
			reason: "Not enough space: needs 4.3 GB (110 %), 3.9 GB free".to_owned(),
			resolution: "Retries automatically when space frees".to_owned(),
			log: lines(&[
				"disk check E: free 3.9 GB",
				"need 110 % of 3.9 GB source = 4.3 GB",
				"queued behind space condition",
			]),
		});

		self.generate(
			holiday,
			&Generated {
				count: 5,
				done: 1.0,
				kind: MediaKind::Video,
				name: |i| format!("GOPR{:04}.MP4", 43 + i),
			},
		);
	}

	fn concerts(&mut self, concerts: NodeId, issues: &mut Vec<Issue>) {
		let name = "concert-MPEG2_1080-interlaced_sample.mkv";
		let searching = self.active(
			concerts,
			name,
			MediaKind::Video,
			135_000_000,
			Phase::Search { probe: 3 },
		);
		self.showcase(
			searching,
			Showcase {
				source_info: "MPEG-2 1920×1080i TFF 29.97 · AC3 5.1(side)",
				plan: &[
					("Plan", "bwdif → 59.94p · AV1 10-bit · SVT preset 5"),
					("Audio", "AC3 5.1(side) → Opus 5.1 256k (relabelled)"),
					("Group", "1080p · 60 · mpeg2 · interlaced · light (new group)"),
				],
				quality: None,
				note: None,
			},
		);

		let failed = self.file(
			concerts,
			"concert_1998.rm",
			MediaKind::Video,
			FileState::Failed,
			412_000_000,
		);
		self.showcase(
			failed,
			Showcase {
				source_info: "RealVideo 4 352×288 25 fps · Cook audio",
				plan: &[("Plan", "AV1 10-bit · SVT preset 5")],
				quality: None,
				note: Some("encode failed the same way 3 times"),
			},
		);
		issues.push(Issue {
			file: failed,
			state: FileState::Failed,
			name: "concert_1998.rm".to_owned(),
			folder: self.library.relative_path(concerts),
			reason: "Encode failed the same way 3 times".to_owned(),
			resolution: "Retry after an ffmpeg update, or copy the original".to_owned(),
			log: lines(&[
				"[rv40] Invalid data found when processing input",
				"Error while decoding stream #0:0",
				"Conversion failed!",
				"exit code 1 (attempt 3/3)",
			]),
		});

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
		let kept = self.file(
			compressed,
			"action-clip.webm",
			MediaKind::Video,
			FileState::KeptOriginal,
			502_000_000,
		);
		self.showcase(
			kept,
			Showcase {
				source_info: "AV1 1920×960 24 fps · 4.4 Mbps",
				plan: &[],
				quality: None,
				note: Some("already efficient: predicted saving 3 %"),
			},
		);

		for i in 0..8 {
			let name = format!("trailer_{:02}.webm", 1 + i);
			let bytes_in = MediaKind::Video.input_size(&mut self.rng);
			self.file(compressed, &name, MediaKind::Video, FileState::KeptOriginal, bytes_in);
		}
	}

	fn concealed_errors(&mut self, videos: NodeId, issues: &mut Vec<Issue>) {
		let warned = self.file(
			videos,
			"garbage_64k_in_middle.mp4",
			MediaKind::Video,
			FileState::Warning,
			18_400_000,
		);
		if let Some(status) = self.library.file_mut(warned) {
			status.bytes_out = Some(6_100_000);
		}
		self.showcase(
			warned,
			Showcase {
				source_info: "H.264 1280×720 29.97 · 64 KiB of garbage in the middle",
				plan: &[("Plan", "AV1 10-bit · SVT preset 5 · CRF 32.00")],
				quality: Some("VMAF 94.8 · p5 88.0"),
				note: Some("212 decoder errors concealed · glitches carried over"),
			},
		);
		issues.push(Issue {
			file: warned,
			state: FileState::Warning,
			name: "garbage_64k_in_middle.mp4".to_owned(),
			folder: self.library.relative_path(videos),
			reason: "Encoded with a warning: 212 decoder errors concealed".to_owned(),
			resolution: "Output kept · glitches carried over".to_owned(),
			log: lines(&[
				"[h264] error while decoding MB 44 20",
				"[h264] concealing 1620 DC, 1620 AC, 1620 MV errors",
				"... 212 decoder errors total",
				"duration check ok: 16.83 s of 16.83 s",
			]),
		});
	}

	fn music(&mut self, music: NodeId) {
		let ost = self.folder(music, "Anime OST");
		let live = self.folder(music, "Live");

		let first = self.file(ost, "01.テスト.mp3", MediaKind::Audio, FileState::Done, 1_470_000);
		self.showcase(
			first,
			Showcase {
				source_info: "MP3 342 kbps · cover 492×704",
				plan: &[("Plan", "Opus 128k · cover + ID3 tags kept")],
				quality: None,
				note: None,
			},
		);

		let second = self.file(ost, "02. 試験曲.mp3", MediaKind::Audio, FileState::Done, 3_670_000);
		self.showcase(
			second,
			Showcase {
				source_info: "MP3 384 kbps · cover 849×1200",
				plan: &[("Plan", "Opus 128k · cover + ID3 tags kept")],
				quality: None,
				note: None,
			},
		);

		let duplicate = self.file(
			ost,
			"battle-music.mp3",
			MediaKind::Audio,
			FileState::Duplicate,
			1_150_000,
		);
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

		let canonical = self.file(
			music,
			"battle-music.mp3",
			MediaKind::Audio,
			FileState::Done,
			1_150_000,
		);
		self.duplicate_of.insert(duplicate, canonical);
	}

	fn screenshots(&mut self, screenshots: NodeId) -> NodeId {
		let game = self.folder(screenshots, "Cyberpunk 2077");
		let desktop = self.folder(screenshots, "Desktop");

		let done = self.file(
			game,
			"game-screenshot.png",
			MediaKind::Screenshot,
			FileState::Done,
			4_500_000,
		);
		self.showcase(
			done,
			Showcase {
				source_info: "PNG 1883×1028 RGBA (alpha all 255)",
				plan: &[("Plan", "opaque AVIF · alpha dropped")],
				quality: Some("SSIMULACRA2 88.0"),
				note: None,
			},
		);

		let name = "photomode_01012020_120000.png";
		let verifying = self.active(
			game,
			name,
			MediaKind::Screenshot,
			4_500_000,
			Phase::Verify { fraction: 0.9 },
		);
		self.showcase(
			verifying,
			Showcase {
				source_info: "PNG 2560×1440 RGBA",
				plan: &[("Plan", "AVIF 10-bit with alpha")],
				quality: None,
				note: None,
			},
		);

		self.generate(
			game,
			&Generated {
				count: 38,
				done: 0.6,
				kind: MediaKind::Screenshot,
				name: |i| {
					format!(
						"photomode_{:02}122020_{:06}.png",
						1 + i % 28,
						100_000 + i * 7919 % 800_000
					)
				},
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
		desktop
	}

	fn documents(&mut self, documents: NodeId) {
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

fn lines(text: &[&str]) -> Vec<String> {
	text.iter().map(|&line| line.to_owned()).collect()
}
