use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::outcome::{ErrorKind, JobError};
use crate::probe::{self, Frame, MediaInfo, ProbeError, Stream};
use crate::process::{self, STALL_TIMEOUT, ToolCommand};
use crate::toolchain::Toolchain;
use crate::video::plan::{Encoder, VideoPlan};
use crate::video::run_ffmpeg;
use crate::video::streams::StreamPlan;

const MIN_ALPHA_SSIM: f64 = 0.98;
const MASTERING_DISPLAY: &str = "Mastering display metadata";
const DOVI_RECORD: &str = "DOVI configuration record";

pub struct Expected<'a> {
	pub video: &'a VideoPlan,
	pub streams: &'a StreamPlan,
	pub source_chapters: usize,
	pub attached_files: usize,
}

pub fn check_output(output: &MediaInfo, first_frame: Option<&Frame>, expected: &Expected<'_>) -> Result<(), String> {
	let count = |kind: &str| output.streams.iter().filter(|stream| stream.codec_type == kind).count();
	let videos: Vec<&Stream> = output.streams.iter().filter(|stream| stream.is_real_video()).collect();
	let attachments = count("attachment") + output.streams.iter().filter(|stream| stream.is_attached_pic()).count();

	expect_count("video streams", videos.len(), expected.video.streams.len())?;
	expect_count("audio streams", count("audio"), expected.streams.audio.len())?;
	expect_count("subtitle streams", count("subtitle"), expected.streams.subtitles.len())?;
	expect_count(
		"attachments",
		attachments,
		expected.streams.attachments.len() + expected.attached_files,
	)?;
	expect_count("chapters", output.chapters.len(), expected.source_chapters)?;

	let main = videos[0];
	let plan = expected.video;
	let codec = if plan.alpha { "vp9" } else { "av1" };
	if main.codec_name != codec {
		return Err(format!("the video is {}, not {codec}", main.codec_name));
	}

	let size = (main.width.unwrap_or(0), main.height.unwrap_or(0));
	if size != (plan.width, plan.height) {
		return Err(format!(
			"the video is {}×{}, not {}×{}",
			size.0, size.1, plan.width, plan.height
		));
	}

	check_colour(
		"container",
		[
			&main.color_range,
			&main.color_space,
			&main.color_transfer,
			&main.color_primaries,
		],
		plan,
	)?;
	if !matches!(plan.encoder, Encoder::Vp9Alpha { .. }) {
		let frame = first_frame.ok_or("no decodable video frame")?;
		check_colour(
			"bitstream",
			[
				&frame.color_range,
				&frame.color_space,
				&frame.color_transfer,
				&frame.color_primaries,
			],
			plan,
		)?;
	}

	if plan.alpha && main.tag("alpha_mode") != Some("1") {
		return Err("the alpha channel is not signalled".into());
	}

	if main.side_data(DOVI_RECORD).is_some() {
		return Err("a Dolby Vision configuration record is present".into());
	}

	let frame_has_mastering = first_frame.is_some_and(|frame| {
		frame
			.side_data_list
			.iter()
			.any(|data| data.side_data_type == MASTERING_DISPLAY)
	});
	if plan.hdr10 && (main.side_data(MASTERING_DISPLAY).is_none() || !frame_has_mastering) {
		return Err("HDR10 mastering-display metadata is missing".into());
	}
	Ok(())
}

pub async fn verify_output(
	tools: &Toolchain,
	output: &Path,
	expected: &Expected<'_>,
	cancel: &CancellationToken,
) -> Result<(), JobError> {
	let info = probe::probe(tools, output, cancel).await.map_err(unreadable)?;
	let frames = probe::first_frames(tools, output, 0, 1, cancel)
		.await
		.map_err(unreadable)?;

	check_output(&info, frames.first(), expected)
		.map_err(|reason| ErrorKind::EncodeFailed(format!("verification failed: {reason}")).into())
}

pub fn parse_ssim(stderr: &[String]) -> Option<f64> {
	stderr
		.iter()
		.rev()
		.filter(|line| line.contains("] SSIM "))
		.find_map(|line| line.split("All:").nth(1)?.split_whitespace().next()?.parse().ok())
}

