use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::outcome::{ErrorKind, JobError};
use crate::probe::{self, Frame, MediaInfo, ProbeError, Stream};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};
use crate::toolchain::Toolchain;
use crate::video::mux::{MuxInput, OutputStream};
use crate::video::plan::{Encoder, VideoPlan};
use crate::video::streams::StreamPlan;
use crate::video::{burn, delay, run_ffmpeg};

const MIN_ALPHA_SSIM: f64 = 0.98;
const MIN_START_TOLERANCE_S: f64 = 0.05;
const MIN_SYNC_TOLERANCE_S: f64 = 0.02;
/// Packets read to find the ones that don't decode: they all come before the first decodable GOP.
const LEAD_PACKETS: &str = "%+#1000";
const MASTERING_DISPLAY: &str = "Mastering display metadata";
const DOVI_RECORD: &str = "DOVI configuration record";

pub struct Expected<'a> {
	pub video: &'a VideoPlan,
	pub streams: &'a StreamPlan,
	pub source_chapters: usize,
	pub attached_files: usize,
	pub start_offsets: Vec<Option<f64>>,
	pub source_fps: Option<f64>,
}

pub struct Timelines<'a> {
	pub source: &'a MediaInfo,
	pub video_temp: &'a MediaInfo,
	pub audio_temp: Option<&'a MediaInfo>,
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

	check_start_offsets(output, expected)
}

pub async fn verify_output(
	tools: &Toolchain,
	output: &Path,
	expected: &Expected<'_>,
	cancel: &CancellationToken,
) -> Result<(), JobError> {
	let (info, frames) = tokio::try_join!(
		Box::pin(probe::probe(tools, output, cancel)),
		Box::pin(probe::first_frames(tools, output, 0, 1, cancel))
	)
	.map_err(unreadable)?;

	check_output(&info, frames.first(), expected)
		.map_err(|reason| ErrorKind::EncodeFailed(format!("verification failed: {reason}")).into())
}

pub async fn probe_temp(tools: &Toolchain, temp: &Path, cancel: &CancellationToken) -> Result<MediaInfo, JobError> {
	probe::probe(tools, temp, cancel).await.map_err(unreadable)
}

pub fn earliest_video_start(info: &MediaInfo) -> Option<f64> {
	info.streams
		.iter()
		.filter(|stream| stream.is_real_video())
		.filter_map(|stream| stream.start_time)
		.reduce(f64::min)
}

// Encoded streams are measured in their temps rather than in the source: a stream's start_time there counts
// packets the decoder drops (edit-list pre-roll, leading frames before the first keyframe).
pub fn start_offsets(outputs: &[OutputStream], timelines: &Timelines<'_>) -> Vec<Option<f64>> {
	let video_start = earliest_video_start(timelines.video_temp);

	outputs
		.iter()
		.map(|output| Some(landing(output, timelines)? - video_start?))
		.collect()
}

/// An output stream whose start is checked against its source stream's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Synced {
	pub source: u32,
	pub output: u32,
	pub audio: bool,
}

/// Where a stream's first decoded frame lands, in the source and in the output, each on its file's own timestamps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FirstFrames {
	/// The stream's index in the output.
	pub output: u32,
	pub source_s: Option<f64>,
	pub output_s: Option<f64>,
}

/// The output's audio and video streams, main video first. A merged track has no single source to start with.
pub fn synced_streams(outputs: &[OutputStream], source: &MediaInfo, merge_from: &[u32]) -> Vec<Synced> {
	outputs
		.iter()
		.zip(0..)
		.filter_map(|(output, position)| {
			let stream = source.streams.iter().find(|stream| stream.index == output.origin)?;
			let audio = stream.codec_type == "audio";
			let synced = stream.is_real_video() || (audio && !merge_from.contains(&output.origin));
			synced.then_some(Synced {
				source: output.origin,
				output: position,
				audio,
			})
		})
		.collect()
}

pub fn sync_tolerance_s(fps: Option<f64>) -> f64 {
	fps.filter(|fps| *fps > 0.0)
		.map_or(MIN_SYNC_TOLERANCE_S, |fps| (1.0 / fps).max(MIN_SYNC_TOLERANCE_S))
}

