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

	let cache = tempfile::tempdir().unwrap();
	let out = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.env("CRYARCHIVER_CACHE_DIR", cache.path())
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

// A synthetic source can be smaller than any output. The original is then kept, and since only a smaller output
// is verified and written, there is nothing to inspect.
fn encoded_or_kept(out_dir: &Path, stdout: &str, stem: &str, extension: &str) -> Option<serde_json::Value> {
	let output = out_dir.join(format!("{stem}.cry.mkv"));
	if output.exists() {
		return Some(ffprobe_json(&output, &[]));
	}

	assert!(
		stdout.contains(&format!("= {stem}.{extension}  kept: encoding saved too little")),
		"{stem}
{stdout}"
	);
	None
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

	if let Some(hdr) = encoded_or_kept(out_dir, stdout, "hdr10_pq_hevc10", "mkv") {
		assert_eq!(hdr["streams"][0]["color_transfer"], "smpte2084");
		assert!(
			hdr["streams"][0]["side_data_list"]
				.to_string()
				.contains("Mastering display metadata"),
			"{hdr}"
		);
	}

	if let Some(hlg) = encoded_or_kept(out_dir, stdout, "hlg_hevc10", "mkv") {
		assert_eq!(hlg["streams"][0]["color_transfer"], "arib-std-b67");
	}

	if let Some(wide) = encoded_or_kept(out_dir, stdout, "bt2020-10_sdr_hevc10", "mkv") {
		assert_eq!(wide["streams"][0]["color_transfer"], "bt2020-10");
	}

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

	let alpha = ffprobe_json(&output("prores4444_alpha"), &[]);
	assert_eq!(alpha["streams"][0]["codec_name"], "vp9", "{alpha}");
	assert!(
		alpha["streams"][0]["tags"]
			.to_string()
			.to_lowercase()
			.contains("\"alpha_mode\":\"1\""),
		"{alpha}"
	);
	assert!(
		stdout.contains("= transparent_video.webm  kept: already VP9"),
		"{stdout}"
	);

	if let Some(deinterlaced) = encoded_or_kept(out_dir, stdout, "concert-MPEG2_1080-interlaced_sample", "mkv") {
		let rate = deinterlaced["streams"][0]["r_frame_rate"].as_str().unwrap();
		let (numerator, denominator) = rate.split_once('/').unwrap();
		let fps = numerator.parse::<f64>().unwrap() / denominator.parse::<f64>().unwrap();
		assert!((fps - 60_000.0 / 1001.0).abs() < 0.01, "{deinterlaced}");
		assert_eq!(deinterlaced["streams"][1]["channel_layout"], "5.1", "{deinterlaced}");
	}

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

	let cache = tempfile::tempdir().unwrap();
	let out = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.env("CRYARCHIVER_CACHE_DIR", cache.path())
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

	assert!(!stdout.contains("✗ garbage_64k_in_middle.mp4"), "{stdout}");
	if stdout.contains("✓ garbage_64k_in_middle.mp4") {
		assert!(stdout.contains("decoding errors"), "{stdout}");
	}

	assert_video_outputs(out_dir.path(), &stdout);
}

#[test]
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus); takes about 30 minutes"]
fn corpus_quality_search_outcomes() {
	let out_dir = tempfile::tempdir().unwrap();
	let cache = tempfile::tempdir().unwrap();
	let corpus = corpus();
	let sources: Vec<PathBuf> = ["short-film.mp4", "already-compressed/action-clip.webm"]
		.iter()
		.map(|file| corpus.join(file))
		.collect();
	let before = snapshot(&sources);

	let out = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.env("CRYARCHIVER_CACHE_DIR", cache.path())
		.arg("-o")
		.arg(out_dir.path())
		.args(&sources)
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&out.stdout);

	assert_eq!(snapshot(&sources), before, "the corpus changed");
	assert!(!out_dir.path().join(".cry-work").exists());
	assert!(stdout.contains("✓ short-film.mp4 → short-film.cry.mkv"), "{stdout}");
	assert!(
		stdout.contains("✓ action-clip.webm → action-clip.cry.mkv"),
		"{stdout}"
	);
	assert!(out_dir.path().join("action-clip.cry.mkv").exists());

	let cached = fs::read_to_string(cache.path().join("quality-cache.json")).unwrap();
	assert!(cached.contains("video/h264/720/fps31/sdr8/progressive/"), "{cached}");
}

