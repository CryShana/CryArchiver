//! Sensor noise and film grain, measured on raw luma. The measure is the residual of the middle frame of three
//! consecutive frames against the mean of the outer two, high-passed so that slow changes (fades, flicker, moving
//! gradients, animated shading) drop out. It is taken only on flat blocks, so line art and texture can't pass for
//! noise, and a block whose outer frames differ more than noise explains is moving and is skipped. Grain depends on
//! brightness, so blocks are pooled per luma band, and the noisiest band that enough triplets measured is the level.

use std::path::Path;

use crate::process::{self, STALL_TIMEOUT, ToolCommand};
use crate::video::analyze::Segment;

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
const MAX_BLOCKS: usize = 8_192;
const FLAT_GRADIENT: f64 = 1.0;
const MIN_BLOCKS: usize = 30;
/// For temporal white noise the outer frames' difference carries 4/3 of the residual's energy; motion carries several
/// times that.
const MOTION_RATIO: f64 = 2.0 * 4.0 / 3.0;
const STILL_SLACK: f64 = 0.1;
const CUT_CHANGE: f64 = 8.0;
const CUT_SHARE: f64 = 0.3;
/// The residual's energy per unit of noise variance: 1.5 for `b − (a + c) / 2`, less the share the box's mean takes.
const RESIDUAL_GAIN: f64 = 1.5 * (1.0 - 1.0 / BOX_AREA);
const DEFAULT_PERIOD: u32 = 25;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Triplet {
	/// The median noise deviation of each luma band's usable blocks, in 8-bit code values.
	pub bands: [Option<f64>; BAND_COUNT],
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

struct Block {
	mean: f64,
	change: f64,
	sigma: f64,
	usable: bool,
}

/// Decodes `TRIPLETS` runs of three consecutive frames, about a second apart, as 16-bit PGM luma on stdout.
pub fn command(
	ffmpeg: &Path,
	source: &Path,
	stream_index: u32,
	decoder: Option<&str>,
	segment: &Segment,
	fps: Option<f64>,
) -> ToolCommand {
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	let period = fps.map_or(DEFAULT_PERIOD, |fps| fps.round().clamp(3.0, 1_000.0) as u32);

	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-noautorotate"]);
	if let Some(length) = segment.length_s {
		cmd.seek(segment.start_s, length);
	}

	if let Some(decoder) = decoder {
		cmd.arg("-c:v").arg(decoder);
	}

	cmd.arg("-i")
		.arg(process::tool_path(source))
		.arg("-map")
		.arg(format!("0:{stream_index}"))
		.arg("-vf")
		.arg(format!(
			"select='lt(mod(n,{period}),3)',extractplanes=y,format=gray16be"
		))
		.arg("-frames:v")
		.arg((3 * TRIPLETS).to_string())
		.args(["-fps_mode", "passthrough", "-c:v", "pgm", "-f", "image2pipe", "-"]);
	cmd
}

pub fn frames(mut bytes: &[u8]) -> Vec<Frame<'_>> {
	let mut frames = Vec::new();
	while let Some((frame, rest)) = pgm(bytes) {
		frames.push(frame);
		bytes = rest;
	}
	frames
}

fn pgm(bytes: &[u8]) -> Option<(Frame<'_>, &[u8])> {
	let rest = bytes.strip_prefix(b"P5")?;
	let (width, rest) = header_number(rest)?;
	let (height, rest) = header_number(rest)?;
	let (max, rest) = header_number(rest)?;
	let rest = rest.get(1..)?;

	let max = u16::try_from(max).ok().filter(|max| *max > 0)?;
	let wide = max > 255;
	let size = width.checked_mul(height)?.checked_mul(if wide { 2 } else { 1 })?;
	if rest.len() < size {
		return None;
	}

	let (data, rest) = rest.split_at(size);
	let frame = Frame {
		width,
		height,
		max: f32::from(max),
		wide,
		data,
	};
	Some((frame, rest))
}

fn header_number(bytes: &[u8]) -> Option<(usize, &[u8])> {
	let start = bytes.iter().position(|byte| !byte.is_ascii_whitespace())?;
	let digits = bytes[start..].iter().take_while(|byte| byte.is_ascii_digit()).count();
	let number = std::str::from_utf8(&bytes[start..start + digits]).ok()?.parse().ok()?;

	Some((number, &bytes[start + digits..]))
}

/// The noise of the middle frame, or `None` for frames that don't match or span a scene cut.
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

	let bands = std::array::from_fn(|band| {
		let mut sigmas: Vec<f64> = blocks
			.iter()
			.filter(|block| block.usable && (BANDS[band]..BANDS[band + 1]).contains(&block.mean))
			.map(|block| block.sigma)
			.collect();
		(sigmas.len() >= MIN_BLOCKS).then(|| median(&mut sigmas))
	});
	Some(Triplet { bands })
}

/// Every 16-bit PGM triplet in `bytes`, measured.
pub fn measure(bytes: &[u8]) -> Vec<Triplet> {
	frames(bytes)
		.as_chunks::<3>()
		.0
		.iter()
		.filter_map(|[a, b, c]| triplet(a, b, c))
		.collect()
}

/// The noisiest luma band's median over the triplets, among the bands measured in at least a third of them (two at
/// least), so that one odd scene can't set the level.
pub fn level(triplets: &[Triplet]) -> Option<f64> {
	let needed = triplets.len().div_ceil(3).max(2);

	(0..BAND_COUNT)
		.filter_map(|band| {
			let mut values: Vec<f64> = triplets.iter().filter_map(|triplet| triplet.bands[band]).collect();
			(values.len() >= needed).then(|| median(&mut values))
		})
		.reduce(f64::max)
}

