use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::audio::plan::{EncodePlan, FilePlan, LayoutFix, TrackPlan};
use crate::audio::verify::{Levels, parse_levels, push_stats_output};
use crate::container::Cover;
use crate::outcome::{ErrorKind, JobError};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};

const RESAMPLE: &str = "aresample=resampler=soxr:osr=48000";

pub fn command(ffmpeg: &Path, source: &Path, output: &Path, plan: &FilePlan) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-y", "-i"])
		.arg(process::tool_path(source))
		.args(["-map_metadata", "0", "-map_chapters", "0"]);

	for (out, track) in plan.tracks.iter().enumerate() {
		cmd.arg("-map").arg(format!("0:{}", track.stream_index));
		match &track.plan {
			TrackPlan::Encode(encode) => push_encode(&mut cmd, out, encode),
			TrackPlan::Copy => {
				cmd.arg(format!("-c:a:{out}")).arg("copy");
			}
		}
	}

	if plan.container.cover == Cover::Stream {
		cmd.args(["-map", "0:v?", "-c:v", "copy"]);
	}
	cmd.args(["-f", plan.container.muxer]).arg(process::tool_path(output));

	let sources: Vec<String> = plan
		.tracks
		.iter()
		.map(|track| format!("0:{}", track.stream_index))
		.collect();
	push_stats_output(&mut cmd, &sources);
	cmd
}

pub async fn run(
	cmd: &ToolCommand,
	plan: &FilePlan,
	duration_s: Option<f64>,
	mut on_fraction: impl FnMut(f64),
	cancel: &CancellationToken,
) -> Result<Vec<Levels>, JobError> {
	let out = process::run(
		cmd,
		StdoutMode::Progress,
		|progress| {
			if let (Some(total), Some(done)) = (duration_s, progress.out_time) {
				on_fraction((done.as_secs_f64() / total).clamp(0.0, 1.0));
			}
		},
		cancel,
	)
	.await?;

	if !out.completed() {
		let reason = process::failure_reason(&out.stderr_tail);
		return Err(JobError::with_log(ErrorKind::EncodeFailed(reason), out.stderr_tail));
	}

	let levels: Option<Vec<Levels>> = plan
		.tracks
		.iter()
		.enumerate()
		.map(|(track, job)| parse_levels(&out.stderr_tail, track, job.sample_rate))
		.collect();
	levels.ok_or_else(|| {
		JobError::with_log(
			ErrorKind::EncodeFailed("the source levels are missing from the log".into()),
			out.stderr_tail,
		)
	})
}

fn push_encode(cmd: &mut ToolCommand, out: usize, plan: &EncodePlan) {
	cmd.arg(format!("-c:a:{out}")).arg("libopus");
	cmd.arg(format!("-b:a:{out}")).arg(format!("{}k", plan.bitrate_kbps));
	cmd.arg(format!("-filter:a:{out}")).arg(filter_chain(plan.layout));

	if plan.channels > 2 {
		cmd.arg(format!("-mapping_family:a:{out}")).arg("1");
	}
}

fn filter_chain(layout: LayoutFix) -> String {
	match layout {
		LayoutFix::Keep(_) => RESAMPLE.to_owned(),
		LayoutFix::Relabel(target) => format!("channelmap=channel_layout={target},{RESAMPLE}"),
		LayoutFix::Remix(target) => format!("{RESAMPLE}:ochl={target}"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::audio::plan::TrackJob;
	use crate::container::{MATROSKA_AUDIO, OGG_OPUS};
	use crate::process::tool_path;

	fn joined(cmd: &ToolCommand) -> String {
		cmd.args
			.iter()
			.map(|arg| arg.to_string_lossy().into_owned())
			.collect::<Vec<_>>()
			.join(" ")
	}

	fn track(stream_index: u32, plan: TrackPlan) -> TrackJob {
		TrackJob {
			stream_index,
			sample_rate: 48_000,
			plan,
		}
	}

	#[test]
	fn builds_arguments_for_a_relabelled_surround_track() {
		let plan = EncodePlan {
			bitrate_kbps: 256,
			channels: 6,
			layout: LayoutFix::Relabel("5.1"),
		};
		let file = FilePlan {
			container: OGG_OPUS,
			tracks: vec![track(0, TrackPlan::Encode(plan))],
		};

		let cmd = command(Path::new("ffmpeg"), Path::new("in.ac3"), Path::new("out.opus"), &file);

		let (source, output) = (tool_path(Path::new("in.ac3")), tool_path(Path::new("out.opus")));
		assert_eq!(
			joined(&cmd),
			format!(
				"-hide_banner -nostdin -y -i {} -map_metadata 0 -map_chapters 0 -map 0:0 -c:a:0 libopus -b:a:0 256k \
				 -filter:a:0 channelmap=channel_layout=5.1,aresample=resampler=soxr:osr=48000 -mapping_family:a:0 1 \
				 -f opus {} -map 0:0 -filter:a:0 astats@s0=measure_overall=Number_of_samples:measure_perchannel=RMS_level \
				 -f null -",
				source.display(),
				output.display()
			)
		);
	}

	#[test]
	fn multi_track_files_keep_copied_tracks_and_cover_streams() {
		let stereo = EncodePlan {
			bitrate_kbps: 128,
			channels: 2,
			layout: LayoutFix::Keep("stereo"),
		};
		let file = FilePlan {
			container: MATROSKA_AUDIO,
			tracks: vec![track(0, TrackPlan::Encode(stereo)), track(1, TrackPlan::Copy)],
		};

		let args = joined(&command(
			Path::new("ffmpeg"),
			Path::new("in.mka"),
			Path::new("out.mka"),
			&file,
		));

		let output = tool_path(Path::new("out.mka"));
		assert!(args.contains("-map 0:1 -c:a:1 copy"));
		assert!(args.contains(&format!(
			"-map 0:v? -c:v copy -f matroska {} -map 0:0 -map 0:1 -filter:a:0 astats@s0=",
			output.display()
		)));
		assert!(args.contains("-filter:a:1 astats@s1="));
	}

	#[test]
	fn filter_chains_resample_and_fix_layouts() {
		assert_eq!(
			filter_chain(LayoutFix::Keep("stereo")),
			"aresample=resampler=soxr:osr=48000"
		);
		assert_eq!(
			filter_chain(LayoutFix::Relabel("5.1")),
			"channelmap=channel_layout=5.1,aresample=resampler=soxr:osr=48000"
		);
		assert_eq!(
			filter_chain(LayoutFix::Remix("5.1")),
			"aresample=resampler=soxr:osr=48000:ochl=5.1"
		);
	}
}
