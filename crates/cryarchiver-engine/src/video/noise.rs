//! Sensor noise and film grain, measured on raw luma from runs of three consecutive frames. Each frame is high-passed
//! with a 5×5 box, so that slow changes (fades, flicker, animated shading) drop out, and each frame's own noise comes
//! from the energies of the three pairwise differences (the "three-cornered hat"); a block takes the median of the
//! three, so that a smoother B-frame or one re-quantised frame can't set it. Only flat blocks count, so line art and
//! texture can't pass for noise, and moving ones are skipped. Grain depends on brightness, so blocks are pooled per
//! luma band.
//!
//! The same frames give the segment's brightness and a direct look at its fields: in a moving block of the middle
//! frame, each field of interlaced video matches the other field of one neighbouring frame (half a frame away) much
//! better than that of the other (one and a half frames away), while the two fields of a progressive frame are one
//! instant and match both neighbours alike.

use std::path::Path;

use crate::images::decode::deep_pix_fmt;
use crate::probe::Stream;
use crate::process::{self, STALL_TIMEOUT, ToolCommand};
use crate::video::analyze::{Segment, forced_decoder};

pub const TRIPLETS: usize = 3;

const BAND_COUNT: usize = 4;
/// Band edges on the 8-bit scale. Blocks darker or brighter than the outer edges may be clipped, which hides noise.
const BANDS: [f64; BAND_COUNT + 1] = [20.0, 50.0, 90.0, 140.0, 230.0];
const BLOCK: usize = 16;
const PIXELS: f64 = 256.0;
/// The 5×5 box needs 2 pixels around the block, and the gradient of the boxed mean one more.
const APRON: usize = 3;
const WINDOW: usize = BLOCK + 2 * APRON;
const BOX: usize = 5;
const BOX_AREA: f64 = 25.0;
/// The share of white noise's variance that the 5×5 box high-pass keeps.
const HIGH_PASS_GAIN: f64 = 1.0 - 1.0 / BOX_AREA;
const MAX_BLOCKS: usize = 8_192;
const FLAT_GRADIENT: f64 = 1.0;
/// Noise raises the boxed mean's gradient by about 0.06σ, so very heavy noise would otherwise make every block
/// look textured.
const FLAT_PER_SIGMA: f64 = 0.1;
const MIN_BLOCKS: usize = 30;
/// For noise, however it is correlated in space, the change across two frames carries no more energy than the two
/// single-frame changes together; for motion it carries up to twice as much. This holds for the high-passed frames and,
/// with the block's mean change removed so that fades pass, for their 5×5 box means, where a smooth area's motion
/// shows.
const LAG_RATIO: f64 = 1.2;
const LAG_SLACK: f64 = 0.1;
const LOW_FREQUENCY_SLACK: f64 = 0.02;
/// How far a block's mean change may stray from the triplet's median one. Fades and flicker change every block alike;
/// motion over a gradient changes each by its own gradient.
const DC_SLACK: f64 = 0.25;
/// A normal deviation over the median absolute deviation.
const MAD_TO_DEVIATION: f64 = 1.4826;
/// How much a second difference of independent values widens their deviation: √6.
const SECOND_DIFFERENCE_GAIN: f64 = 2.449_489_742_783_178;
/// A triplet whose flat blocks mostly move is left out: compression flicker rides on motion, so the blocks of a
/// moving picture that happen to look still can't be trusted.
const MOVING_SHARE: f64 = 0.5;
const CUT_CHANGE: f64 = 8.0;
const CUT_SHARE: f64 = 0.3;
const DEFAULT_PERIOD: u32 = 25;
/// Larger pictures are measured on their centre, so a segment never holds more than three frames of this size.
const MAX_WIDTH: u32 = 3840;
const MAX_HEIGHT: u32 = 2160;
const PGM_HEADER_BYTES: usize = 32;
const LUMA_STEP: usize = 4;
const FIELD_BLOCK: usize = 16;
/// A block moves when its fields' best match with a neighbouring frame is this much worse than with their own frame.
const MOVING_OVER_STILL: f64 = 1.5;
const MIN_MOTION: f64 = 1.0;
/// Interlaced fields match one neighbour about three times better than the other; progressive ones match both alike.
const FIELD_ASYMMETRY: f64 = 1.5;
const PROGRESSIVE_SYMMETRY: f64 = 1.2;
/// The fields of an interlaced frame are half a frame apart, like its better neighbour; progressive fields are one
/// instant, much closer than either neighbour.
const OWN_FIELDS_SHARE: f64 = 0.6;
const MIN_FIELD_GAP: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Triplet {
	/// The median noise deviation of each luma band's usable blocks, in 8-bit code values.
	pub bands: [Option<f64>; BAND_COUNT],
}

/// The moving blocks of the frames between two others, by what their fields show.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Combing {
	pub top_first: u64,
	pub bottom_first: u64,
	pub progressive: u64,
}

impl Combing {
	fn add(&mut self, other: Self) {
		self.top_first += other.top_first;
		self.bottom_first += other.bottom_first;
		self.progressive += other.progressive;
	}
}

/// What a segment's raw frames showed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measured {
	pub triplets: Vec<Triplet>,
	/// The mean luma of the frames, on the 8-bit scale.
	pub luma: Option<f64>,
	pub combing: Combing,
}

#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
	pub width: usize,
	pub height: usize,
	max: f32,
	wide: bool,
	data: &'a [u8],
}

