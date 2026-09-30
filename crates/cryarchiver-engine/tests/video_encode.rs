mod support;

use std::collections::BTreeSet;
use std::fs;
use std::process::Command;

use cryarchiver_engine::options::SubtitlePick;
use cryarchiver_engine::preset::Speed;
use cryarchiver_engine::probe::{av1_film_grain, first_frames, probe};
use cryarchiver_engine::video::VideoStarts;
use cryarchiver_engine::video::analyze::{DEFAULT_SEGMENTS, NoiseClass, analyze};
use cryarchiver_engine::video::burn::{self, FONT_DIR, SUBTITLE_FILE};
use cryarchiver_engine::video::colour::{ColourTags, Range};
use cryarchiver_engine::video::encode::{check_video_temp, encode_video, extract, video_command};
use cryarchiver_engine::video::plan::{
	Encoder, VideoCaps, VideoPlan, grain_level, plan_video, precheck, synthesized_grain, usable_main,
};
use cryarchiver_engine::video::streams::Extracted;
use cryarchiver_engine::workspace::Workspace;
use tokio_util::sync::CancellationToken;

fn plan(expected_frames: Option<u64>) -> VideoPlan {
	VideoPlan {
		streams: vec![0],
		decoder: None,
		filters: "format=yuv420p10le,setparams=field_mode=prog:range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709"
			.into(),
		encoder: Encoder::SvtAv1 {
			preset: 12,
			crf: 40.0,
			film_grain: None,
		},
		expected_frames,
		frame_tolerance: 2,
		width: 320,
		height: 240,
		colour: ColourTags {
			primaries: "bt709".into(),
			transfer: "bt709".into(),
			matrix: "bt709".into(),
			range: Range::Limited,
			chroma_location: None,
		},
		alpha: false,
		hdr10: false,
		warnings: Vec::new(),
		window: None,
		starts: VideoStarts::default(),
		overlay: None,
		subtitles: None,
		threads: None,
		max_bitrate_kbps: None,
	}
}

#[tokio::test]
async fn encodes_the_video_temp_and_reports_frames_and_duration() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::video(dir.path(), "clip.mp4", 2, &["-map", "0:v", "-c:v", "mpeg4"]);
	let tools = support::toolchain().await;
	let output = dir.path().join("video.a1.mkv");
	let video = plan(Some(50));
	let mut fractions = Vec::new();

	let encoded = encode_video(
		&video_command(&tools.ffmpeg, &clip, &output, &video),
		&video,
		Some(2.0),
		|fraction| fractions.push(fraction),
		&CancellationToken::new(),
	)
	.await
	.unwrap();

	assert_eq!(encoded.frames, 50);
	assert!((encoded.decoded_s - 2.0).abs() < 0.1, "{encoded:?}");
	assert_eq!(encoded.decode_errors, 0);
	assert_eq!(check_video_temp(&video, &encoded, Some(2.0), 0.0).unwrap(), None);
	assert!(fractions.last().is_some_and(|last| *last > 0.9), "{fractions:?}");
	let info = probe(&tools, &output, &CancellationToken::new()).await.unwrap();
	assert_eq!(info.streams.len(), 1);
	assert_eq!(info.streams[0].codec_name, "av1");
}