pub async fn alpha_ssim(
	tools: &Toolchain,
	output: &Path,
	source: &Path,
	video: &VideoPlan,
	cancel: &CancellationToken,
) -> Result<f64, JobError> {
	let mut cmd = ToolCommand::new(&tools.ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-c:v", "libvpx-vp9", "-i"])
		.arg(process::tool_path(output));
	if let Some(decoder) = video.decoder {
		cmd.arg("-c:v").arg(decoder);
	}
	cmd.arg("-i")
		.arg(process::tool_path(source))
		.arg("-filter_complex")
		.arg(format!(
			"[0:v:0]alphaextract,format=gray[o];[1:{}]{},alphaextract,format=gray[s];[o][s]ssim",
			video.streams[0], video.filters
		));
	cmd.args(["-f", "null", "-"]);

	let out = run_ffmpeg(&cmd, |_| {}, cancel).await?;
	parse_ssim(&out.stderr_tail).ok_or_else(|| {
		JobError::with_log(
			ErrorKind::EncodeFailed("the alpha check printed no SSIM".into()),
			out.stderr_tail,
		)
	})
}

pub fn check_alpha(ssim: f64) -> Result<(), JobError> {
	if ssim < MIN_ALPHA_SSIM {
		return Err(ErrorKind::EncodeFailed(format!(
			"alpha verification failed: SSIM {ssim:.4} is below {MIN_ALPHA_SSIM}"
		))
		.into());
	}
	Ok(())
}

fn expect_count(what: &str, found: usize, expected: usize) -> Result<(), String> {
	if found == expected {
		Ok(())
	} else {
		Err(format!("{found} {what} in the output, {expected} planned"))
	}
}

fn check_colour(side: &str, fields: [&Option<String>; 4], plan: &VideoPlan) -> Result<(), String> {
	let tags = &plan.colour;
	let wanted = [
		tags.range.name(),
		tags.matrix.as_str(),
		tags.transfer.as_str(),
		tags.primaries.as_str(),
	];

	for (found, wanted) in fields.iter().zip(wanted) {
		if found.as_deref() != Some(wanted) {
			return Err(format!(
				"the {side} colour tags are {}, expected {}",
				fields.map(|field| field.as_deref().unwrap_or("unset")).join("/"),
				wanted_line(tags)
			));
		}
	}
	Ok(())
}

fn wanted_line(tags: &crate::video::colour::ColourTags) -> String {
	format!(
		"{}/{}/{}/{}",
		tags.range.name(),
		tags.matrix,
		tags.transfer,
		tags.primaries
	)
}

fn unreadable(err: ProbeError) -> JobError {
	match err {
		ProbeError::Run(err) => err.into(),
		ProbeError::Failed(log) => {
			JobError::with_log(ErrorKind::EncodeFailed("the output could not be read".into()), log)
		}
		ProbeError::Json(err) => ErrorKind::EncodeFailed(format!("the output could not be read: {err}")).into(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::audio::plan::{TrackJob, TrackPlan};
	use crate::video::colour::{ColourTags, Range};

	fn plan(alpha: bool, hdr10: bool) -> VideoPlan {
		VideoPlan {
			streams: vec![0],
			decoder: None,
			filters: String::new(),
			encoder: if alpha {
				Encoder::Vp9Alpha { cpu_used: 4, crf: 33 }
			} else {
				Encoder::SvtAv1 {
					preset: 8,
					params: String::new(),
				}
			},
			expected_frames: None,
			width: 320,
			height: 240,
			colour: ColourTags {
				primaries: "bt709".into(),
				transfer: "bt709".into(),
				matrix: "bt709".into(),
				range: Range::Limited,
			},
			alpha,
			hdr10,
			warnings: Vec::new(),
		}
	}

	fn output(video: &str, extra: &str) -> MediaInfo {
		MediaInfo::parse(
			format!(
				r#"{{"streams":[{video},
				{{"index":1,"codec_type":"audio","codec_name":"opus"}},
				{{"index":2,"codec_type":"video","codec_name":"mjpeg","disposition":{{"attached_pic":1}}}}{extra}],
				"chapters":[{{"id":1}}]}}"#
			)
			.as_bytes(),
		)
		.unwrap()
	}

	const AV1: &str = r#"{"index":0,"codec_type":"video","codec_name":"av1","width":320,"height":240,
		"color_range":"tv","color_space":"bt709","color_transfer":"bt709","color_primaries":"bt709"}"#;

	fn frame(json: &str) -> Frame {
		serde_json::from_str(json).unwrap()
	}

	fn tagged_frame() -> Frame {
		frame(r#"{"color_range":"tv","color_space":"bt709","color_transfer":"bt709","color_primaries":"bt709"}"#)
	}

	fn streams(audio: usize) -> StreamPlan {
		StreamPlan {
			audio: (0..audio)
				.map(|index| TrackJob {
					stream_index: u32::try_from(index + 1).unwrap(),
					sample_rate: 48_000,
					plan: TrackPlan::Copy,
				})
				.collect(),
			..StreamPlan::default()
		}
	}

	fn expected<'a>(video: &'a VideoPlan, streams: &'a StreamPlan) -> Expected<'a> {
		Expected {
			video,
			streams,
			source_chapters: 1,
			attached_files: 1,
		}
	}

	#[test]
	fn a_matching_output_passes() {
		let (video, streams) = (plan(false, false), streams(1));

		assert_eq!(
			check_output(&output(AV1, ""), Some(&tagged_frame()), &expected(&video, &streams)),
			Ok(())
		);
	}

	#[test]
	fn missing_streams_and_chapters_fail() {
		let (video, one_track, two_tracks) = (plan(false, false), streams(1), streams(2));
		let mut more_chapters = expected(&video, &one_track);
		more_chapters.source_chapters = 3;

		let reason = check_output(&output(AV1, ""), Some(&tagged_frame()), &expected(&video, &two_tracks)).unwrap_err();

		assert!(reason.contains("audio"), "{reason}");
		assert!(
			check_output(&output(AV1, ""), Some(&tagged_frame()), &more_chapters)
				.unwrap_err()
				.contains("chapters")
		);
	}

	#[test]
	fn colour_must_match_in_container_and_bitstream() {
		let (video, streams) = (plan(false, false), streams(1));
		let untagged_bitstream = frame(r#"{"color_range":"tv"}"#);
		let wrong_container = AV1.replace(r#""color_transfer":"bt709""#, r#""color_transfer":"smpte170m""#);

		assert!(check_output(&output(AV1, ""), Some(&untagged_bitstream), &expected(&video, &streams)).is_err());
		assert!(
			check_output(
				&output(&wrong_container, ""),
				Some(&tagged_frame()),
				&expected(&video, &streams)
			)
			.is_err()
		);
		assert!(check_output(&output(AV1, ""), None, &expected(&video, &streams)).is_err());
	}

	#[test]
	fn size_codec_dolby_vision_and_hdr10_are_checked() {
		let (video, streams) = (plan(false, false), streams(1));
		let hdr_plan = plan(false, true);
		let rotated_wrong = AV1.replace(r#""width":320,"height":240"#, r#""width":240,"height":320"#);
		let with_dovi = AV1.replace(
			r#""codec_name":"av1","#,
			r#""codec_name":"av1","side_data_list":[{"side_data_type":"DOVI configuration record"}],"#,
		);
		let with_mastering = AV1.replace(
			r#""codec_name":"av1","#,
			r#""codec_name":"av1","side_data_list":[{"side_data_type":"Mastering display metadata"}],"#,
		);
		let mastering_frame = frame(
			r#"{"color_range":"tv","color_space":"bt709","color_transfer":"bt709","color_primaries":"bt709",
			"side_data_list":[{"side_data_type":"Mastering display metadata"}]}"#,
		);

		assert!(
			check_output(
				&output(&rotated_wrong, ""),
				Some(&tagged_frame()),
				&expected(&video, &streams)
			)
			.is_err()
		);
		assert!(
			check_output(
				&output(&with_dovi, ""),
				Some(&tagged_frame()),
				&expected(&video, &streams)
			)
			.is_err()
		);
		assert!(check_output(&output(AV1, ""), Some(&tagged_frame()), &expected(&hdr_plan, &streams)).is_err());
		assert_eq!(
			check_output(
				&output(&with_mastering, ""),
				Some(&mastering_frame),
				&expected(&hdr_plan, &streams)
			),
			Ok(())
		);
	}

	#[test]
	fn transparent_video_needs_the_alpha_tag_but_not_bitstream_colour() {
		let (video, streams) = (plan(true, false), streams(1));
		let vp9 = |tags: &str| {
			format!(
				r#"{{"index":0,"codec_type":"video","codec_name":"vp9","width":320,"height":240,"color_range":"tv",
				"color_space":"bt709","color_transfer":"bt709","color_primaries":"bt709","tags":{{{tags}}}}}"#
			)
		};
		let vp9_frame = frame(r#"{"color_range":"tv","color_space":"bt709"}"#);

		assert_eq!(
			check_output(
				&output(&vp9(r#""ALPHA_MODE":"1""#), ""),
				Some(&vp9_frame),
				&expected(&video, &streams)
			),
			Ok(())
		);
		assert!(check_output(&output(&vp9(""), ""), Some(&vp9_frame), &expected(&video, &streams)).is_err());
	}

	#[test]
	fn parses_the_ssim_summary() {
		let lines = [
			"[Parsed_ssim_4 @ 0000026a4ea854c0] SSIM Y:0.997841 (26.656558) All:0.997841 (26.656558)".to_owned(),
			"[out#0/null @ 0000026a4e9f1c00] video:52KiB audio:0KiB".to_owned(),
		];

		assert_eq!(parse_ssim(&lines), Some(0.997_841));
		assert_eq!(parse_ssim(&lines[1..]), None);
		assert!(check_alpha(0.99).is_ok());
		assert!(check_alpha(0.0786).is_err());
	}
}
