use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::audio::plan::{FilePlan, LayoutFix, TrackPlan};
use crate::outcome::{ErrorKind, JobError};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};
use crate::toolchain::Toolchain;

const DURATION_TOLERANCE_S: f64 = 0.05;
const LEVEL_TOLERANCE_DB: f64 = 1.0;
const SILENCE_DB: f64 = -60.0;
pub(crate) const STATS: &str = "measure_overall=Number_of_samples:measure_perchannel=RMS_level";

#[derive(Debug, Clone, PartialEq)]
pub struct Levels {
	pub duration_s: f64,
	pub rms_db: Vec<f64>,
}

pub fn push_stats_output(cmd: &mut ToolCommand, streams: &[String]) {
	for stream in streams {
		cmd.arg("-map").arg(stream);
	}

	for track in 0..streams.len() {
		cmd.arg(format!("-filter:a:{track}"))
			.arg(format!("astats@s{track}={STATS}"));
	}
	cmd.args(["-f", "null", "-"]);
}

#[allow(clippy::cast_precision_loss)]
pub fn parse_levels(lines: &[String], track: usize, sample_rate: u32) -> Option<Levels> {
	let prefix = format!("[astats@s{track} @ ");
	let mut rms_db = Vec::new();
	let mut samples = None;

	for line in lines.iter().filter(|line| line.starts_with(&prefix)) {
		if let Some(value) = line.split("RMS level dB:").nth(1) {
			rms_db.push(value.trim().parse().ok()?);
		} else if let Some(value) = line.split("Number of samples:").nth(1) {
			samples = value.trim().parse::<u64>().ok();
		}
	}

	Some(Levels {
		duration_s: samples? as f64 / f64::from(sample_rate),
		rms_db,
	})
}