/// `starts` follow `synced_streams`. The output's timestamps count from `zero_s` on the source's.
///
/// Each stream must start as far from the main video as in the source. In a trim, only streams that begin inside
/// the window have a start to compare: one already running at the cut starts wherever the cut's first decodable
/// frame falls. When the main video is one of those, the others are measured from the cut instead.
pub fn sync_error(starts: &[FirstFrames], zero_s: f64, trimmed: bool, tolerance_s: f64) -> Option<String> {
	let expected: Vec<Option<f64>> = starts
		.iter()
		.map(|start| {
			let expected = start.source_s? - zero_s;
			(!trimmed || expected > tolerance_s).then_some(expected)
		})
		.collect();
	let main = starts
		.first()
		.and_then(|main| Some((expected.first().copied()??, main.output_s?)));
	let (anchor, (expected_anchor, output_anchor)) = match main {
		Some(main) => ("the main video", main),
		None => ("the cut", (0.0, 0.0)),
	};

	starts
		.iter()
		.zip(&expected)
		.skip(usize::from(main.is_some()))
		.find_map(|(start, expected)| {
			let wanted = expected.as_ref()? - expected_anchor;
			let actual = start.output_s? - output_anchor;
			((actual - wanted).abs() > tolerance_s).then(|| {
				format!(
					"stream {} starts {actual:.3} s after {anchor}, {wanted:.3} s in the source",
					start.output
				)
			})
		})
}

