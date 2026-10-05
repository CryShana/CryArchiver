use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::audio::plan::{FilePlan, LayoutFix, TrackPlan};
use crate::outcome::{ErrorKind, JobError};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};
use crate::toolchain::Toolchain;

const DURATION_TOLERANCE_S: f64 = 0.05;
const LEVEL_TOLERANCE_DB: f64 = 1.0;
const SILENCE_DB: f64 = -60.0;
const OVER_FULL_SCALE_DB: f64 = 0.5;
pub(crate) const STATS: &str = "measure_overall=Number_of_samples:measure_perchannel=RMS_level+Peak_level";

#[derive(Debug, Clone, PartialEq)]
pub struct Levels {
	pub duration_s: f64,
	pub rms_db: Vec<f64>,
	pub peak_db: Vec<f64>,
}

pub fn push_stats_output(cmd: &mut ToolCommand, streams: &[String]) {
	push_cut_stats_output(cmd, streams, "");
}

/// `push_stats_output` measuring only what `cut` (a filter such as `atrim`, or empty) keeps.
pub fn push_cut_stats_output(cmd: &mut ToolCommand, streams: &[String], cut: &str) {
	for stream in streams {
		cmd.arg("-map").arg(stream);
	}

	let cut = if cut.is_empty() {
		String::new()
	} else {
		format!("{cut},")
	};
	for track in 0..streams.len() {
		cmd.arg(format!("-filter:a:{track}"))
			.arg(format!("{cut}astats@s{track}={STATS}"));
	}
	cmd.args(["-f", "null", "-"]);
}