#[tokio::test]
async fn a_noisy_clip_is_encoded_with_the_grain_level_its_noise_measures() {
	let dir = tempfile::tempdir().unwrap();
	let noisy = dir.path().join("noisy.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"color=c=gray:s=320x240:r=25:d=3,noise=alls=10:allf=t",
		"-pix_fmt",
		"yuv420p",
		"-c:v",
		"ffv1",
		noisy.to_str().unwrap(),
	]);
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, &noisy, &cancel).await.unwrap();
	let caps = VideoCaps {
		vp9_alpha: false,
		libaom: false,
	};
	let main = precheck(&info, &caps).unwrap();
	let analysis = Box::pin(analyze(
		&tools,
		&noisy,
		main,
		info.format.duration,
		DEFAULT_SEGMENTS,
		None,
		&cancel,
	))
	.await
	.unwrap();
	let mut video = plan_video(&info, main, &[], &analysis, Speed::Moderate);
	video.encoder = video.encoder.with_crf(45.0);
	let output = dir.path().join("video.a1.mkv");

	let cmd = video_command(&tools.ffmpeg, &noisy, &output, &video);
	let encoded = encode_video(&cmd, &video, info.format.duration, |_| {}, &cancel)
		.await
		.unwrap();

	let level = grain_level(analysis.noise_sigma);
	assert_eq!(analysis.noise, NoiseClass::Light, "{analysis:?}");
	assert!(level.is_some(), "{analysis:?}");
	assert_eq!(video.encoder.grain_level(), level);
	let grain = format!(":film-grain={}:film-grain-denoise=1", level.unwrap());
	assert!(
		cmd.args
			.iter()
			.any(|arg| arg.to_str().is_some_and(|arg| arg.ends_with(&grain))),
		"{:?}",
		cmd.args
	);
	assert_eq!(encoded.frames, 75);
	assert_eq!(encoded.decode_errors, 0);
}

#[tokio::test]
async fn an_av1_source_with_film_grain_gets_no_more_grain() {
	let dir = tempfile::tempdir().unwrap();
	let grained = dir.path().join("grained.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"color=c=gray:s=320x240:r=25:d=3,noise=alls=10:allf=t",
		"-c:v",
		"libsvtav1",
		"-preset",
		"12",
		"-svtav1-params",
		"crf=40:film-grain=10:film-grain-denoise=1",
		grained.to_str().unwrap(),
	]);
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, &grained, &cancel).await.unwrap();
	let caps = VideoCaps {
		vp9_alpha: false,
		libaom: false,
	};
	let main = usable_main(&info, &caps).unwrap();
	let frames = first_frames(&tools, &grained, main.index, 5, &cancel).await.unwrap();
	let analysis = Box::pin(analyze(
		&tools,
		&grained,
		main,
		info.format.duration,
		DEFAULT_SEGMENTS,
		None,
		&cancel,
	))
	.await
	.unwrap();

	let plain = dir.path().join("plain.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"color=c=gray:s=320x240:r=25:d=1",
		"-c:v",
		"libsvtav1",
		"-preset",
		"12",
		plain.to_str().unwrap(),
	]);

	let video = plan_video(&info, main, &frames, &analysis, Speed::Slow);

	assert!(synthesized_grain(&frames), "{frames:?}");
	assert_eq!(analysis.noise, NoiseClass::Light, "{analysis:?}");
	assert_eq!(video.encoder.grain_level(), None);
	assert!(av1_film_grain(&tools, &grained, main.index, &cancel).await.unwrap());
	assert!(!av1_film_grain(&tools, &plain, 0, &cancel).await.unwrap());
}

#[tokio::test]
async fn a_truncated_matroska_decodes_short() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::video(dir.path(), "cut.mkv", 6, &["-map", "0:v", "-c:v", "mpeg4"]);
	let bytes = fs::read(&clip).unwrap();
	fs::write(&clip, &bytes[..bytes.len() / 2]).unwrap();
	let tools = support::toolchain().await;
	let output = dir.path().join("video.a1.mkv");
	let video = plan(Some(150));

	let encoded = encode_video(
		&video_command(&tools.ffmpeg, &clip, &output, &video),
		&video,
		Some(6.0),
		|_| {},
		&CancellationToken::new(),
	)
	.await
	.unwrap();

	assert!(encoded.decoded_s < 4.0, "{encoded:?}");
	assert!(check_video_temp(&video, &encoded, Some(6.0), 0.0).is_err());
}