impl<'a> Frame<'a> {
	pub fn gray16(width: usize, height: usize, data: &'a [u8]) -> Option<Self> {
		(data.len() == width * height * 2).then_some(Self {
			width,
			height,
			max: 65_535.0,
			wide: true,
			data,
		})
	}

	/// The luma at `(x, y)` on the 8-bit scale.
	pub fn sample(&self, x: usize, y: usize) -> f32 {
		let index = y * self.width + x;
		let value = if self.wide {
			u16::from_be_bytes([self.data[2 * index], self.data[2 * index + 1]])
		} else {
			u16::from(self.data[index])
		};

		f32::from(value) * 255.0 / self.max
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
	width: usize,
	height: usize,
	max: u16,
	/// Where the pixels start.
	offset: usize,
	size: usize,
}

struct Held {
	header: Header,
	bytes: Vec<u8>,
}

impl Held {
	fn frame(&self) -> Frame<'_> {
		Frame {
			width: self.header.width,
			height: self.header.height,
			max: f32::from(self.header.max),
			wide: self.header.max > 255,
			data: &self.bytes[self.header.offset..self.header.offset + self.header.size],
		}
	}
}

/// Measures PGM frames as they stream in, three at a time, holding no more than the frames of the triplet being read.
#[derive(Default)]
pub struct Reader {
	pending: Vec<u8>,
	held: Vec<Held>,
	triplets: Vec<Triplet>,
	luma: Vec<f64>,
	combing: Combing,
	peak_bytes: usize,
	/// The stream stopped being PGM; nothing after it is read.
	broken: bool,
}

impl Reader {
	/// A frame's bytes go into a buffer reserved to its exact size and never past its end, which would double it.
	pub fn push(&mut self, mut chunk: &[u8]) {
		while !chunk.is_empty() && !self.broken {
			let header = header(&self.pending);
			if header.is_none() && self.pending.len() >= PGM_HEADER_BYTES {
				self.broken = true;
				self.pending = Vec::new();
				return;
			}

			let wanted = header.map_or(1, |header| header.offset + header.size - self.pending.len());
			self.pending.reserve_exact(wanted);

			let (taken, rest) = chunk.split_at(wanted.min(chunk.len()));
			self.pending.extend_from_slice(taken);
			chunk = rest;
			self.note_peak();

			if let Some(header) = header
				&& self.pending.len() == header.offset + header.size
			{
				let bytes = std::mem::take(&mut self.pending);
				self.held.push(Held { header, bytes });
				self.note_peak();
			}

			if let [a, b, c] = self.held.as_slice() {
				let (triplet, luma, combing) = off_the_worker(|| {
					let [a, b, c] = [a.frame(), b.frame(), c.frame()];
					(
						triplet(&a, &b, &c),
						[a, b, c].map(|frame| mean_luma(&frame)),
						combing(&a, &b, &c),
					)
				});
				self.held.clear();

				self.triplets.extend(triplet);
				self.luma.extend(luma.into_iter().flatten());
				self.combing.add(combing);
			}
		}
	}

	/// The most bytes held at once, frames and the one being read together.
	pub fn peak_bytes(&self) -> usize {
		self.peak_bytes
	}

	#[allow(clippy::cast_precision_loss)]
	pub fn finish(self) -> Measured {
		Measured {
			luma: (!self.luma.is_empty()).then(|| self.luma.iter().sum::<f64>() / self.luma.len() as f64),
			triplets: self.triplets,
			combing: self.combing,
		}
	}

	fn note_peak(&mut self) {
		let held = self.pending.capacity() + self.held.iter().map(|held| held.bytes.capacity()).sum::<usize>();
		self.peak_bytes = self.peak_bytes.max(held);
	}
}

/// Decodes `TRIPLETS` runs of three consecutive frames, about a second apart, as PGM luma on stdout: 8-bit for 8-bit
/// sources, 16-bit for deeper ones.
pub fn command(ffmpeg: &Path, source: &Path, main: &Stream, segment: &Segment) -> ToolCommand {
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	let period = main
		.fps()
		.map_or(DEFAULT_PERIOD, |fps| fps.round().clamp(3.0, 1_000.0) as u32);
	let (format, depth) = if main.pix_fmt.as_deref().is_some_and(deep_pix_fmt) {
		("gray16be", 2)
	} else {
		("gray", 1)
	};
	let width = main.width.map_or(MAX_WIDTH, |width| width.min(MAX_WIDTH)) as usize;
	let height = main.height.map_or(MAX_HEIGHT, |height| height.min(MAX_HEIGHT)) as usize;

	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.capture_limit = Some(3 * TRIPLETS * ((width + BLOCK) * (height + BLOCK) * depth + PGM_HEADER_BYTES));
	cmd.args(["-hide_banner", "-nostdin", "-noautorotate"]);
	if let Some(length) = segment.length_s {
		cmd.seek(segment.start_s, length);
	}

	if let Some(decoder) = forced_decoder(main) {
		cmd.arg("-c:v").arg(decoder);
	}

	cmd.arg("-i")
		.arg(process::tool_path(source))
		.arg("-map")
		.arg(format!("0:{}", main.index))
		.arg("-vf")
		.arg(format!(
			"select='lt(mod(n,{period}),3)',extractplanes=y,\
			 crop='min(iw,{MAX_WIDTH})':'min(ih,{MAX_HEIGHT})',format={format}"
		))
		.arg("-frames:v")
		.arg((3 * TRIPLETS).to_string())
		.args(["-fps_mode", "passthrough", "-c:v", "pgm", "-f", "image2pipe", "-"]);
	cmd
}

