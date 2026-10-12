use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use cryarchiver_engine::budget::Lanes;
use cryarchiver_engine::calibration::{Tables, parse, tables};
use cryarchiver_engine::detect::{self, Detection, MediaClass};
use cryarchiver_engine::options::TrimWindow;
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe;
use cryarchiver_engine::quality::search::Target;
use cryarchiver_engine::toolchain::{ToolConfig, Toolchain};
use cryarchiver_engine::video::VideoStarts;
use cryarchiver_engine::video::analyze::{Analysis, analyze};
use cryarchiver_engine::video::encode::Sample;
use cryarchiver_engine::video::plan::{Encoder, Resolution, VideoCaps, VideoPlan, plan_video, usable_main};
use cryarchiver_engine::video::quality::{
	Aim, Found, Scorer, Searched, analysis_segments, search, search_samples, target,
};
use cryarchiver_engine::workspace::Workspace;
use tokio_util::sync::CancellationToken;

const SLOW: u8 = 0;
const MODERATE: u8 = 1;
const FAST: u8 = 2;
const SPEEDS: [(u8, Speed); 3] = [(FAST, Speed::Fast), (MODERATE, Speed::Moderate), (SLOW, Speed::Slow)];
const PRESET_4: u8 = 4;
/// Clear of the searches' probe rounds, so its sample files never collide with theirs.
const GRAIN_FREE_ROUND: usize = 90;
const REPORT_KEYS: [&str; 2] = ["videos-measured", "moderate-preset4-bytes-ratio"];

#[derive(Debug, Default)]
pub struct Measurements {
	pub crf: BTreeMap<(u8, usize), Vec<f64>>,
	pub bpp: [Vec<f64>; 6],
	pub grain_mean: Vec<f64>,
	pub grain_floor: Vec<f64>,
	pub preset4_ratio: Vec<f64>,
	pub slope: Vec<f64>,
	pub videos: usize,
}

impl Measurements {
	fn absorb(&mut self, other: Self) {
		let Self {
			crf,
			bpp,
			grain_mean,
			grain_floor,
			preset4_ratio,
			slope,
			videos,
		} = other;

		for (cell, values) in crf {
			self.crf.entry(cell).or_default().extend(values);
		}
		for (all, values) in self.bpp.iter_mut().zip(bpp) {
			all.extend(values);
		}

		self.grain_mean.extend(grain_mean);
		self.grain_floor.extend(grain_floor);
		self.preset4_ratio.extend(preset4_ratio);
		self.slope.extend(slope);
		self.videos += videos;
	}
}

pub fn median(values: &[f64]) -> Option<f64> {
	if values.is_empty() {
		return None;
	}

	let mut sorted = values.to_vec();
	sorted.sort_by(f64::total_cmp);
	let middle = sorted.len() / 2;
	Some(if sorted.len().is_multiple_of(2) {
		f64::midpoint(sorted[middle - 1], sorted[middle])
	} else {
		sorted[middle]
	})
}

#[allow(clippy::cast_precision_loss)]
pub fn updated(mut tables: Tables, measured: &Measurements) -> Tables {
	let rows = [
		(SLOW, &mut tables.video.svt_crf_slow),
		(MODERATE, &mut tables.video.svt_crf_moderate),
		(FAST, &mut tables.video.svt_crf_fast),
	];
	for (speed, row) in rows {
		for (index, cell) in row.iter_mut().enumerate() {
			if let Some(crf) = measured.crf.get(&(speed, index)).and_then(|values| median(values)) {
				*cell = (crf * 4.0).floor() / 4.0;
			}
		}
	}

	for (cell, values) in tables.video.expected_bpp.iter_mut().zip(&measured.bpp) {
		if let Some(bpp) = median(values) {
			*cell = thousandths(bpp);
		}
	}

	if let Some(mean) = median(&measured.grain_mean) {
		tables.video.grain_allowance_mean = mean.max(0.0);
	}

	if let Some(floor) = median(&measured.grain_floor) {
		tables.video.grain_allowance_floor = floor.max(0.0);
	}

	if let Some(slope) = median(&measured.slope) {
		tables.video.default_slope = thousandths(slope);
	}

	tables.report.insert(REPORT_KEYS[0].into(), measured.videos as f64);
	if let Some(ratio) = median(&measured.preset4_ratio) {
		tables.report.insert(REPORT_KEYS[1].into(), ratio);
	}

	tables
}

/// Rounded to a thousandth, and never below one: the tables refuse a zero.
fn thousandths(value: f64) -> f64 {
	((value * 1000.0).round() / 1000.0).max(0.001)
}