#[tokio::test]
async fn extracts_covers_and_data_streams_byte_exact() {
	let dir = tempfile::tempdir().unwrap();
	let cover = dir.path().join("cover.png");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"color=c=red:s=64x64",
		"-frames:v",
		"1",
		"-update",
		"1",
		cover.to_str().unwrap(),
	]);
	let mp4 = dir.path().join("with-cover.mp4");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=1",
		"-i",
		cover.to_str().unwrap(),
		"-map",
		"0:v",
		"-map",
		"1:v",
		"-c:v:0",
		"mpeg4",
		"-c:v:1",
		"png",
		"-disposition:v:1",
		"attached_pic",
		mp4.to_str().unwrap(),
	]);
	let workspace = Workspace::create(&dir.path().join(".cry-work"), &mp4).unwrap();
	let wanted = [Extracted {
		stream_index: 1,
		file_name: "cover.png".into(),
		mime_type: "image/png".into(),
	}];

	extract(
		&support::toolchain().await.ffmpeg,
		&mp4,
		&workspace,
		&wanted,
		&CancellationToken::new(),
	)
	.await
	.unwrap();

	let written = fs::read(workspace.file("stream-1.bin")).unwrap();
	assert_eq!(&written[..8], b"\x89PNG\r\n\x1a\n");
}

#[tokio::test]
async fn a_text_burn_makes_no_fonts_folder_and_an_ass_burn_takes_the_attached_fonts() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let text = support::clip_with_extras(dir.path());
	let ass = dir.path().join("styled.mkv");
	support::ffmpeg(&[
		"-i",
		text.to_str().unwrap(),
		"-map",
		"0",
		"-c",
		"copy",
		"-c:s",
		"ass",
		ass.to_str().unwrap(),
	]);

	let mut workspaces = Vec::new();
	for source in [&text, &ass] {
		let info = probe(&tools, source, &cancel).await.unwrap();
		let chosen = burn::pick(&info, &SubtitlePick::Default).unwrap();
		let workspace = Workspace::create(&dir.path().join(".cry-work"), source).unwrap();
		burn::prepare(&tools, source, &workspace, &info, chosen, &cancel)
			.await
			.unwrap();
		workspaces.push(workspace);
	}

	let [from_text, from_ass] = workspaces.as_slice() else {
		unreachable!()
	};
	assert!(
		fs::read_to_string(from_text.file(SUBTITLE_FILE))
			.unwrap()
			.contains("five")
	);
	assert!(!from_text.file(FONT_DIR).exists());
	assert!(
		fs::read_to_string(from_ass.file(SUBTITLE_FILE))
			.unwrap()
			.contains("five")
	);
	assert_eq!(
		fs::read(from_ass.file(FONT_DIR).join("attachment-0.ttf")).unwrap(),
		b"not a real font"
	);
}

#[tokio::test]
async fn a_ten_bit_bitmap_burn_stays_ten_bit_through_the_overlay() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let pgs = support::pgs_box(dir.path(), 1_000, 2_000);
	let deep = support::subtitled_clip(
		dir.path(),
		"deep.mkv",
		&support::Picture {
			source: "color=c=black",
			seconds: 3,
			codec: &["-c:v", "ffv1", "-pix_fmt", "yuv420p10le"],
			second_angle: false,
			marked_frame: None,
		},
		&pgs,
		"copy",
	);
	let info = probe(&tools, &deep, &cancel).await.unwrap();
	let chosen = burn::pick(&info, &SubtitlePick::Default).unwrap();
	let workspace = Workspace::create(&dir.path().join(".cry-work"), &deep).unwrap();
	let start_s = burn::prepare(&tools, &deep, &workspace, &info, chosen, &cancel)
		.await
		.unwrap()
		.unwrap();
	let burned = VideoPlan {
		filters: "showinfo,format=yuv420p10le".into(),
		overlay: Some(burn::overlay(&info, 0, chosen, start_s)),
		..plan(None)
	};

	let cmd = video_command(&tools.ffmpeg, &deep, &workspace.file("video.a1.mkv"), &burned);
	let out = Command::new(&cmd.program).args(&cmd.args).output().unwrap();

	let log = String::from_utf8_lossy(&out.stderr);
	assert!(out.status.success(), "{log}");
	let formats: BTreeSet<&str> = log
		.lines()
		.filter(|line| line.contains("Parsed_showinfo"))
		.filter_map(|line| line.split_whitespace().find_map(|field| field.strip_prefix("fmt:")))
		.collect();
	assert_eq!(formats, BTreeSet::from(["yuv420p10le"]), "{log}");
}