fn header(bytes: &[u8]) -> Option<Header> {
	let rest = bytes.strip_prefix(b"P5")?;
	let (width, rest) = header_number(rest)?;
	let (height, rest) = header_number(rest)?;
	let (max, rest) = header_number(rest)?;
	rest.first().filter(|byte| byte.is_ascii_whitespace())?;

	let max = u16::try_from(max).ok().filter(|max| *max > 0)?;
	let size = width.checked_mul(height)?.checked_mul(if max > 255 { 2 } else { 1 })?;
	Some(Header {
		width,
		height,
		max,
		offset: bytes.len() - rest.len() + 1,
		size,
	})
}

/// A header number, once a whitespace byte after it shows it is complete.
fn header_number(bytes: &[u8]) -> Option<(usize, &[u8])> {
	let start = bytes.iter().position(|byte| !byte.is_ascii_whitespace())?;
	let digits = bytes[start..].iter().take_while(|byte| byte.is_ascii_digit()).count();
	let rest = &bytes[start + digits..];
	rest.first()?;

	let number = std::str::from_utf8(&bytes[start..start + digits]).ok()?.parse().ok()?;
	Some((number, rest))
}

/// The noise of three consecutive frames, or `None` for frames that don't match, span a scene cut or mostly move.
#[allow(clippy::cast_precision_loss)]
pub fn triplet(a: &Frame<'_>, b: &Frame<'_>, c: &Frame<'_>) -> Option<Triplet> {
	let (width, height) = (a.width, a.height);
	if [b, c]
		.iter()
		.any(|frame| (frame.width, frame.height) != (width, height))
		|| width < WINDOW
		|| height < WINDOW
	{
		return None;
	}

	let (across, down) = ((width - 2 * APRON) / BLOCK, (height - 2 * APRON) / BLOCK);
	let stride = stride(across * down);
	let blocks: Vec<Block> = (0..down)
		.step_by(stride)
		.flat_map(|row| (0..across).step_by(stride).map(move |column| (column, row)))
		.map(|(column, row)| block([a, b, c], APRON + column * BLOCK, APRON + row * BLOCK))
		.collect();

	let cuts = blocks.iter().filter(|block| block.change > CUT_CHANGE).count();
	if blocks.is_empty() || cuts as f64 > CUT_SHARE * blocks.len() as f64 {
		return None;
	}

	let candidate = |block: &Block| block.flat && (BANDS[0]..BANDS[BAND_COUNT]).contains(&block.mean);
	let candidates: Vec<&Block> = blocks.iter().filter(|block| candidate(block)).collect();
	let typical_dc = median(&mut candidates.iter().map(|block| block.dc).collect::<Vec<_>>());
	let grain_tolerance = DC_SLACK + 3.0 * unshared_dc_spread(&blocks, across.div_ceil(stride), candidate);
	let usable: Vec<&Block> = candidates
		.iter()
		.copied()
		.filter(|block| block.still && (block.dc - typical_dc).abs() <= block.dc_tolerance.max(grain_tolerance))
		.collect();
	if (candidates.len() - usable.len()) as f64 > MOVING_SHARE * candidates.len() as f64 {
		return None;
	}

	let bands = std::array::from_fn(|band| {
		let mut sigmas: Vec<f64> = usable
			.iter()
			.filter(|block| (BANDS[band]..BANDS[band + 1]).contains(&block.mean))
			.map(|block| block.sigma)
			.collect();
		(sigmas.len() >= MIN_BLOCKS).then(|| median(&mut sigmas))
	});
	Some(Triplet { bands })
}

/// Every PGM triplet in `bytes`, measured.
pub fn measure(bytes: &[u8]) -> Measured {
	let mut reader = Reader::default();
	reader.push(bytes);
	reader.finish()
}

/// Pixel work done inside an async task: on a multi-threaded runtime, its worker hands its other tasks on first.
fn off_the_worker<T>(work: impl FnOnce() -> T) -> T {
	match tokio::runtime::Handle::try_current() {
		Ok(runtime) if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
			tokio::task::block_in_place(work)
		}
		_ => work(),
	}
}

#[allow(clippy::cast_precision_loss)]
fn mean_luma(frame: &Frame<'_>) -> Option<f64> {
	let mut sum = 0.0;
	let mut count = 0_usize;
	for y in (0..frame.height).step_by(LUMA_STEP) {
		for x in (0..frame.width).step_by(LUMA_STEP) {
			sum += f64::from(frame.sample(x, y));
			count += 1;
		}
	}

	(count > 0).then(|| sum / count as f64)
}

/// The field votes of the moving blocks of `b`, the frame between `a` and `c`.
pub fn combing(a: &Frame<'_>, b: &Frame<'_>, c: &Frame<'_>) -> Combing {
	let (width, height) = (b.width, b.height);
	let mut combing = Combing::default();
	if [a, c]
		.iter()
		.any(|frame| (frame.width, frame.height) != (width, height))
		|| width < FIELD_BLOCK
		|| height < FIELD_BLOCK + 2
	{
		return combing;
	}

	let (across, down) = (width / FIELD_BLOCK, (height - 2) / FIELD_BLOCK);
	let stride = stride(across * down);
	for row in (0..down).step_by(stride) {
		for column in (0..across).step_by(stride) {
			match field_vote([a, b, c], column * FIELD_BLOCK, 1 + row * FIELD_BLOCK) {
				Some(FieldVote::TopFirst) => combing.top_first += 1,
				Some(FieldVote::BottomFirst) => combing.bottom_first += 1,
				Some(FieldVote::Progressive) => combing.progressive += 1,
				None => {}
			}
		}
	}
	combing
}