pub fn check_out(out: &Path) -> Result<(), String> {
	if fs::symlink_metadata(out).is_ok() {
		return Err(format!(
			"{} already exists; calibration never replaces a file",
			out.display()
		));
	}

	let folder = match out.parent() {
		Some(parent) if !parent.as_os_str().is_empty() => parent,
		_ => Path::new("."),
	};
	if !folder.is_dir() {
		return Err(format!("{} is not a folder", folder.display()));
	}

	tempfile::tempfile_in(folder)
		.map(drop)
		.map_err(|err| format!("{} can't be written: {err}", folder.display()))
}

pub async fn run(corpus: &Path, out: &Path) -> Result<(), String> {
	let cancel = CancellationToken::new();
	let stop = cancel.clone();
	tokio::spawn(async move {
		if tokio::signal::ctrl_c().await.is_ok() {
			stop.cancel();
		}
	});

	let tools = Toolchain::detect(&ToolConfig::default(), &cancel)
		.await
		.map_err(|err| err.to_string())?;
	if !tools.has_vmaf() {
		return Err("calibration needs ffmpeg with libvmaf".into());
	}

	let scratch = tempfile::tempdir().map_err(|err| err.to_string())?;
	let mut measured = Measurements::default();
	for file in corpus_files(corpus)? {
		match Box::pin(measure_video(&tools, &file, scratch.path(), &mut measured, &cancel)).await {
			Ok(true) => println!("✓ {}", file.display()),
			Ok(false) => {}
			Err(reason) => println!("· {}: {reason}", file.display()),
		}

		if cancel.is_cancelled() {
			return Err("stopped; nothing was written".into());
		}
	}

	println!("videos measured: {}", measured.videos);
	if measured.videos == 0 {
		return Err("no video in the corpus could be measured; nothing was written".into());
	}

	write_tables(out, &updated(tables().clone(), &measured))
}

fn write_tables(out: &Path, tables: &Tables) -> Result<(), String> {
	let text = toml::to_string_pretty(tables).map_err(|err| err.to_string())?;

	let written = parse(&text).and_then(|_| {
		fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(out)
			.and_then(|mut file| file.write_all(text.as_bytes()))
			.map_err(|err| format!("{}: {err}", out.display()))
	});
	if let Err(err) = written {
		println!("{text}");
		return Err(format!("{err}; the measured tables are printed above"));
	}

	Ok(())
}

fn corpus_files(root: &Path) -> Result<Vec<PathBuf>, String> {
	let mut files = Vec::new();
	let mut folders = vec![root.to_path_buf()];
	while let Some(folder) = folders.pop() {
		let entries = fs::read_dir(&folder).map_err(|err| format!("{}: {err}", folder.display()))?;
		for entry in entries.flatten() {
			match entry.file_type() {
				Ok(kind) if kind.is_dir() => folders.push(entry.path()),
				Ok(kind) if kind.is_file() => files.push(entry.path()),
				_ => {}
			}
		}
	}
	files.sort();
	Ok(files)
}

async fn measure_video(
	tools: &Toolchain,
	source: &Path,
	scratch: &Path,
	measured: &mut Measurements,
	cancel: &CancellationToken,
) -> Result<bool, String> {
	let Detection { class, .. } = detect::detect(tools, source, cancel)
		.await
		.map_err(|err| err.to_string())?;
	let MediaClass::Video(_) = class else {
		return Ok(false);
	};

	let bytes = fs::metadata(source).map_err(|err| err.to_string())?.len();
	let (info, _) = probe::probe_video(tools, source, bytes, true, cancel)
		.await
		.map_err(|err| err.to_string())?;
	let caps = VideoCaps {
		vp9_alpha: tools.vp9_alpha,
	};
	let main = usable_main(&info, &caps).map_err(|reason| format!("{reason:?}"))?;
	let declared = main.declared_duration().or(info.format.duration);
	let span = declared.map(|length_s| TrimWindow { start_s: 0.0, length_s });
	let workspace = Workspace::create(scratch, source).map_err(|err| err.to_string())?;
	let threads = std::thread::available_parallelism().map_or(1, usize::from);
	let mut by_segments: BTreeMap<usize, Analysis> = BTreeMap::new();
	let mut video = Measurements::default();

	let mut any = false;
	let mut unmeasured = "no search samples: the duration is unknown";
	for (row, speed) in SPEEDS {
		let sampling_speed = if speed == Speed::Fast { Speed::Moderate } else { speed };
		let segments = analysis_segments(sampling_speed, declared);
		let analysis = match by_segments.entry(segments) {
			Entry::Occupied(known) => known.into_mut(),
			Entry::Vacant(slot) => slot.insert(
				Box::pin(analyze(tools, source, main, declared, segments, None, cancel))
					.await
					.map_err(|err| err.to_string())?,
			),
		};

		let mut plan = plan_video(&info, main, &[], analysis, speed, None);
		let Encoder::SvtAv1 { .. } = plan.encoder else {
			return Err("the planned encoder is not SVT-AV1".into());
		};
		plan.starts = VideoStarts::of(&info, &plan.streams);
		let samples = search_samples(analysis, span, sampling_speed);
		if samples.is_empty() {
			continue;
		}

		let goal = target(
			speed,
			plan.encoder.film_grain(),
			plan.colour.is_hdr(),
			&Overrides::default(),
		);
		let scorer = Scorer {
			tools,
			source,
			workspace: &workspace,
			plan: &plan,
			threads,
			lanes: Lanes::SERIAL,
			cancel,
		};
		let start = plan.encoder.crf();
		let default_slope = tables().video.default_slope;
		let Searched::Found(found) = search(&scorer, &samples, start, Aim::exact(goal), default_slope, |_| {})
			.await
			.map_err(|err| err.to_string())?
			.searched
		else {
			unmeasured = "the quality target was reached at no speed";
			continue;
		};
		any = true;
		let bucket = Resolution::of(plan.width, plan.height).index();
		video.crf.entry((row, bucket)).or_default().push(found.crf);
		if row == MODERATE {
			let fps = main.fps().unwrap_or(25.0);
			Box::pin(measure_at_moderate(scorer, &samples, found, (goal, fps), &mut video)).await?;
		}
	}

	if !any {
		return Err(unmeasured.into());
	}

	video.videos = 1;
	measured.absorb(video);
	Ok(true)
}