#[allow(clippy::cast_precision_loss)]
pub fn parse_levels(lines: &[String], track: usize, sample_rate: u32) -> Option<Levels> {
	let prefix = format!("[astats@s{track} @ ");
	let mut rms_db = Vec::new();
	let mut peak_db = Vec::new();
	let mut samples = None;

	for line in lines.iter().filter(|line| line.starts_with(&prefix)) {
		if let Some(value) = line.split("RMS level dB:").nth(1) {
			rms_db.push(value.trim().parse().ok()?);
		} else if let Some(value) = line.split("Peak level dB:").nth(1) {
			peak_db.push(value.trim().parse().ok()?);
		} else if let Some(value) = line.split("Number of samples:").nth(1) {
			samples = value.trim().parse::<u64>().ok();
		}
	}

	Some(Levels {
		duration_s: samples? as f64 / f64::from(sample_rate),
		rms_db,
		peak_db,
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

/// The warnings on success: a channel whose level doesn't match is excused with a warning when its source goes beyond
/// full scale, which the output can't carry, and clipping can explain the drop.
pub fn compare(
	source: &Levels,
	output: &Levels,
	check_levels: bool,
	lfe: Option<usize>,
) -> Result<Vec<String>, String> {
	let clipped = clipped_channels(source, output, check_levels, lfe)?;
	Ok(clipped_warning(&clipped, None).into_iter().collect())
}

/// The channels, counted from 1, whose level mismatch clipping explains.
fn clipped_channels(
	source: &Levels,
	output: &Levels,
	check_levels: bool,
	lfe: Option<usize>,
) -> Result<Vec<usize>, String> {
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
		return Ok(Vec::new());
	}

	let mut clipped = Vec::new();
	for (index, (&src, &out)) in source.rms_db.iter().zip(&output.rms_db).enumerate() {
		if Some(index) == lfe {
			continue;
		}

		let Some(mismatch) = level_mismatch(src, out) else {
			continue;
		};

		if source
			.peak_db
			.get(index)
			.is_some_and(|&peak| peak > OVER_FULL_SCALE_DB && clipping_explains(src, out, peak))
		{
			clipped.push(index + 1);
			continue;
		}

		return Err(format!("channel {} {mismatch}", index + 1));
	}
	Ok(clipped)
}

/// Clipping only lowers samples, those beyond full scale by at most `peak` dB, so it can only make a channel quieter,
/// and by no more than that.
fn clipping_explains(src: f64, out: f64, peak: f64) -> bool {
	out.is_finite() && src > out && src - out <= peak + LEVEL_TOLERANCE_DB
}

fn clipped_warning(channels: &[usize], track: Option<usize>) -> Option<String> {
	if channels.is_empty() {
		return None;
	}

	let track = track.map_or_else(String::new, |track| format!("track {track}, "));
	let noun = if channels.len() == 1 { "channel" } else { "channels" };
	Some(format!(
		"the source audio goes beyond full scale on {track}{noun} {}, likely damaged; it was clipped",
		channel_list(channels)
	))
}

fn level_mismatch(src: f64, out: f64) -> Option<String> {
	if src.is_nan() || out.is_nan() {
		return Some("level could not be measured".to_owned());
	}

	if src < SILENCE_DB {
		return None;
	}

	let delta = (src - out).abs();
	(delta > LEVEL_TOLERANCE_DB).then(|| format!("level differs by {delta:.1} dB"))
}

fn channel_list(channels: &[usize]) -> String {
	let names: Vec<String> = channels.iter().map(ToString::to_string).collect();
	match names.split_last() {
		Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
		_ => names.join(""),
	}
}

pub fn lfe_index(layout: &str) -> Option<usize> {
	matches!(layout, "5.1" | "6.1" | "7.1").then_some(3)
}

pub fn check_tracks(plan: &FilePlan, source: &[Levels], output: &[Levels]) -> Result<Vec<String>, String> {
	let several = plan.tracks.len() > 1;
	let mut warnings = Vec::new();

	for (index, (track, (source, measured))) in plan.tracks.iter().zip(source.iter().zip(output)).enumerate() {
		let TrackPlan::Encode(encoded) = &track.plan else {
			continue;
		};

		let check_levels = !matches!(encoded.layout, LayoutFix::Remix(_));
		let clipped = clipped_channels(source, measured, check_levels, lfe_index(encoded.layout.target()))?;
		warnings.extend(clipped_warning(&clipped, several.then_some(index + 1)));
	}
	Ok(warnings)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::audio::plan::{EncodePlan, FileOptions, TrackJob};
	use crate::container::OGG_OPUS;

	fn levels(duration_s: f64, rms_db: &[f64]) -> Levels {
		Levels {
			duration_s,
			rms_db: rms_db.to_vec(),
			peak_db: Vec::new(),
		}
	}

	fn peaking(levels: Levels, peak_db: &[f64]) -> Levels {
		Levels {
			peak_db: peak_db.to_vec(),
			..levels
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
			Ok(Vec::new())
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

		assert_eq!(compare(&source, &output, true, Some(3)), Ok(Vec::new()));
		assert_eq!(
			compare(
				&levels(10.0, &[-3.0, -3.0]),
				&levels(10.0, &[-20.0, -20.0]),
				false,
				None
			),
			Ok(Vec::new())
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
			Ok(Vec::new())
		);
	}

	#[test]
	fn a_mismatched_channel_beyond_full_scale_is_warned_instead_of_failed() {
		let source = peaking(levels(10.0, &[67.5, -12.0]), &[83.8, -9.0]);

		assert_eq!(
			compare(&source, &levels(10.0, &[-13.7, -12.2]), true, None),
			Ok(vec![
				"the source audio goes beyond full scale on channel 1, likely damaged; it was clipped".to_owned()
			])
		);
		assert!(
			compare(&source, &levels(10.0, &[-13.7, -30.0]), true, None)
				.unwrap_err()
				.contains("channel 2 level differs")
		);
		assert!(
			compare(
				&peaking(levels(10.0, &[-3.0, -3.0]), &[0.5, 0.0]),
				&levels(10.0, &[-4.3, -3.0]),
				true,
				None
			)
			.unwrap_err()
			.contains("channel 1 level differs")
		);
		assert_eq!(
			compare(
				&peaking(levels(10.0, &[-3.0, -3.0]), &[0.6, 0.0]),
				&levels(10.0, &[-4.3, -3.0]),
				true,
				None
			),
			Ok(vec![
				"the source audio goes beyond full scale on channel 1, likely damaged; it was clipped".to_owned()
			])
		);
	}

	#[test]
	fn only_a_drop_clipping_can_cause_is_excused() {
		let loud = peaking(levels(10.0, &[-0.4, -3.4]), &[1.9, -0.3]);
		let output = |level: f64| levels(10.0, &[level, -3.3]);

		assert_eq!(
			compare(&loud, &output(-2.9), true, None),
			Ok(vec![
				"the source audio goes beyond full scale on channel 1, likely damaged; it was clipped".to_owned()
			])
		);

		for (level, failure) in [
			(f64::NEG_INFINITY, "channel 1 level differs"),
			(f64::NAN, "channel 1 level could not be measured"),
			(2.6, "channel 1 level differs"),
			(-6.4, "channel 1 level differs"),
		] {
			let result = compare(&loud, &output(level), true, None);
			assert!(
				result.as_ref().is_err_and(|err| err.contains(failure)),
				"{level}: {result:?}"
			);
		}
	}

	#[test]
	fn an_unmeasurable_source_level_is_never_excused() {
		let damaged = peaking(levels(10.0, &[f64::NAN, -3.4]), &[83.8, -0.3]);

		assert!(
			compare(&damaged, &levels(10.0, &[-13.7, -3.3]), true, None)
				.unwrap_err()
				.contains("channel 1 level could not be measured")
		);
	}

	#[test]
	fn a_matching_channel_beyond_full_scale_gives_no_warning() {
		let loud = peaking(levels(10.0, &[-0.4, -3.4]), &[1.9, -0.3]);

		assert_eq!(compare(&loud, &levels(10.0, &[-0.5, -3.3]), true, None), Ok(Vec::new()));
	}

	#[test]
	fn channels_beyond_full_scale_are_listed_in_one_warning() {
		let warning = |peak_db: &[f64]| {
			let source_rms_db: Vec<f64> = peak_db.iter().map(|&peak| peak - 3.0).collect();
			let output_rms_db: Vec<f64> = peak_db.iter().map(|&peak| peak.min(0.0) - 3.0).collect();
			compare(
				&peaking(levels(10.0, &source_rms_db), peak_db),
				&levels(10.0, &output_rms_db),
				true,
				None,
			)
			.unwrap()
		};

		assert_eq!(
			warning(&[-6.0, 20.0, -6.0]),
			["the source audio goes beyond full scale on channel 2, likely damaged; it was clipped"]
		);
		assert_eq!(
			warning(&[20.0, 20.0]),
			["the source audio goes beyond full scale on channels 1 and 2, likely damaged; it was clipped"]
		);
		assert_eq!(
			warning(&[20.0, 20.0, -6.0, 20.0]),
			["the source audio goes beyond full scale on channels 1, 2 and 4, likely damaged; it was clipped"]
		);
	}

	#[test]
	fn the_warning_names_the_track_when_the_file_has_several() {
		let track = |stream_index| TrackJob {
			stream_index,
			sample_rate: 48_000,
			plan: TrackPlan::Encode(EncodePlan {
				bitrate_kbps: 128,
				channels: 2,
				layout: LayoutFix::Keep("stereo"),
			}),
		};
		let plan = |tracks| FilePlan {
			container: OGG_OPUS,
			tracks,
			options: FileOptions::default(),
		};
		let clipped = peaking(levels(10.0, &[17.0, 17.0]), &[20.0, 20.0]);
		let fine = peaking(levels(10.0, &[-3.0, -3.0]), &[-1.0, -1.0]);
		let output = levels(10.0, &[-3.0, -3.0]);

		assert_eq!(
			check_tracks(
				&plan(vec![track(0), track(1), track(2)]),
				&[
					peaking(levels(10.0, &[17.0, -3.0]), &[20.0, -1.0]),
					fine,
					clipped.clone()
				],
				&[output.clone(), output.clone(), output.clone()]
			),
			Ok(vec![
				"the source audio goes beyond full scale on track 1, channel 1, likely damaged; it was clipped"
					.to_owned(),
				"the source audio goes beyond full scale on track 3, channels 1 and 2, likely damaged; it was clipped"
					.to_owned(),
			])
		);
		assert_eq!(
			check_tracks(&plan(vec![track(0)]), &[clipped], &[output]),
			Ok(vec![
				"the source audio goes beyond full scale on channels 1 and 2, likely damaged; it was clipped"
					.to_owned()
			])
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
	fn parses_each_channel_peak() {
		let lines = [
			"[astats@s0 @ 0000029c1411b440] Channel: 1",
			"[astats@s0 @ 0000029c1411b440] Peak level dB: 83.800000",
			"[astats@s0 @ 0000029c1411b440] RMS level dB: 67.500000",
			"[astats@s0 @ 0000029c1411b440] Channel: 2",
			"[astats@s0 @ 0000029c1411b440] Peak level dB: -inf",
			"[astats@s0 @ 0000029c1411b440] RMS level dB: -inf",
			"[astats@s0 @ 0000029c1411b440] Overall",
			"[astats@s0 @ 0000029c1411b440] Number of samples: 48000",
		]
		.map(str::to_owned);

		assert_eq!(
			parse_levels(&lines, 0, 48_000),
			Some(peaking(
				levels(1.0, &[67.5, f64::NEG_INFINITY]),
				&[83.8, f64::NEG_INFINITY]
			))
		);
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