enum FieldVote {
	TopFirst,
	BottomFirst,
	Progressive,
}

/// How each row of the block at `(x0, y0)` in the middle frame matches the rows of the other field around it, in its own
/// frame and in each neighbour. Row 0 belongs to the top field. Top first, a top row is half a frame after the bottom
/// rows of the frame before it and a bottom row half a frame before the top rows of the frame after it; bottom first,
/// the other way round.
#[allow(clippy::cast_precision_loss)]
fn field_vote(frames: [&Frame<'_>; 3], x0: usize, y0: usize) -> Option<FieldVote> {
	let [a, b, c] = frames;
	let (mut own, mut top_first, mut bottom_first) = (0.0, 0.0, 0.0);

	for y in y0..y0 + FIELD_BLOCK {
		for x in x0..x0 + FIELD_BLOCK {
			let here = f64::from(b.sample(x, y));
			let across =
				|frame: &Frame<'_>| (here - f64::from(frame.sample(x, y - 1) + frame.sample(x, y + 1)) / 2.0).abs();
			let (before, after) = (across(a), across(c));

			own += across(b);
			if y % 2 == 0 {
				top_first += before;
				bottom_first += after;
			} else {
				top_first += after;
				bottom_first += before;
			}
		}
	}

	let pixels = (FIELD_BLOCK * FIELD_BLOCK) as f64;
	let (closer, farther) = (top_first.min(bottom_first), top_first.max(bottom_first));
	if farther < MOVING_OVER_STILL * own + MIN_MOTION * pixels {
		return None;
	}

	if farther >= FIELD_ASYMMETRY * closer
		&& own >= OWN_FIELDS_SHARE * closer
		&& farther - closer >= MIN_FIELD_GAP * pixels
	{
		Some(if top_first < bottom_first {
			FieldVote::TopFirst
		} else {
			FieldVote::BottomFirst
		})
	} else if farther < PROGRESSIVE_SYMMETRY * closer {
		Some(FieldVote::Progressive)
	} else {
		None
	}
}

/// The noisiest luma band's median over the triplets, among the bands measured in at least a third of them (two at
/// least), so that one odd scene can't set the level. The darkest band, where dither and debanding sit, counts only
/// when no brighter one was measured.
pub fn level(triplets: &[Triplet]) -> Option<f64> {
	let needed = triplets.len().div_ceil(3).max(2);
	let levels: Vec<Option<f64>> = (0..BAND_COUNT)
		.map(|band| {
			let mut values: Vec<f64> = triplets.iter().filter_map(|triplet| triplet.bands[band]).collect();
			(values.len() >= needed).then(|| median(&mut values))
		})
		.collect();

	levels[1..].iter().flatten().copied().reduce(f64::max).or(levels[0])
}

struct Block {
	mean: f64,
	change: f64,
	/// The mean change from the first frame to the last.
	dc: f64,
	dc_tolerance: f64,
	sigma: f64,
	flat: bool,
	still: bool,
}

/// The deviation of the still blocks' mean changes that their neighbours in the row don't share, from the median
/// second difference along the row. Grain moves each block's mean on its own, the more the coarser it is, while motion
/// over a smooth gradient moves neighbouring blocks by amounts that change smoothly along it.
fn unshared_dc_spread(blocks: &[Block], columns: usize, candidate: impl Fn(&Block) -> bool) -> f64 {
	let mut curvatures: Vec<f64> = blocks
		.chunks(columns.max(1))
		.flat_map(|row| row.windows(3))
		.filter(|run| run.iter().all(|block| block.still && candidate(block)))
		.map(|run| (run[0].dc - 2.0 * run[1].dc + run[2].dc).abs())
		.collect();

	MAD_TO_DEVIATION * median(&mut curvatures) / SECOND_DIFFERENCE_GAIN
}

fn stride(blocks: usize) -> usize {
	let mut stride = 1;
	while blocks.div_ceil(stride * stride) > MAX_BLOCKS {
		stride += 1;
	}
	stride
}

fn block(frames: [&Frame<'_>; 3], x0: usize, y0: usize) -> Block {
	let mut windows = [[0.0; WINDOW * WINDOW]; 3];
	for wy in 0..WINDOW {
		for wx in 0..WINDOW {
			for (window, frame) in windows.iter_mut().zip(frames) {
				window[wy * WINDOW + wx] = f64::from(frame.sample(x0 + wx - APRON, y0 + wy - APRON));
			}
		}
	}

	let slow = windows.each_ref().map(boxed);
	let smooth = |index: usize| (slow[0][index] + slow[1][index] + slow[2][index]) / 3.0;
	let (mut pairs, mut slow_pairs, mut totals) = ([0.0; 3], [Energy::default(); 3], [0.0; 3]);
	let mut gradient = 0.0;
	let core = APRON..APRON + BLOCK;

	for wy in core.clone() {
		for wx in core.clone() {
			let index = wy * WINDOW + wx;
			let [a, b, c] = [0, 1, 2].map(|frame| windows[frame][index] - slow[frame][index]);
			pairs[0] += (a - b).powi(2);
			pairs[1] += (b - c).powi(2);
			pairs[2] += (a - c).powi(2);

			let [slow_a, slow_b, slow_c] = [0, 1, 2].map(|frame| slow[frame][index]);
			slow_pairs[0].add(slow_b - slow_a);
			slow_pairs[1].add(slow_c - slow_b);
			slow_pairs[2].add(slow_c - slow_a);
			for (total, window) in totals.iter_mut().zip(&windows) {
				*total += window[index];
			}

			gradient += (smooth(index + 1) - smooth(index - 1)).abs() / 2.0
				+ (smooth(index + WINDOW) - smooth(index - WINDOW)).abs() / 2.0;
		}
	}

	let [ab, bc, ac] = pairs;
	let [slow_first, slow_second, slow_across] = slow_pairs.map(Energy::without_mean);
	let outer_noise = ac / PIXELS / HIGH_PASS_GAIN;
	let mut hat = [(ab + ac - bc) / 2.0, (ab + bc - ac) / 2.0, (ac + bc - ab) / 2.0];
	let sigma = (median(&mut hat).max(0.0) / PIXELS / HIGH_PASS_GAIN).sqrt();

	Block {
		mean: totals.iter().sum::<f64>() / 3.0 / PIXELS,
		change: (totals[1] - totals[0]).abs().max((totals[2] - totals[1]).abs()) / PIXELS,
		dc: (totals[2] - totals[0]) / PIXELS,
		dc_tolerance: DC_SLACK + 3.0 * (outer_noise / PIXELS).sqrt(),
		sigma,
		flat: gradient / PIXELS < FLAT_GRADIENT.max(FLAT_PER_SIGMA * sigma),
		still: ac <= LAG_RATIO * (ab + bc) + LAG_SLACK * PIXELS
			&& slow_across <= LAG_RATIO * (slow_first + slow_second) + LOW_FREQUENCY_SLACK * PIXELS,
	}
}

/// The energy of a block's values about their mean.
#[derive(Debug, Clone, Copy, Default)]
struct Energy {
	sum: f64,
	squares: f64,
}

impl Energy {
	fn add(&mut self, value: f64) {
		self.sum += value;
		self.squares += value * value;
	}

	fn without_mean(self) -> f64 {
		self.squares - self.sum * self.sum / PIXELS
	}
}

/// The 5×5 mean around each pixel at least 2 pixels inside the window; 0 nearer its edge.
fn boxed(values: &[f64; WINDOW * WINDOW]) -> [f64; WINDOW * WINDOW] {
	const SIDE: usize = WINDOW + 1;
	let mut sums = [0.0; SIDE * SIDE];
	for y in 0..WINDOW {
		for x in 0..WINDOW {
			sums[(y + 1) * SIDE + x + 1] =
				values[y * WINDOW + x] + sums[y * SIDE + x + 1] + sums[(y + 1) * SIDE + x] - sums[y * SIDE + x];
		}
	}

	let half = BOX / 2;
	let mut boxed = [0.0; WINDOW * WINDOW];
	for y in half..WINDOW - half {
		for x in half..WINDOW - half {
			let (top, left, bottom, right) = (y - half, x - half, y + half + 1, x + half + 1);
			boxed[y * WINDOW + x] =
				(sums[bottom * SIDE + right] - sums[top * SIDE + right] - sums[bottom * SIDE + left]
					+ sums[top * SIDE + left])
					/ BOX_AREA;
		}
	}
	boxed
}

/// 0 for no values.
fn median(values: &mut [f64]) -> f64 {
	if values.is_empty() {
		return 0.0;
	}

	values.sort_by(f64::total_cmp);
	let middle = values.len() / 2;

	if values.len().is_multiple_of(2) {
		f64::midpoint(values[middle - 1], values[middle])
	} else {
		values[middle]
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Random(u64);

	impl Random {
		fn uniform(&mut self) -> f64 {
			self.0 ^= self.0 << 13;
			self.0 ^= self.0 >> 7;
			self.0 ^= self.0 << 17;
			#[allow(clippy::cast_precision_loss)]
			let unit = (self.0 >> 11) as f64 / (1_u64 << 53) as f64;
			unit
		}

		fn gaussian(&mut self) -> f64 {
			let (u, v) = (self.uniform().max(1e-12), self.uniform());
			(-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
		}
	}

	const W: usize = 320;
	const H: usize = 240;

	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	fn encode(width: usize, height: usize, mut pixel: impl FnMut(usize, usize) -> f64) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(width * height * 2);
		for y in 0..height {
			for x in 0..width {
				let value = (pixel(x, y) * 257.0).round().clamp(0.0, 65_535.0) as u16;
				bytes.extend_from_slice(&value.to_be_bytes());
			}
		}
		bytes
	}

	fn pgm16(width: usize, height: usize, pixels: &[u8]) -> Vec<u8> {
		let mut bytes = format!("P5\n{width} {height}\n65535\n").into_bytes();
		bytes.extend_from_slice(pixels);
		bytes
	}

	fn three(make: impl Fn(usize) -> Vec<u8>) -> [Vec<u8>; 3] {
		[make(0), make(1), make(2)]
	}

	fn measured(frames: &[Vec<u8>; 3]) -> Option<Triplet> {
		let [a, b, c] = frames.each_ref().map(|bytes| Frame::gray16(W, H, bytes).unwrap());
		triplet(&a, &b, &c)
	}

	fn strongest(triplet: Option<Triplet>) -> Option<f64> {
		triplet?.bands.into_iter().flatten().reduce(f64::max)
	}

	fn noisy_gray(sigmas: [f64; 3], seed: u64) -> [Vec<u8>; 3] {
		three(|index| {
			let mut random = Random(seed + index as u64 * 7919);
			encode(W, H, |_, _| 120.0 + sigmas[index] * random.gaussian())
		})
	}

	#[test]
	fn temporal_noise_on_a_flat_picture_is_measured_as_its_deviation() {
		for sigma in [0.5, 1.0, 2.0, 4.0, 20.0] {
			let level = strongest(measured(&noisy_gray([sigma; 3], 11))).unwrap();

			assert!((level - sigma).abs() < sigma * 0.15, "{sigma}: {level}");
		}
	}

	#[test]
	fn a_smoother_frame_in_the_run_does_not_hide_the_noise_of_the_others() {
		for sigmas in [[2.0, 0.5, 2.0], [0.5, 2.0, 2.0], [2.0, 2.0, 0.5]] {
			let level = strongest(measured(&noisy_gray(sigmas, 5))).unwrap();

			assert!((level - 2.0).abs() < 0.3, "{sigmas:?}: {level}");
		}
	}

	#[test]
	fn one_flickering_frame_is_not_noise() {
		let flicker = three(|index| {
			let mut random = Random(77);
			encode(W, H, |_, _| {
				let flicker = random.gaussian();
				if index == 1 { 120.0 + 1.5 * flicker } else { 120.0 }
			})
		});

		assert!(
			strongest(measured(&flicker)).is_none_or(|level| level < 0.1),
			"{:?}",
			measured(&flicker)
		);
	}

	#[test]
	fn a_clean_still_picture_has_no_noise() {
		let clean = three(|_| encode(W, H, |x, y| if (x / 40 + y / 40) % 2 == 0 { 60.0 } else { 180.0 }));

		assert!(strongest(measured(&clean)).is_none_or(|level| level < 0.05));
	}

	#[test]
	fn static_fine_texture_is_not_noise() {
		let texture = |x: usize, y: usize| {
			let mut random = Random((x * 7_331 + y * 104_729 + 1) as u64);
			100.0 + 30.0 * random.uniform()
		};
		let still = three(|_| encode(W, H, texture));

		assert!(strongest(measured(&still)).is_none_or(|level| level < 0.05));
	}

	#[test]
	fn a_pan_over_texture_or_a_fade_is_not_noise() {
		let wave =
			|x: f64, y: f64| 120.0 + 25.0 * (x / 9.0).sin() * (y / 13.0).cos() + 10.0 * (x / 3.1 + y / 4.3).sin();
		#[allow(clippy::cast_precision_loss)]
		let pan = three(|index| encode(W, H, |x, y| wave(x as f64 + 1.5 * index as f64, y as f64)));
		#[allow(clippy::cast_precision_loss)]
		let fade = three(|index| encode(W, H, |x, _| 60.0 + x as f64 * 0.3 + 6.0 * index as f64));

		assert!(
			strongest(measured(&pan)).is_none_or(|level| level < 0.1),
			"{:?}",
			measured(&pan)
		);
		assert!(
			strongest(measured(&fade)).is_some_and(|level| level < 0.05),
			"{:?}",
			measured(&fade)
		);
	}

	#[test]
	fn flicker_on_a_slow_pan_over_a_smooth_gradient_is_not_noise() {
		let curved = |x: f64, y: f64| 110.0 + 35.0 * (x / 50.0).sin() * (y / 70.0).cos();
		let pan = three(|index| {
			let mut flicker = Random(31 + index as u64);
			#[allow(clippy::cast_precision_loss)]
			encode(W, H, |x, y| {
				curved(x as f64 + index as f64, y as f64) + 0.7 * flicker.gaussian()
			})
		});
		let still = three(|index| {
			let mut noise = Random(31 + index as u64);
			#[allow(clippy::cast_precision_loss)]
			encode(W, H, |x, y| curved(x as f64, y as f64) + 0.7 * noise.gaussian())
		});

		assert!(
			strongest(measured(&pan)).is_none_or(|level| level < 0.1),
			"{:?}",
			measured(&pan)
		);
		assert!(
			strongest(measured(&still)).is_some_and(|level| (level - 0.7).abs() < 0.1),
			"{:?}",
			measured(&still)
		);
	}

	fn coarse(sigma: f64, grain_px: usize, seed: u64) -> impl FnMut(usize, usize) -> f64 {
		let (across, down) = (W / grain_px + 1, H / grain_px + 1);
		let mut random = Random(seed);
		let cells: Vec<f64> = (0..across * down).map(|_| random.gaussian()).collect();

		move |x, y| 120.0 + sigma * cells[(y / grain_px) * across + x / grain_px]
	}

	#[test]
	fn coarse_grain_on_a_still_picture_is_measured() {
		for grain_px in [2, 4, 6, 8] {
			let frames = three(|index| encode(W, H, coarse(4.0, grain_px, 41 + index as u64)));

			let level = strongest(measured(&frames));

			assert!(level.is_some_and(|level| level > 1.0), "{grain_px} px: {level:?}");
		}
	}

	fn pattern(x: f64, y: f64) -> f64 {
		120.0 + 40.0 * (x / 6.0).sin() * (y / 9.0).cos() + 15.0 * (x / 2.7 + y / 3.3).sin()
	}

	/// Three frames of a picture moving by `motion` pixels (x, y) a frame, each frame's rows taken at the times its
	/// fields show: `Some(true)` for top field first, `Some(false)` for bottom first, `None` for progressive.
	#[allow(clippy::cast_precision_loss)]
	fn filmed(fields: Option<bool>, motion: (f64, f64), noise: f64, seed: u64) -> [Vec<u8>; 3] {
		three(|index| {
			let mut random = Random(seed + index as u64);
			encode(W, H, |x, y| {
				let late = match fields {
					Some(top_first) => (y % 2 == 0) != top_first,
					None => false,
				};
				let time = index as f64 + if late { 0.5 } else { 0.0 };
				pattern(x as f64 - motion.0 * time, y as f64 - motion.1 * time) + noise * random.gaussian()
			})
		})
	}

	fn fields_of(frames: &[Vec<u8>; 3]) -> Combing {
		let [a, b, c] = frames.each_ref().map(|bytes| Frame::gray16(W, H, bytes).unwrap());
		combing(&a, &b, &c)
	}

	#[test]
	fn woven_fields_of_a_moving_picture_vote_for_their_field_order() {
		for motion in [(2.0, 0.0), (6.0, 0.0), (0.0, 2.0), (3.0, 3.0)] {
			let top = fields_of(&filmed(Some(true), motion, 1.0, 3));
			let bottom = fields_of(&filmed(Some(false), motion, 1.0, 3));

			assert!(
				top.top_first > 100 && top.top_first > 20 * (top.bottom_first + top.progressive),
				"{motion:?}: {top:?}"
			);
			assert!(
				bottom.bottom_first > 100 && bottom.bottom_first > 20 * (bottom.top_first + bottom.progressive),
				"{motion:?}: {bottom:?}"
			);
		}
	}

	#[test]
	fn progressive_motion_does_not_vote_for_fields() {
		for motion in [(2.0, 0.0), (6.0, 0.0), (0.0, 1.0), (0.0, 3.0), (3.0, 3.0), (0.5, 0.0)] {
			let votes = fields_of(&filmed(None, motion, 1.0, 5));

			assert!(
				votes.progressive > 20 && votes.top_first + votes.bottom_first <= votes.progressive / 50,
				"{motion:?}: {votes:?}"
			);
		}
	}

	#[test]
	fn still_noise_and_scene_cuts_cast_no_field_votes() {
		let noisy = fields_of(&noisy_gray([4.0; 3], 9));
		let [a, b, _] = filmed(None, (0.0, 0.0), 1.0, 7);
		let [_, _, c] = filmed(Some(true), (40.0, 0.0), 1.0, 8);
		let cut = fields_of(&[a, b, c]);

		assert_eq!(noisy.top_first + noisy.bottom_first, 0, "{noisy:?}");
		assert!(noisy.progressive < 5, "{noisy:?}");
		assert!(cut.top_first + cut.bottom_first <= cut.progressive / 50, "{cut:?}");
	}

	#[test]
	fn the_reader_keeps_the_brightness_and_the_field_votes_of_what_it_measured() {
		let flat = |level: f64| encode(W, H, |_, _| level);
		let frames: Vec<Vec<u8>> = [flat(40.0), flat(40.0), flat(40.0)]
			.into_iter()
			.chain(filmed(Some(true), (4.0, 0.0), 1.0, 11))
			.map(|pixels| pgm16(W, H, &pixels))
			.collect();

		let measured = measure(&frames.concat());

		assert!(
			measured
				.luma
				.is_some_and(|luma| (luma - f64::midpoint(40.0, 120.0)).abs() < 2.0),
			"{measured:?}"
		);
		assert!(measured.combing.top_first > 100, "{measured:?}");
		assert_eq!(Reader::default().finish().luma, None);
	}

	#[test]
	fn a_scene_cut_is_not_measured() {
		let cut = three(|index| encode(W, H, |_, _| if index < 2 { 60.0 } else { 160.0 }));

		assert_eq!(measured(&cut), None);
	}

	#[test]
	fn noise_is_measured_per_brightness_band() {
		let frames = three(|index| {
			let mut random = Random(99 + index as u64);
			encode(W, H, |x, _| {
				let grain = random.gaussian();
				if x < W / 2 { 30.0 } else { 120.0 + 2.0 * grain }
			})
		});

		let bands = measured(&frames).unwrap().bands;

		assert!(bands[0].is_some_and(|level| level < 0.05), "{bands:?}");
		assert_eq!(bands[1], None);
		assert!(bands[2].is_some_and(|level| (level - 2.0).abs() < 0.3), "{bands:?}");
		assert_eq!(bands[3], None);
	}

	#[test]
	fn the_level_is_the_noisiest_band_that_enough_triplets_measured() {
		let triplet = |bands: [Option<f64>; 4]| Triplet { bands };
		let triplets = [
			triplet([Some(0.6), Some(1.0), None, Some(9.0)]),
			triplet([Some(0.7), Some(1.2), None, None]),
			triplet([Some(0.6), Some(1.1), Some(3.0), None]),
			triplet([Some(0.8), None, None, None]),
			triplet([None, None, None, None]),
			triplet([Some(0.7), Some(0.9), None, None]),
		];

		assert_eq!(level(&triplets), Some(1.05));
		assert_eq!(level(&triplets[..1]), None);
		assert_eq!(level(&[]), None);
	}

	#[test]
	fn the_darkest_band_counts_only_when_no_brighter_one_was_measured() {
		let triplet = |bands: [Option<f64>; 4]| Triplet { bands };
		let dither = [triplet([Some(0.6), Some(0.1), Some(0.05), None]); 3];
		let dark_only = [triplet([Some(0.9), None, None, None]); 3];

		assert_eq!(level(&dither), Some(0.1));
		assert_eq!(level(&dark_only), Some(0.9));
	}

	#[test]
	fn frames_are_measured_as_they_stream_in_whatever_the_chunks() {
		let frames: Vec<Vec<u8>> = (0..3)
			.flat_map(|seed| noisy_gray([1.0; 3], 100 + seed))
			.map(|pixels| pgm16(W, H, &pixels))
			.collect();
		let mut stream = frames.concat();
		stream.extend_from_slice(b"P5\n320 240\n65535\n\x00\x01");
		let expected: Vec<Triplet> = frames
			.chunks(3)
			.map(|three| {
				let [a, b, c] = [0, 1, 2].map(|index| Frame::gray16(W, H, &three[index][17..]).unwrap());
				triplet(&a, &b, &c).unwrap()
			})
			.collect();

		for chunk in [1, 7, 4_096, 1 << 20] {
			let mut reader = Reader::default();
			for piece in stream.chunks(chunk) {
				reader.push(piece);
			}

			assert_eq!(reader.finish().triplets, expected, "chunks of {chunk}");
		}
		assert_eq!(measure(&stream).triplets, expected);
	}

	#[test]
	fn frames_are_measured_on_either_kind_of_runtime() {
		let stream: Vec<u8> = noisy_gray([1.0; 3], 21)
			.iter()
			.flat_map(|pixels| pgm16(W, H, pixels))
			.collect();
		let runtimes = [
			tokio::runtime::Builder::new_current_thread().build(),
			tokio::runtime::Builder::new_multi_thread().worker_threads(2).build(),
		];

		for runtime in runtimes {
			let stream = stream.clone();
			let measured = runtime
				.unwrap()
				.block_on(async { tokio::spawn(async move { measure(&stream) }).await.unwrap() });

			assert_eq!(measured.triplets.len(), 1, "{measured:?}");
		}
	}

	#[test]
	fn a_stream_that_is_not_pgm_is_dropped_at_once() {
		let mut reader = Reader::default();

		reader.push(&vec![b'x'; 100_000]);
		reader.push(&pgm16(W, H, &noisy_gray([1.0; 3], 1)[0]));

		assert!(reader.peak_bytes() <= 64, "{}", reader.peak_bytes());
		assert!(reader.finish().triplets.is_empty());
	}

	#[test]
	fn pgm_headers_are_read_only_once_complete() {
		let eight_bit = b"P5\n4 2\n255\n\x07";

		assert_eq!(
			header(eight_bit),
			Some(Header {
				width: 4,
				height: 2,
				max: 255,
				offset: 11,
				size: 8,
			})
		);
		assert_eq!(header(b"P5\n4 2\n65535\n").map(|header| header.size), Some(16));
		assert_eq!(header(b"P5\n4 2\n6553"), None);
		assert_eq!(header(b"P5\n4 2\n65535"), None);
		assert_eq!(header(b"P6\n4 2\n255\n"), None);

		let pixels = [7_u8; 8];
		let frame = Frame {
			width: 4,
			height: 2,
			max: 255.0,
			wide: false,
			data: &pixels,
		};
		assert!((frame.sample(3, 1) - 7.0).abs() < 1e-3);
	}

	#[test]
	fn an_8k_stream_holds_about_three_frames() {
		const WIDTH: usize = 7_680;
		const HEIGHT: usize = 4_320;
		let mut frame = format!("P5\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
		frame.resize(frame.len() + WIDTH * HEIGHT, 120);
		let mut reader = Reader::default();
		let (total, chunk) = (6 * frame.len(), 65_521);

		for start in (0..total).step_by(chunk) {
			let piece: Vec<u8> = (start..(start + chunk).min(total))
				.map(|position| frame[position % frame.len()])
				.collect();
			reader.push(&piece);
		}

		let peak = reader.peak_bytes();
		assert!(peak <= 3 * frame.len() + (1 << 17), "{peak} bytes held");
		assert_eq!(reader.finish().triplets.len(), 2);
	}

	#[test]
	fn the_command_decodes_frame_triplets_of_luma_at_the_source_depth() {
		let segment = Segment {
			start_s: 12.5,
			length_s: Some(3.0),
		};
		let stream = |pix_fmt: &str, width: u32, height: u32| -> Stream {
			serde_json::from_str(&format!(
				r#"{{"index":2,"codec_type":"video","codec_name":"vp9","width":{width},"height":{height},
				"pix_fmt":"{pix_fmt}","avg_frame_rate":"24000/1001","r_frame_rate":"24000/1001",
				"tags":{{"ALPHA_MODE":"1"}}}}"#
			))
			.unwrap()
		};

		let deep = command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			&stream("yuva420p10le", 7680, 4320),
			&segment,
		);
		let eight_bit = command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			&stream("yuva420p", 1920, 1080),
			&segment,
		);
		let args = crate::video::joined(&deep);

		assert!(args.contains("-ss 12.500 -t 3.000 -c:v libvpx-vp9 -i "), "{args}");
		assert!(
			args.contains(
				"-map 0:2 -vf select='lt(mod(n,24),3)',extractplanes=y,crop='min(iw,3840)':'min(ih,2160)',\
				 format=gray16be -frames:v 9 "
			),
			"{args}"
		);
		assert!(args.ends_with("-c:v pgm -f image2pipe -"), "{args}");
		assert!(crate::video::joined(&eight_bit).contains(",format=gray -frames:v 9 "));
		assert_eq!(deep.capture_limit, Some(9 * (3856 * 2176 * 2 + 32)));
		assert_eq!(eight_bit.capture_limit, Some(9 * (1936 * 1096 + 32)));
	}
}