#[allow(clippy::cast_precision_loss)]
async fn measure_at_moderate(
	scorer: Scorer<'_>,
	samples: &[Sample],
	found: Found,
	(goal, fps): (Target, f64),
	measured: &mut Measurements,
) -> Result<(), String> {
	let plan = scorer.plan;
	let bucket = Resolution::of(plan.width, plan.height).index();

	measured.slope.push(found.slope);
	let seconds: f64 = samples.iter().map(|sample| sample.length_s).sum();
	let pixels = f64::from(plan.width) * f64::from(plan.height) * seconds * fps;
	if pixels > 0.0 {
		measured.bpp[bucket].push(found.sample_bytes as f64 * 8.0 / pixels);
	}

	if plan.encoder.film_grain() {
		let grain_free = VideoPlan {
			encoder: Encoder::SvtAv1 {
				preset: plan.encoder.preset(),
				crf: found.crf,
				film_grain: None,
			},
			..plan.clone()
		};
		let without = Scorer {
			plan: &grain_free,
			..scorer
		}
		.probe(found.crf, samples, GRAIN_FREE_ROUND, |_| {})
		.await
		.map_err(|err| err.to_string())?;
		measured.grain_mean.push(without.score.mean - found.score.mean);
		measured.grain_floor.push(without.score.p5 - found.score.p5);
	}

	let slower = VideoPlan {
		encoder: Encoder::SvtAv1 {
			preset: PRESET_4,
			crf: found.crf,
			film_grain: plan.encoder.grain_level(),
		},
		..plan.clone()
	};
	let slower_scorer = Scorer {
		plan: &slower,
		..scorer
	};
	if let Searched::Found(at_preset_4) = search(
		&slower_scorer,
		samples,
		found.crf,
		Aim::exact(goal),
		found.slope,
		|_| {},
	)
	.await
	.map_err(|err| err.to_string())?
	.searched
		&& found.sample_bytes > 0
	{
		measured
			.preset4_ratio
			.push(at_preset_4.sample_bytes as f64 / found.sample_bytes as f64);
	}

	Ok(())
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
	use super::*;

	#[test]
	fn medians_of_odd_and_even_counts() {
		assert_eq!(median(&[]), None);
		assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
		assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
	}

	#[test]
	fn only_measured_cells_change_and_crfs_round_to_quarter_steps() {
		let mut measured = Measurements::default();
		measured
			.crf
			.entry((MODERATE, 2))
			.or_default()
			.extend([31.1, 31.6, 30.2]);
		measured.bpp[2].extend([0.051, 0.049]);
		measured.grain_mean.push(1.4);
		measured.grain_floor.push(2.1);
		measured.preset4_ratio.push(0.96);
		measured.videos = 3;
		let before = tables().clone();

		let after = updated(before.clone(), &measured);

		assert_eq!(after.video.svt_crf_moderate[2], 31.0);
		assert_eq!(after.video.svt_crf_moderate[0], before.video.svt_crf_moderate[0]);
		assert_eq!(after.video.svt_crf_slow, before.video.svt_crf_slow);
		assert_eq!(after.video.expected_bpp[2], 0.05);
		assert_eq!(after.video.grain_allowance_mean, 1.4);
		assert_eq!(after.video.grain_allowance_floor, 2.1);
		assert_eq!(after.report.get("videos-measured"), Some(&3.0));
		assert_eq!(after.report.get("moderate-preset4-bytes-ratio"), Some(&0.96));
		assert_eq!(parse(&toml::to_string_pretty(&after).unwrap()).as_ref(), Ok(&after));
	}

	#[test]
	fn a_video_adds_all_its_measurements_to_the_run() {
		let mut run = Measurements::default();
		run.crf.entry((MODERATE, 1)).or_default().push(30.0);
		run.bpp[1].push(0.04);
		run.videos = 1;
		let mut video = Measurements::default();
		video.crf.entry((MODERATE, 1)).or_default().push(32.0);
		video.crf.entry((SLOW, 3)).or_default().push(28.0);
		video.bpp[1].push(0.06);
		video.slope.push(0.5);
		video.grain_mean.push(1.0);
		video.grain_floor.push(2.0);
		video.preset4_ratio.push(0.9);
		video.videos = 1;

		run.absorb(video);

		assert_eq!(run.crf[&(MODERATE, 1)], [30.0, 32.0]);
		assert_eq!(run.crf[&(SLOW, 3)], [28.0]);
		assert_eq!(run.bpp[1], [0.04, 0.06]);
		assert_eq!(
			(
				run.slope,
				run.grain_mean,
				run.grain_floor,
				run.preset4_ratio,
				run.videos
			),
			(vec![0.5], vec![1.0], vec![2.0], vec![0.9], 2)
		);
	}

	#[test]
	fn the_report_holds_numbers_under_fixed_keys_only() {
		let mut measured = Measurements::default();
		measured.preset4_ratio.push(0.9);
		measured.slope.extend([0.4, 0.6]);
		measured.videos = 2;

		let after = updated(tables().clone(), &measured);

		assert!(
			after.report.keys().all(|key| REPORT_KEYS.contains(&key.as_str())),
			"{:?}",
			after.report
		);
		assert_eq!(after.video.default_slope, 0.5);
	}

	#[test]
	fn tiny_medians_never_round_to_a_value_the_tables_refuse() {
		let mut measured = Measurements::default();
		measured.bpp[5].push(0.0004);
		measured.slope.push(0.0002);

		let after = updated(tables().clone(), &measured);

		assert_eq!(after.video.expected_bpp[5], 0.001);
		assert_eq!(after.video.default_slope, 0.001);
		assert!(parse(&toml::to_string_pretty(&after).unwrap()).is_ok());
	}

	#[test]
	fn an_output_that_exists_or_has_no_folder_is_refused_up_front() {
		let folder = tempfile::tempdir().unwrap();
		let existing = folder.path().join("tables.toml");
		fs::write(&existing, "kept").unwrap();

		assert!(check_out(&folder.path().join("new.toml")).is_ok());
		assert!(check_out(&existing).unwrap_err().contains("already exists"));
		assert!(check_out(folder.path()).unwrap_err().contains("already exists"));
		assert!(
			check_out(&folder.path().join("missing").join("tables.toml"))
				.unwrap_err()
				.contains("is not a folder")
		);
		assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
	}

	#[test]
	fn tables_are_written_to_a_new_file_and_never_over_an_existing_one() {
		let folder = tempfile::tempdir().unwrap();
		let out = folder.path().join("tables.toml");
		let existing = folder.path().join("taken.toml");
		fs::write(&existing, "kept").unwrap();

		write_tables(&out, tables()).unwrap();

		assert_eq!(parse(&fs::read_to_string(&out).unwrap()).as_ref(), Ok(tables()));
		assert!(write_tables(&existing, tables()).unwrap_err().contains("printed above"));
		assert_eq!(fs::read_to_string(&existing).unwrap(), "kept");
	}

	#[test]
	fn every_file_of_every_folder_is_listed() {
		let corpus = tempfile::tempdir().unwrap();
		let deeper = corpus.path().join("nested").join("deeper");
		fs::create_dir_all(&deeper).unwrap();
		for file in [
			corpus.path().join("notes.mp4"),
			corpus.path().join("_draft.mkv"),
			deeper.join("late-start.mkv"),
		] {
			fs::write(file, b"x").unwrap();
		}

		let files = corpus_files(corpus.path()).unwrap();

		assert_eq!(
			files,
			[
				corpus.path().join("_draft.mkv"),
				deeper.join("late-start.mkv"),
				corpus.path().join("notes.mp4"),
			]
		);
	}
}
