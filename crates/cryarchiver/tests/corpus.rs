use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::ogg::{OggPictureStorage, OpusFile};

fn corpus() -> PathBuf {
	std::env::var_os("CRY_CORPUS").map_or_else(|| PathBuf::from(r"C:\media-corpus"), PathBuf::from)
}

fn copy_into(dir: &Path, relative: &str) {
	let source = corpus().join(relative);
	fs::copy(&source, dir.join(source.file_name().unwrap())).unwrap();
}

#[test]
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus)"]
fn corpus_audio_outcomes() {
	let dir = tempfile::tempdir().unwrap();
	for file in [
		"01.テスト.mp3",
		"02. 試験曲.mp3",
		"battle-music.mp3",
		"generated-valid/audio_opus_mono_32k.opus",
		"generated-valid/audio_10ch_reads-as-5.1.4.wav",
		"generated-valid/audio_hexagonal_flac.mka",
		"generated-valid/audio_24bit_96k.flac",
		"generated-valid/audio_alac_cover.m4a",
		"generated-corrupted/garbage_prefix_4k.mp3",
		"generated-corrupted/text_file_named.mp4",
	] {
		copy_into(dir.path(), file);
	}

	let out = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.current_dir(dir.path())
		.arg(".")
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&out.stdout);

	for encoded in [
		"01.テスト",
		"02. 試験曲",
		"battle-music",
		"audio_24bit_96k",
		"audio_hexagonal_flac",
		"audio_alac_cover",
		"garbage_prefix_4k",
	] {
		assert!(
			dir.path().join(format!("{encoded}.cry.opus")).is_file(),
			"{encoded} missing\n{stdout}"
		);
	}
	assert!(stdout.contains("= audio_opus_mono_32k.opus  kept"), "{stdout}");
	assert!(stdout.contains("= audio_10ch_reads-as-5.1.4.wav  kept"), "{stdout}");
	assert!(stdout.contains("· text_file_named.mp4  skipped"), "{stdout}");

	for with_cover in ["01.テスト.cry.opus", "audio_alac_cover.cry.opus"] {
		let file = OpusFile::read_from(
			&mut File::open(dir.path().join(with_cover)).unwrap(),
			ParseOptions::new(),
		)
		.unwrap();
		assert_eq!(
			file.vorbis_comments().pictures().len(),
			1,
			"{with_cover} lost its cover"
		);
	}
}

fn ffprobe_json(path: &Path, extra: &[&str]) -> serde_json::Value {
	let out = Command::new("ffprobe")
		.args([
			"-v",
			"error",
			"-print_format",
			"json",
			"-show_streams",
			"-show_chapters",
		])
		.args(extra)
		.arg(path)
		.output()
		.unwrap();
	serde_json::from_slice(&out.stdout).unwrap()
}

fn snapshot(files: &[PathBuf]) -> Vec<(u64, std::time::SystemTime)> {
	files
		.iter()
		.map(|file| {
			let meta = fs::metadata(file).unwrap();
			(meta.len(), meta.modified().unwrap())
		})
		.collect()
}

