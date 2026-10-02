mod support;

use std::path::{Path, PathBuf};

use cryarchiver_engine::options::TrimWindow;
use cryarchiver_engine::probe::probe;
use cryarchiver_engine::video::analyze::{Analysis, DEFAULT_SEGMENTS, NoiseClass, ScanType, analyze};
use cryarchiver_engine::video::plan::grain_level;
use tokio_util::sync::CancellationToken;

fn encode(dir: &Path, name: &str, source: &str, args: &[&str]) -> PathBuf {
	let path = dir.join(name);
	let mut all = vec!["-f", "lavfi", "-i", source];
	all.extend_from_slice(args);
	all.push(path.to_str().unwrap());
	support::ffmpeg(&all);
	path
}

async fn scan_and_noise(path: &Path) -> (ScanType, NoiseClass) {
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, path, &cancel).await.unwrap();
	let main = info.streams.iter().find(|stream| stream.is_real_video()).unwrap();

	let analysis = Box::pin(analyze(
		&tools,
		path,
		main,
		info.format.duration,
		DEFAULT_SEGMENTS,
		None,
		&cancel,
	))
	.await
	.unwrap();
	(analysis.scan, analysis.noise)
}

const INTERLACED_MPEG2: &[&str] = &["-c:v", "mpeg2video", "-b:v", "12M", "-flags", "+ilme+ildct"];

#[tokio::test]
async fn detects_interlaced_tff_and_bff() {
	let dir = tempfile::tempdir().unwrap();
	let tff = encode(
		dir.path(),
		"tff.mpg",
		"testsrc2=s=720x576:r=50:d=4,interlace=scan=tff,setfield=tff",
		INTERLACED_MPEG2,
	);
	let bff = encode(
		dir.path(),
		"bff.mpg",
		"testsrc2=s=720x576:r=50:d=4,interlace=scan=bff,setfield=bff",
		INTERLACED_MPEG2,
	);

	assert_eq!(scan_and_noise(&tff).await.0, ScanType::Interlaced { top_first: true });
	assert_eq!(scan_and_noise(&bff).await.0, ScanType::Interlaced { top_first: false });
}

#[tokio::test]
async fn faintly_interlaced_still_home_video_is_detected_with_its_field_order() {
	let dir = tempfile::tempdir().unwrap();

	for (scan, top_first) in [("tff", true), ("bff", false)] {
		for container in ["mpg", "mkv"] {
			let clip = support::still_home_video(dir.path(), &format!("{scan}.{container}"), scan);

			assert_eq!(
				scan_and_noise(&clip).await.0,
				ScanType::Interlaced { top_first },
				"{scan}.{container}"
			);
		}
	}
}

#[tokio::test]
async fn faintly_interlaced_video_without_a_field_flag_is_detected_from_its_combing() {
	let dir = tempfile::tempdir().unwrap();

	for (scan, top_first) in [("tff", true), ("bff", false)] {
		let clip = support::unflagged_home_video(dir.path(), &format!("{scan}.mkv"), scan);

		assert_eq!(
			scan_and_noise(&clip).await.0,
			ScanType::Interlaced { top_first },
			"{scan}"
		);
	}
}

#[tokio::test]
async fn progressive_video_in_an_interlaced_container_stays_progressive() {
	let dir = tempfile::tempdir().unwrap();

	for container in ["mpg", "mkv"] {
		let name = format!("still.{container}");
		let still = support::still_home_video(dir.path(), &name, "progressive");

		assert_eq!(scan_and_noise(&still).await.0, ScanType::Progressive, "{name}");

		for (tilt, rows) in [("down", "n*3"), ("up", "1400-n*3")] {
			let name = format!("tilt-{tilt}.{container}");
			let source = format!("testsrc2=s=720x2000:r=25:d=4,crop=720:576:0:'{rows}',setfield=tff");
			let panned = encode(dir.path(), &name, &source, INTERLACED_MPEG2);

			assert_eq!(scan_and_noise(&panned).await.0, ScanType::Progressive, "{name}");
		}
	}
}

#[tokio::test]
async fn detects_telecine_and_progressive() {
	let dir = tempfile::tempdir().unwrap();
	let telecined = encode(
		dir.path(),
		"telecined.mpg",
		"testsrc2=s=720x480:r=24000/1001:d=6,telecine=first_field=top:pattern=23,setfield=tff",
		&["-c:v", "mpeg2video", "-b:v", "12M", "-flags", "+ilme+ildct"],
	);
	let progressive = encode(
		dir.path(),
		"progressive.mp4",
		"testsrc2=s=640x360:r=30:d=14",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);

	assert_eq!(scan_and_noise(&telecined).await.0, ScanType::Telecined);
	assert_eq!(
		scan_and_noise(&progressive).await,
		(ScanType::Progressive, NoiseClass::Clean)
	);
}

#[tokio::test]
async fn interlaced_h264_high_frame_rates_and_very_short_clips_are_detected() {
	let dir = tempfile::tempdir().unwrap();
	let h264 = encode(
		dir.path(),
		"tff_480i.mkv",
		"testsrc2=s=720x480:r=60000/1001:d=4,interlace=scan=tff,setfield=tff",
		&[
			"-c:v",
			"libx264",
			"-crf",
			"18",
			"-flags",
			"+ilme+ildct",
			"-x264opts",
			"tff=1",
		],
	);
	let high_rate = encode(
		dir.path(),
		"hfr.mp4",
		"testsrc2=s=320x240:r=120:d=3",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);
	let very_short = encode(
		dir.path(),
		"short.mp4",
		"testsrc2=s=320x240:r=25:d=0.4",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);

	assert_eq!(scan_and_noise(&h264).await.0, ScanType::Interlaced { top_first: true });
	assert_eq!(scan_and_noise(&high_rate).await.0, ScanType::Progressive);
	assert_eq!(scan_and_noise(&very_short).await.0, ScanType::Progressive);
}