const IMAGE_FILES: &[&str] = &[
	"DSC_0001.JPG",
	"mountains.jpg",
	"comic-veryTallImage.jpg",
	"company-logo.png",
	"photo.png",
	"screenshot_with_ICC.avif",
	"generated-valid/jpeg_exif_orientation6_gps.jpg",
	"generated-valid/jpeg_icc_adobergb.jpg",
	"generated-valid/jpeg_iptc_xmp.jpg",
	"generated-valid/png16_real_gradient.png",
	"generated-valid/png16_padded_8bit-data.png",
	"generated-valid/tiff_16bit_rgb.tif",
	"generated-valid/huge_20000x800.png",
	"generated-valid/png_alpha_real.png",
	"generated-valid/webp_lossless.webp",
	"generated-valid/jpeg2000.jp2",
	"generated-valid/jxl_lossless_existing.jxl",
	"generated-valid/gif_loop3_variable-delays.gif",
	"generated-valid/gif_transparent_anim.gif",
	"generated-valid/apng_alpha_anim.apng",
	"generated-valid/webp_anim_alpha.webp",
	"generated-valid/tiny_1x1.png",
	"generated-corrupted/truncated_half.jpg",
	"generated-corrupted/truncated_70pct.png",
	"generated-corrupted/truncated_50pct_anim.gif",
	"generated-corrupted/truncated_60pct.avif",
	"generated-corrupted/icc_broken_profile-size.jpg",
	"generated-corrupted/png_bomb_header-100000x100000.png",
	"generated-corrupted/bad_crc_only_data-intact.png",
	"generated-corrupted/exif_broken_ifd0-offset.jpg",
	"generated-corrupted/bitflips_entropy-data.jpg",
	"generated-corrupted/missing_soi-marker.jpg",
	"generated-corrupted/html_503_page_named.gif",
	"generated-corrupted/zero_bytes.jpg",
];

fn video_stream_count(path: &Path) -> usize {
	ffprobe_json(path, &[])["streams"]
		.as_array()
		.unwrap()
		.iter()
		.filter(|stream| stream["codec_type"] == "video")
		.count()
}

#[test]
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus); takes about a minute"]
fn corpus_image_outcomes() {
	let out_dir = tempfile::tempdir().unwrap();
	let corpus = corpus();
	let sources: Vec<PathBuf> = IMAGE_FILES.iter().map(|file| corpus.join(file)).collect();
	let before = snapshot(&sources);

	let cache = tempfile::tempdir().unwrap();
	let out = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.env("CRYARCHIVER_CACHE_DIR", cache.path())
		.args(["-s", "fast", "--min-savings", "0", "-o"])
		.arg(out_dir.path())
		.args(&sources)
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&out.stdout);
	let output = |name: &str| out_dir.path().join(name);

	assert_eq!(snapshot(&sources), before, "the corpus changed");
	assert!(!out_dir.path().join(".cry-work").exists());

	for damaged in [
		"truncated_half.jpg",
		"truncated_70pct.png",
		"truncated_50pct_anim.gif",
		"truncated_60pct.avif",
	] {
		assert!(
			stdout.contains(&format!("✗ {damaged}  failed: source damaged")),
			"{damaged}\n{stdout}"
		);
	}
	for other in ["missing_soi-marker.jpg", "html_503_page_named.gif", "zero_bytes.jpg"] {
		assert!(stdout.contains(&format!("· {other}  skipped")), "{other}\n{stdout}");
	}
	assert!(
		stdout.contains("= icc_broken_profile-size.jpg  kept: invalid ICC profile"),
		"{stdout}"
	);
	assert!(
		stdout.contains("= png_bomb_header-100000x100000.png  kept: the header claims 10000 MP"),
		"{stdout}"
	);
	assert!(stdout.contains("CRC mismatch in the IDAT chunk"), "{stdout}");
	assert!(stdout.contains("decoding errors were concealed"), "{stdout}");

	for encoded in [
		"DSC_0001.cry.avif",
		"png16_padded_8bit-data.cry.avif",
		"jpeg_exif_orientation6_gps.cry.avif",
	] {
		assert!(output(encoded).is_file(), "{encoded}\n{stdout}");
	}
	for deep_or_large in [
		"comic-veryTallImage.cry.jxl",
		"png16_real_gradient.cry.jxl",
		"huge_20000x800.cry.jxl",
	] {
		assert!(output(deep_or_large).is_file(), "{deep_or_large}\n{stdout}");
	}

	let upright = ffprobe_json(&output("jpeg_exif_orientation6_gps.cry.avif"), &[]);
	assert_eq!(
		(
			upright["streams"][0]["width"].as_u64(),
			upright["streams"][0]["height"].as_u64()
		),
		(Some(800), Some(1200))
	);

	let looping = fs::read(output("gif_loop3_variable-delays.cry.avif")).unwrap();
	assert_eq!(cryarchiver_engine::images::animated::plays(&looping), Some(Some(4)));
	for transparent in [
		"gif_transparent_anim.cry.avif",
		"apng_alpha_anim.cry.avif",
		"webp_anim_alpha.cry.avif",
	] {
		assert_eq!(video_stream_count(&output(transparent)), 4, "{transparent}");
	}
}
