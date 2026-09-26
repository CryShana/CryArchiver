mod support;

use std::fs;

use cryarchiver_engine::probe::probe;
use cryarchiver_engine::video::colour::{ColourTags, Range};
use cryarchiver_engine::video::encode::{check_video_temp, encode_video, extract, video_command};
use cryarchiver_engine::video::plan::{Encoder, VideoPlan};
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
			params: "tune=0:enable-variance-boost=1:keyint=5s:crf=40".into(),
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
		},
		alpha: false,
		hdr10: false,
		warnings: Vec::new(),
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