pub async fn measure_output(
	tools: &Toolchain,
	output: &Path,
	plan: &FilePlan,
	cancel: &CancellationToken,
) -> Result<Vec<Levels>, JobError> {
	let streams: Vec<String> = (0..plan.tracks.len()).map(|track| format!("0:a:{track}")).collect();
	let mut cmd = ToolCommand::new(&tools.ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-i"])
		.arg(process::tool_path(output));
	push_stats_output(&mut cmd, &streams);

	let out = process::run(&cmd, StdoutMode::Progress, |_| {}, cancel).await?;
	let levels: Option<Vec<Levels>> = plan
		.tracks
		.iter()
		.enumerate()
		.map(|(track, job)| parse_levels(&out.stderr_tail, track, job.output_rate()))
		.collect();

	match levels {
		Some(levels) if out.completed() => Ok(levels),
		_ => Err(JobError::with_log(
			ErrorKind::EncodeFailed("the output could not be decoded".into()),
			out.stderr_tail,
		)),
	}
}

pub fn compare(source: &Levels, output: &Levels, check_levels: bool, lfe: Option<usize>) -> Result<(), String> {
	let drift = (source.duration_s - output.duration_s).abs();
	if drift > DURATION_TOLERANCE_S {
		return Err(format!(
			"duration differs by {drift:.3} s ({:.3} s vs {:.3} s)",
			source.duration_s, output.duration_s
		));
	}

	if source.rms_db.len() != output.rms_db.len() {
		return Err(format!(
			"channel count differs: {} vs {}",
			source.rms_db.len(),
			output.rms_db.len()
		));
	}

	if !check_levels {
		return Ok(());
	}

	for (index, (src, out)) in source.rms_db.iter().zip(&output.rms_db).enumerate() {
		if Some(index) == lfe {
			continue;
		}

		if src.is_nan() || out.is_nan() {
			return Err(format!("channel {} level could not be measured", index + 1));
		}

		if *src < SILENCE_DB {
			continue;
		}

		let delta = (src - out).abs();
		if delta > LEVEL_TOLERANCE_DB {
			return Err(format!("channel {} level differs by {delta:.1} dB", index + 1));
		}
	}
	Ok(())
}

pub fn lfe_index(layout: &str) -> Option<usize> {
	matches!(layout, "5.1" | "6.1" | "7.1").then_some(3)
}

pub fn check_tracks(plan: &FilePlan, source: &[Levels], output: &[Levels]) -> Result<(), String> {
	for (track, (source, measured)) in plan.tracks.iter().zip(source.iter().zip(output)) {
		let TrackPlan::Encode(encoded) = &track.plan else {
			continue;
		};

		let check_levels = !matches!(encoded.layout, LayoutFix::Remix(_));
		compare(source, measured, check_levels, lfe_index(encoded.layout.target()))?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn levels(duration_s: f64, rms_db: &[f64]) -> Levels {
		Levels {
			duration_s,
			rms_db: rms_db.to_vec(),
		}
	}

	#[test]
	fn accepts_matching_audio() {
		assert_eq!(
			compare(
				&levels(10.0, &[-12.0, -13.0]),
				&levels(10.03, &[-12.4, -12.6]),
				true,
				None
			),
			Ok(())
		);
	}

	#[test]
	fn rejects_duration_channel_and_level_drift() {
		assert!(
			compare(&levels(10.0, &[-12.0]), &levels(9.9, &[-12.0]), true, None)
				.unwrap_err()
				.contains("duration")
		);
		assert!(
			compare(&levels(10.0, &[-12.0, -12.0]), &levels(10.0, &[-12.0]), true, None)
				.unwrap_err()
				.contains("channel count")
		);
		assert!(
			compare(
				&levels(10.0, &[-12.0, -12.0]),
				&levels(10.0, &[-12.0, -30.0]),
				true,
				None
			)
			.unwrap_err()
			.contains("channel 2")
		);
	}

	#[test]
	fn skips_lfe_silent_channels_and_remixed_levels() {
		let source = levels(10.0, &[-12.0, -12.0, -12.0, -20.0, f64::NEG_INFINITY, -12.0]);
		let output = levels(10.0, &[-12.0, -12.0, -12.0, -35.0, -80.0, -12.0]);

		assert_eq!(compare(&source, &output, true, Some(3)), Ok(()));
		assert_eq!(
			compare(
				&levels(10.0, &[-3.0, -3.0]),
				&levels(10.0, &[-20.0, -20.0]),
				false,
				None
			),
			Ok(())
		);
	}

	#[test]
	fn unmeasurable_levels_fail() {
		assert!(
			compare(
				&levels(10.0, &[-12.0, f64::NAN]),
				&levels(10.0, &[-12.0, -12.0]),
				true,
				None
			)
			.unwrap_err()
			.contains("channel 2")
		);
		assert!(
			compare(
				&levels(10.0, &[-12.0, -12.0]),
				&levels(10.0, &[-12.0, f64::NAN]),
				true,
				None
			)
			.unwrap_err()
			.contains("channel 2")
		);
		assert_eq!(
			compare(
				&levels(10.0, &[-12.0, f64::NAN]),
				&levels(10.0, &[-12.0, -12.0]),
				true,
				Some(1)
			),
			Ok(())
		);
	}

	#[test]
	fn lfe_sits_after_the_centre_in_standard_layouts() {
		assert_eq!(lfe_index("5.1"), Some(3));
		assert_eq!(lfe_index("7.1"), Some(3));
		assert_eq!(lfe_index("stereo"), None);
		assert_eq!(lfe_index("quad"), None);
	}

	#[test]
	fn parses_labelled_astats_in_any_order() {
		let lines = [
			"[astats@s1 @ 0000029c14119a40] Channel: 1",
			"[astats@s1 @ 0000029c14119a40] RMS level dB: -16.989700",
			"[astats@s1 @ 0000029c14119a40] Overall",
			"[astats@s1 @ 0000029c14119a40] Number of samples: 144000",
			"[astats@s0 @ 0000029c1411b440] Channel: 1",
			"[astats@s0 @ 0000029c1411b440] RMS level dB: -13.467875",
			"[astats@s0 @ 0000029c1411b440] Channel: 2",
			"[astats@s0 @ 0000029c1411b440] RMS level dB: -inf",
			"[astats@s0 @ 0000029c1411b440] Overall",
			"[astats@s0 @ 0000029c1411b440] Number of samples: 88200",
		]
		.map(str::to_owned);

		assert_eq!(
			parse_levels(&lines, 0, 44_100),
			Some(levels(2.0, &[-13.467_875, f64::NEG_INFINITY]))
		);
		assert_eq!(parse_levels(&lines, 1, 48_000), Some(levels(3.0, &[-16.9897])));
		assert_eq!(parse_levels(&lines, 2, 48_000), None);
	}

	#[test]
	fn stats_output_maps_and_labels_each_stream() {
		let mut cmd = ToolCommand::new(Path::new("ffmpeg"), STALL_TIMEOUT);

		push_stats_output(&mut cmd, &["0:0".to_owned(), "0:2".to_owned()]);

		let args: Vec<String> = cmd.args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
		assert_eq!(
			args.join(" "),
			format!("-map 0:0 -map 0:2 -filter:a:0 astats@s0={STATS} -filter:a:1 astats@s1={STATS} -f null -")
		);
	}
}