fn assert_video_outputs(out_dir: &Path, stdout: &str) {
	let output = |stem: &str| out_dir.join(format!("{stem}.cry.mkv"));

	let rotated = ffprobe_json(&output("rotation_90"), &[]);
	assert_eq!(
		(
			rotated["streams"][0]["width"].as_u64(),
			rotated["streams"][0]["height"].as_u64()
		),
		(Some(360), Some(640))
	);
	assert!(rotated["streams"][0]["side_data_list"].is_null(), "{rotated}");

	let hdr = ffprobe_json(&output("hdr10_pq_hevc10"), &[]);
	assert_eq!(hdr["streams"][0]["color_transfer"], "smpte2084");
	assert!(
		hdr["streams"][0]["side_data_list"]
			.to_string()
			.contains("Mastering display metadata"),
		"{hdr}"
	);
	assert_eq!(
		ffprobe_json(&output("hlg_hevc10"), &[])["streams"][0]["color_transfer"],
		"arib-std-b67"
	);
	assert_eq!(
		ffprobe_json(&output("bt2020-10_sdr_hevc10"), &[])["streams"][0]["color_transfer"],
		"bt2020-10"
	);
	assert_eq!(
		ffprobe_json(&output("fullrange_pc_h264"), &[])["streams"][0]["color_range"],
		"pc"
	);

	let languages = ffprobe_json(&output("multi-audio_3tracks_langs"), &[]);
	let audio: Vec<&serde_json::Value> = languages["streams"]
		.as_array()
		.unwrap()
		.iter()
		.filter(|stream| stream["codec_type"] == "audio")
		.collect();
	assert_eq!(audio.len(), 3);
	assert_eq!(audio[1]["tags"]["language"], "jpn");

	let subtitles = ffprobe_json(&output("subs_mov_text_chapters"), &[]);
	assert!(subtitles["streams"].to_string().contains("\"subrip\""), "{subtitles}");
	assert_eq!(subtitles["chapters"].as_array().unwrap().len(), 3);
	assert!(!subtitles["streams"].to_string().contains("bin_data"), "{subtitles}");

	let fonts = ffprobe_json(&output("subs_srt_ass_font"), &[]);
	assert!(fonts["streams"].to_string().contains("arial.ttf"), "{fonts}");

	let cover = ffprobe_json(&output("mp4_with_cover_thumbnail"), &[]);
	assert!(cover["streams"].to_string().contains("cover.jpg"), "{cover}");

	for alpha in ["prores4444_alpha", "transparent_video"] {
		let info = ffprobe_json(&output(alpha), &[]);
		assert_eq!(info["streams"][0]["codec_name"], "vp9", "{alpha}");
		assert!(
			info["streams"][0]["tags"]
				.to_string()
				.to_lowercase()
				.contains("\"alpha_mode\":\"1\""),
			"{info}"
		);
	}

	let deinterlaced = ffprobe_json(&output("concert-MPEG2_1080-interlaced_sample"), &[]);
	assert_eq!(
		deinterlaced["streams"][0]["r_frame_rate"], "60000/1001",
		"{deinterlaced}"
	);
	assert_eq!(deinterlaced["streams"][1]["channel_layout"], "5.1", "{deinterlaced}");

	let iphone = ffprobe_json(&output("IMG_0001 2"), &[]);
	assert_eq!(
		(
			iphone["streams"][0]["width"].as_u64(),
			iphone["streams"][0]["height"].as_u64()
		),
		(Some(1080), Some(1920))
	);
	assert_eq!(iphone["streams"][0]["color_transfer"], "arib-std-b67");
	assert!(!iphone.to_string().contains("DOVI"), "{iphone}");
	assert!(stdout.contains("Dolby Vision metadata dropped"), "{stdout}");
}

#[test]
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus); takes several minutes"]
fn corpus_video_outcomes() {
	let out_dir = tempfile::tempdir().unwrap();
	let corpus = corpus();
	let relative = [
		"generated-valid/interlaced_tff_480i_h264.mkv",
		"generated-valid/telecined_480i_mpeg2.mpg",
		"generated-valid/hdr10_pq_hevc10.mkv",
		"generated-valid/hlg_hevc10.mkv",
		"generated-valid/bt2020-10_sdr_hevc10.mkv",
		"generated-valid/fullrange_pc_h264.mp4",
		"generated-valid/rotation_90.mp4",
		"generated-valid/multi-audio_3tracks_langs.mkv",
		"generated-valid/subs_mov_text_chapters.mp4",
		"generated-valid/subs_srt_ass_font.mkv",
		"generated-valid/mp4_with_cover_thumbnail.mp4",
		"generated-valid/mov_timecode_tmcd.mov",
		"generated-valid/prores4444_alpha.mov",
		"generated-valid/video_no_audio.mp4",
		"generated-valid/vfr_alternating.mkv",
		"generated-valid/actually_video.mp3",
		"generated-corrupted/garbage_64k_in_middle.mp4",
		"generated-corrupted/truncated_50pct_faststart.mp4",
		"generated-corrupted/truncated_60pct.mkv",
		"generated-corrupted/valid-header_random-clusters.mkv",
		"generated-corrupted/truncated_50pct_alpha.webm",
		"transparent_video.webm",
		"IMG_0001 2.MOV",
		"concert-MPEG2_1080-interlaced_sample.mkv",
	];
	let sources: Vec<PathBuf> = relative.iter().map(|file| corpus.join(file)).collect();
	let before = snapshot(&sources);

	let out = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.args(["-s", "fast", "--min-savings", "0", "-o"])
		.arg(out_dir.path())
		.args(&sources)
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&out.stdout);

	assert_eq!(snapshot(&sources), before, "the corpus changed");
	assert!(!out_dir.path().join(".cry-work").exists());

	for damaged in [
		"truncated_50pct_faststart.mp4",
		"truncated_60pct.mkv",
		"valid-header_random-clusters.mkv",
		"truncated_50pct_alpha.webm",
	] {
		assert!(
			stdout.contains(&format!("✗ {damaged}  failed: source damaged")),
			"{damaged}\n{stdout}"
		);
	}

	for valid in &relative[..16] {
		let name = Path::new(valid).file_name().unwrap().to_string_lossy();
		assert!(!stdout.contains(&format!("✗ {name}")), "{name} failed\n{stdout}");
	}

	assert!(stdout.contains("✓ garbage_64k_in_middle.mp4"), "{stdout}");
	assert!(stdout.contains("decoding errors"), "{stdout}");

	assert_video_outputs(out_dir.path(), &stdout);
}