fn stride(blocks: usize) -> usize {
	let mut stride = 1;
	while blocks.div_ceil(stride * stride) > MAX_BLOCKS {
		stride += 1;
	}
	stride
}

fn block(frames: [&Frame<'_>; 3], x0: usize, y0: usize) -> Block {
	let mut mean = [0.0; WINDOW * WINDOW];
	let mut residual = [0.0; WINDOW * WINDOW];
	let mut outer = [0.0; WINDOW * WINDOW];
	let mut totals = [0.0; 3];
	let core = APRON..APRON + BLOCK;

	for wy in 0..WINDOW {
		for wx in 0..WINDOW {
			let [a, b, c] = frames.map(|frame| f64::from(frame.sample(x0 + wx - APRON, y0 + wy - APRON)));
			let index = wy * WINDOW + wx;
			mean[index] = (a + b + c) / 3.0;
			residual[index] = b - f64::midpoint(a, c);
			outer[index] = c - a;

			if core.contains(&wx) && core.contains(&wy) {
				totals[0] += a;
				totals[1] += b;
				totals[2] += c;
			}
		}
	}

	let (smooth, slow_residual, slow_outer) = (boxed(&mean), boxed(&residual), boxed(&outer));
	let (mut energy, mut outer_energy, mut gradient, mut level) = (0.0, 0.0, 0.0, 0.0);
	for wy in core.clone() {
		for wx in core.clone() {
			let index = wy * WINDOW + wx;
			energy += (residual[index] - slow_residual[index]).powi(2);
			outer_energy += (outer[index] - slow_outer[index]).powi(2);
			gradient += (smooth[index + 1] - smooth[index - 1]).abs() / 2.0
				+ (smooth[index + WINDOW] - smooth[index - WINDOW]).abs() / 2.0;
			level += mean[index];
		}
	}

	Block {
		mean: level / PIXELS,
		change: (totals[1] - totals[0]).abs().max((totals[2] - totals[1]).abs()) / PIXELS,
		sigma: (energy / PIXELS / RESIDUAL_GAIN).sqrt(),
		usable: gradient / PIXELS < FLAT_GRADIENT && outer_energy <= MOTION_RATIO * energy + STILL_SLACK * PIXELS,
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

fn median(values: &mut [f64]) -> f64 {
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

	fn noisy_gray(sigma: f64, seed: u64) -> [Vec<u8>; 3] {
		three(|index| {
			let mut random = Random(seed + index as u64 * 7919);
			encode(W, H, |_, _| 120.0 + sigma * random.gaussian())
		})
	}

	#[test]
	fn temporal_noise_on_a_flat_picture_is_measured_as_its_deviation() {
		for sigma in [0.5, 1.0, 2.0, 4.0] {
			let level = strongest(measured(&noisy_gray(sigma, 11))).unwrap();

			assert!((level - sigma).abs() < sigma * 0.15, "{sigma}: {level}");
		}
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
			strongest(measured(&pan)).is_none_or(|level| level < 0.3),
			"{:?}",
			measured(&pan)
		);
		assert!(
			strongest(measured(&fade)).is_none_or(|level| level < 0.05),
			"{:?}",
			measured(&fade)
		);
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
			triplet([Some(0.1), Some(1.0), None, Some(9.0)]),
			triplet([Some(0.2), Some(1.2), None, None]),
			triplet([Some(0.1), Some(1.1), Some(3.0), None]),
			triplet([Some(0.3), None, None, None]),
			triplet([None, None, None, None]),
			triplet([Some(0.2), Some(0.9), None, None]),
		];

		assert_eq!(level(&triplets), Some(1.05));
		assert_eq!(level(&triplets[..1]), None);
		assert_eq!(level(&[]), None);
	}

	#[test]
	fn pgm_frames_are_read_back_to_back() {
		let mut bytes = b"P5\n4 2\n65535\n".to_vec();
		bytes.extend(encode(4, 2, |x, _| f64::from(u8::try_from(x).unwrap()) * 10.0));
		bytes.extend_from_slice(b"P5\n4 2\n255\n");
		bytes.extend([7_u8; 8]);
		bytes.extend_from_slice(b"P5\n4 2\n65535\n\x00");

		let frames = frames(&bytes);

		assert_eq!(frames.len(), 2);
		assert_eq!((frames[0].width, frames[0].height), (4, 2));
		assert!((frames[0].sample(3, 1) - 30.0).abs() < 1e-3);
		assert!((frames[1].sample(0, 0) - 7.0).abs() < 1e-3);
	}

	#[test]
	fn the_command_decodes_frame_triplets_to_16_bit_luma() {
		let segment = Segment {
			start_s: 12.5,
			length_s: Some(3.0),
		};

		let cmd = command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			2,
			Some("libvpx-vp9"),
			&segment,
			Some(23.976),
		);
		let args = crate::video::joined(&cmd);

		assert!(args.contains("-ss 12.500 -t 3.000 -c:v libvpx-vp9 -i "), "{args}");
		assert!(
			args.contains("-map 0:2 -vf select='lt(mod(n,24),3)',extractplanes=y,format=gray16be -frames:v 9 "),
			"{args}"
		);
		assert!(args.ends_with("-c:v pgm -f image2pipe -"), "{args}");
	}
}