/// When `stream`'s first decoded frame is shown, on `path`'s own timestamps; `None` when nothing decodes.
pub async fn first_decoded_s(
	tools: &Toolchain,
	path: &Path,
	stream: u32,
	audio: bool,
	cancel: &CancellationToken,
) -> Result<Option<f64>, JobError> {
	let (frames, filter, info) = if audio {
		("-frames:a", "-af", "ashowinfo")
	} else {
		("-frames:v", "-vf", "showinfo")
	};
	let mut cmd = ToolCommand::new(&tools.ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-copyts", "-i"])
		.arg(process::tool_path(path))
		.arg("-map")
		.arg(format!("0:{stream}"))
		.args([frames, "1", filter, info, "-f", "null", "-"]);

	let out = run_ffmpeg(&cmd, |_| {}, cancel).await?;
	Ok(out
		.stderr_tail
		.iter()
		.filter(|line| line.contains(&format!("Parsed_{info}")))
		.find_map(|line| {
			line.split_whitespace()
				.find_map(|field| field.strip_prefix("pts_time:"))?
				.parse()
				.ok()
		}))
}

/// How many of video `stream`'s leading packets never decode into a frame: those shown before its first decoded
/// frame, such as an open GOP's leading B-frames or the frames before the first keyframe of a TS cut mid-GOP.
pub async fn undecoded_lead(
	tools: &Toolchain,
	path: &Path,
	stream: u32,
	cancel: &CancellationToken,
) -> Result<u64, JobError> {
	let Some(first_s) = first_decoded_s(tools, path, stream, false, cancel).await? else {
		return Ok(0);
	};

	let mut cmd = ToolCommand::new(&tools.ffprobe, STALL_TIMEOUT);
	cmd.args(["-v", "error", "-select_streams"])
		.arg(stream.to_string())
		.args(["-show_entries", "packet=pts_time", "-read_intervals", LEAD_PACKETS])
		.args(["-of", "csv=p=0", "-i"])
		.arg(process::tool_path(path));
	let out = process::run(&cmd, StdoutMode::Capture, |_| {}, cancel).await?;
	if !out.status.success() {
		return Err(JobError::with_log(
			ErrorKind::EncodeFailed("the source's first video packets can't be listed".into()),
			out.stderr_tail,
		));
	}

	Ok(undecoded_count(&String::from_utf8_lossy(&out.stdout), first_s))
}

/// `packets` is ffprobe's `pts_time` list, one per line.
pub fn undecoded_count(packets: &str, first_decoded_s: f64) -> u64 {
	let count = packets
		.lines()
		.filter_map(|line| line.trim().trim_end_matches(',').parse::<f64>().ok())
		.filter(|pts_s| delay(first_decoded_s - pts_s).is_some())
		.count();
	u64::try_from(count).unwrap_or(u64::MAX)
}

/// Fails when an output stream doesn't start as far from the main video as it does in the source (`sync_error`).
pub async fn check_sync(
	tools: &Toolchain,
	(source, output): (&Path, &Path),
	streams: &[Synced],
	(zero_s, trimmed): (f64, bool),
	tolerance_s: f64,
	cancel: &CancellationToken,
) -> Result<(), JobError> {
	let mut starts = Vec::with_capacity(streams.len());
	for stream in streams {
		starts.push(FirstFrames {
			output: stream.output,
			source_s: first_decoded_s(tools, source, stream.source, stream.audio, cancel).await?,
			output_s: first_decoded_s(tools, output, stream.output, stream.audio, cancel).await?,
		});
	}

	match sync_error(&starts, zero_s, trimmed, tolerance_s) {
		Some(reason) => Err(ErrorKind::EncodeFailed(format!("verification failed: {reason}")).into()),
		None => Ok(()),
	}
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
	if let Some(window) = video.window {
		cmd.seek(window.start_s, window.length_s);
	}
	if let Some(decoder) = video.decoder {
		cmd.arg("-c:v").arg(decoder);
	}
	cmd.arg("-i")
		.arg(process::tool_path(source))
		.arg("-filter_complex")
		.arg(format!(
			"[0:v:0]alphaextract,format=gray[o];[1:{}]{},alphaextract,format=gray[s];[o][s]ssim",
			video.streams[0],
			burn::chained(&[&video.shape, &video.filters])
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

fn landing(output: &OutputStream, timelines: &Timelines<'_>) -> Option<f64> {
	match output.input {
		MuxInput::VideoTemp(ordinal) => {
			let mut videos = timelines
				.video_temp
				.streams
				.iter()
				.filter(|stream| stream.is_real_video());
			videos.nth(ordinal)?.start_time
		}
		MuxInput::AudioTemp(ordinal) => timelines.audio_temp?.audio_streams().nth(ordinal)?.start_time,
		MuxInput::Source => {
			let stream = timelines
				.source
				.streams
				.iter()
				.find(|stream| stream.index == output.origin)?;
			Some(stream.start_time? - timelines.source.format.start_time.unwrap_or(0.0))
		}
	}
}

fn check_start_offsets(output: &MediaInfo, expected: &Expected<'_>) -> Result<(), String> {
	let Some(video_start) = earliest_video_start(output) else {
		return Ok(());
	};
	let tolerance = expected
		.source_fps
		.map_or(MIN_START_TOLERANCE_S, |fps| (1.0 / fps).max(MIN_START_TOLERANCE_S));

	for (stream, wanted) in output.streams.iter().zip(&expected.start_offsets) {
		let (Some(start), Some(wanted)) = (stream.start_time, wanted) else {
			continue;
		};

		// ffprobe reports a Matroska subtitle stream as starting at 0 whenever other streams do, whatever its first
		// cue.
		if stream.codec_type == "subtitle" {
			continue;
		}

		let actual = start - video_start;
		if (actual - wanted).abs() > tolerance {
			return Err(format!(
				"stream {} starts {actual:.3} s after the video, {wanted:.3} s expected",
				stream.index
			));
		}
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
	use crate::video::plan::test_plan;

	fn plan(alpha: bool, hdr10: bool) -> VideoPlan {
		VideoPlan {
			encoder: if alpha {
				Encoder::Vp9Alpha { cpu_used: 4, crf: 33 }
			} else {
				test_plan().encoder
			},
			alpha,
			hdr10,
			..test_plan()
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
			start_offsets: Vec::new(),
			source_fps: None,
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
	fn streams_must_start_where_they_started_against_the_video() {
		let (video, streams) = (plan(false, false), streams(1));
		let delayed = MediaInfo {
			streams: output(AV1, "")
				.streams
				.into_iter()
				.zip([Some(1.32), Some(1.8), None])
				.map(|(stream, start_time)| Stream { start_time, ..stream })
				.collect(),
			..output(AV1, "")
		};
		let with_offsets = |start_offsets: Vec<Option<f64>>, source_fps: Option<f64>| Expected {
			start_offsets,
			source_fps,
			..expected(&video, &streams)
		};

		assert_eq!(
			check_output(
				&delayed,
				Some(&tagged_frame()),
				&with_offsets(vec![Some(0.0), Some(0.5)], None)
			),
			Ok(())
		);
		assert_eq!(
			check_output(
				&delayed,
				Some(&tagged_frame()),
				&with_offsets(vec![Some(0.0), None], None)
			),
			Ok(())
		);
		assert_eq!(
			check_output(
				&delayed,
				Some(&tagged_frame()),
				&with_offsets(vec![Some(0.0), Some(0.0)], None)
			),
			Err("stream 1 starts 0.480 s after the video, 0.000 s expected".into())
		);
		assert!(
			check_output(
				&delayed,
				Some(&tagged_frame()),
				&with_offsets(vec![Some(0.0), Some(0.4)], None)
			)
			.is_err()
		);
		assert_eq!(
			check_output(
				&delayed,
				Some(&tagged_frame()),
				&with_offsets(vec![Some(0.0), Some(0.4)], Some(10.0))
			),
			Ok(())
		);
	}

	#[test]
	fn subtitle_starts_are_not_compared() {
		let (video, streams) = (plan(false, false), streams(1));
		let muxed = MediaInfo::parse(
			br#"{"streams":[{"index":0,"codec_type":"video","start_time":"0.000000"},
			{"index":1,"codec_type":"subtitle","start_time":"0.000000"}]}"#,
		)
		.unwrap();
		let expected = Expected {
			start_offsets: vec![Some(0.0), Some(0.2)],
			source_fps: None,
			..expected(&video, &streams)
		};

		assert_eq!(check_start_offsets(&muxed, &expected), Ok(()));
	}

	#[test]
	fn encoded_streams_are_placed_by_their_temps_and_copies_by_the_source() {
		let source = MediaInfo::parse(
			br#"{"streams":[{"index":0,"codec_type":"video","start_time":"-1.300000"},
			{"index":1,"codec_type":"audio","start_time":"-1.250000"},
			{"index":2,"codec_type":"audio","start_time":"0.500000"},
			{"index":3,"codec_type":"subtitle"}],
			"format":{"start_time":"-1.250000"}}"#,
		)
		.unwrap();
		let video_temp =
			MediaInfo::parse(br#"{"streams":[{"index":0,"codec_type":"video","start_time":"1.250000"}]}"#).unwrap();
		let audio_temp =
			MediaInfo::parse(br#"{"streams":[{"index":0,"codec_type":"audio","start_time":"1.750000"}]}"#).unwrap();
		let outputs = [
			OutputStream {
				origin: 0,
				input: MuxInput::VideoTemp(0),
			},
			OutputStream {
				origin: 1,
				input: MuxInput::AudioTemp(0),
			},
			OutputStream {
				origin: 2,
				input: MuxInput::Source,
			},
			OutputStream {
				origin: 3,
				input: MuxInput::Source,
			},
		];

		let offsets = start_offsets(
			&outputs,
			&Timelines {
				source: &source,
				video_temp: &video_temp,
				audio_temp: Some(&audio_temp),
			},
		);

		assert_eq!(offsets, [Some(0.0), Some(0.5), Some(0.5), None]);
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

	#[test]
	fn every_stream_must_start_as_far_from_the_main_video_as_in_the_source() {
		let starts = |pairs: &[(f64, f64)]| -> Vec<FirstFrames> {
			pairs
				.iter()
				.zip(0..)
				.map(|(&(source_s, output_s), output)| FirstFrames {
					output,
					source_s: Some(source_s),
					output_s: Some(output_s),
				})
				.collect()
		};
		let whole = |pairs: &[(f64, f64)]| sync_error(&starts(pairs), 1.4, false, 0.04);
		let cut = |pairs: &[(f64, f64)]| sync_error(&starts(pairs), 5.4, true, 0.04);

		assert_eq!(whole(&[(1.743, 0.343), (1.4, 0.0)]), None);
		assert_eq!(
			whole(&[(1.743, 0.0), (1.4, 0.0)]).unwrap(),
			"stream 1 starts 0.000 s after the main video, -0.343 s in the source"
		);
		assert_eq!(whole(&[(1.743, 5.343), (1.4, 5.0)]), None);
		assert_eq!(whole(&[(1.4, 0.0), (1.743, 0.36)]), None);
		assert!(whole(&[(1.4, 0.0), (1.743, 0.4)]).is_some());

		assert_eq!(cut(&[(1.743, 0.32), (1.4, 0.0), (6.0, 0.6)]), None);
		assert_eq!(
			cut(&[(1.743, 0.32), (1.4, 0.0), (6.0, 0.9)]).unwrap(),
			"stream 2 starts 0.900 s after the cut, 0.600 s in the source"
		);
		assert_eq!(cut(&[(5.743, 0.343), (1.4, 0.0), (6.0, 0.6)]), None);
		assert!(cut(&[(5.743, 0.343), (1.4, 0.0), (6.0, 0.3)]).is_some());

		let undecodable = [FirstFrames {
			output: 0,
			source_s: None,
			output_s: Some(0.0),
		}];
		assert_eq!(sync_error(&undecodable, 0.0, false, 0.04), None);
	}

	#[test]
	fn packets_shown_before_the_first_decoded_frame_are_the_ones_that_do_not_decode() {
		let packets = "3.240000,\n3.400000,\n3.320000,\nN/A,\n3.280000\n3.360000\n3.560000\n3.480000,\n";

		assert_eq!(undecoded_count(packets, 3.4), 4);
		assert_eq!(undecoded_count(packets, 3.24), 0);
		assert_eq!(undecoded_count("", 3.4), 0);
	}

	#[test]
	fn the_sync_tolerance_is_a_frame_or_20_ms() {
		assert!((sync_tolerance_s(Some(25.0)) - 0.04).abs() < 1e-9);
		assert!((sync_tolerance_s(Some(120.0)) - 0.02).abs() < 1e-9);
		assert!((sync_tolerance_s(None) - 0.02).abs() < 1e-9);
	}
}