async fn measured(path: &Path, window: Option<TrimWindow>) -> Analysis {
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, path, &cancel).await.unwrap();
	let main = info.streams.iter().find(|stream| stream.is_real_video()).unwrap();

	Box::pin(analyze(
		&tools,
		path,
		main,
		info.format.duration,
		DEFAULT_SEGMENTS,
		window,
		&cancel,
	))
	.await
	.unwrap()
}

const CARTOON: &str = "testsrc2=s=640x360:r=24:d=4,\
	drawbox=x=40:y=190:w=220:h=130:color=0xE0C080:t=fill,drawbox=x=40:y=190:w=220:h=130:color=black:t=4,\
	drawbox=x=380:y=30:w=200:h=150:color=0x3070C0:t=fill,drawbox=x=380:y=30:w=200:h=150:color=black:t=3";

#[tokio::test]
async fn a_compressed_clean_cartoon_gets_no_grain() {
	let dir = tempfile::tempdir().unwrap();
	let cartoon = encode(
		dir.path(),
		"cartoon.mkv",
		CARTOON,
		&["-pix_fmt", "yuv420p", "-c:v", "libx264", "-crf", "30"],
	);
	let bars = encode(
		dir.path(),
		"bars.mkv",
		"smptehdbars=s=640x360:r=24:d=4,drawbox=x=100:y=60:w=160:h=90:color=white:t=6",
		&["-pix_fmt", "yuv420p", "-c:v", "libx264", "-crf", "30"],
	);

	for clip in [cartoon, bars] {
		let analysis = measured(&clip, None).await;

		assert_eq!(analysis.noise, NoiseClass::Clean, "{analysis:?}");
		assert_eq!(grain_level(analysis.noise_sigma), None, "{analysis:?}");
	}
}

#[tokio::test]
async fn temporal_noise_gets_grain_that_rises_with_it() {
	let dir = tempfile::tempdir().unwrap();
	let noisy = |name: &str, strength: u32| {
		encode(
			dir.path(),
			name,
			&format!("{CARTOON},noise=alls={strength}:allf=t"),
			&["-pix_fmt", "yuv420p", "-c:v", "libx264", "-crf", "8"],
		)
	};
	let (light, heavy) = (noisy("light.mkv", 3), noisy("heavy.mkv", 8));

	let (light, heavy) = (measured(&light, None).await, measured(&heavy, None).await);

	assert_eq!(light.noise, NoiseClass::Light, "{light:?}");
	assert_eq!(heavy.noise, NoiseClass::Light, "{heavy:?}");
	assert!(heavy.noise_sigma > light.noise_sigma, "{light:?} {heavy:?}");
	assert!(
		grain_level(heavy.noise_sigma) > grain_level(light.noise_sigma),
		"{light:?} {heavy:?}"
	);
}

#[tokio::test]
async fn fine_static_texture_gets_no_grain() {
	let dir = tempfile::tempdir().unwrap();

	for amplitude in [5, 45] {
		let texture = encode(
			dir.path(),
			&format!("texture{amplitude}.mkv"),
			&format!(
				"color=c=gray:s=640x360:r=24:d=4,format=gray,\
				 geq=lum='128+{amplitude}*sin(X*X*0.37+Y*1.9)*cos(Y*Y*0.23+X*0.7)',format=yuv420p"
			),
			&["-c:v", "libx264", "-crf", "18"],
		);

		let analysis = measured(&texture, None).await;

		assert_eq!(analysis.noise, NoiseClass::Clean, "{amplitude}: {analysis:?}");
	}
}

#[tokio::test]
#[ignore = "calibration on a real clip: set CRY_GRAIN_CLIP, and optionally CRY_GRAIN_WINDOW (start:length in \
            seconds) and CRY_GRAIN_LEVEL (the expected level, 0 for none)"]
async fn a_real_clip_gets_its_expected_grain() {
	let Some(clip) = std::env::var_os("CRY_GRAIN_CLIP").map(PathBuf::from) else {
		return;
	};
	let window = std::env::var("CRY_GRAIN_WINDOW").ok().map(|window| {
		let (start, length) = window.split_once(':').unwrap();
		TrimWindow {
			start_s: start.parse().unwrap(),
			length_s: length.parse().unwrap(),
		}
	});

	let analysis = measured(&clip, window).await;
	eprintln!("noise {:?}, sigma {:?}", analysis.noise, analysis.noise_sigma);
	if let Some(expected) = std::env::var("CRY_GRAIN_LEVEL")
		.ok()
		.map(|level| level.parse::<u8>().unwrap())
	{
		assert_eq!(
			grain_level(analysis.noise_sigma).unwrap_or(0),
			expected,
			"{:?}",
			analysis.noise_sigma
		);
	}
}

#[tokio::test]
async fn detail_is_measured_for_every_segment() {
	let dir = tempfile::tempdir().unwrap();
	let clip = encode(
		dir.path(),
		"detailed.mp4",
		"testsrc2=s=640x360:r=30:d=14",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, &clip, &cancel).await.unwrap();
	let main = info.streams.iter().find(|stream| stream.is_real_video()).unwrap();

	let analysis = Box::pin(analyze(&tools, &clip, main, info.format.duration, 8, None, &cancel))
		.await
		.unwrap();

	assert_eq!(analysis.detail.len(), 8);
	assert!(
		analysis
			.detail
			.iter()
			.all(|detail| detail.usable && detail.si.is_some_and(|si| si > 15.0)),
		"{:?}",
		analysis.detail
	);
}
