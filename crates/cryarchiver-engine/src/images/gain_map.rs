use std::fs;

use avif_serialize::Aviffy;
use avif_serialize::constants::{ColorPrimaries, MatrixCoefficients, TransferCharacteristics};
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, GrayImage, ImageFormat};
use rav1e::prelude::{
	BlockSize, ChromaSamplePosition, ChromaSampling, ColorDescription, Config, Context, EncoderConfig, EncoderStatus,
	FrameType, PartitionRange, PixelRange, PredictionModesSetting, Rational, SGRComplexityLevel, SceneDetectionSpeed,
	SpeedSettings, Tune,
};
use tokio_util::sync::CancellationToken;

use crate::container::av1;
use crate::images::blocking;
use crate::images::decode::{self, PamRequest, pam_command, run_pam};
use crate::images::ssim;
use crate::metadata::gain_map::GainMapSource;
use crate::outcome::{IoSide, JobError};
use crate::toolchain::Toolchain;
use crate::workspace::Workspace;

/// The SSIMULACRA2 score an encoded gain map must reach against the gain map it was made from.
pub const TARGET: f64 = 70.0;
/// The quality a gain map is encoded again at when its first encode misses `TARGET`.
pub const RAISED_QUALITY: f32 = 90.0;

const ENCODED_FILE: &str = "gainmap.avif";

pub fn decode(bytes: &[u8], source: &GainMapSource) -> Result<GrayImage, String> {
	let jpeg = bytes
		.get(source.jpeg.clone())
		.ok_or("the gain map lies outside the file")?;
	let image = image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg)
		.map_err(|err| format!("the gain map could not be decoded: {err}"))?;

	match image {
		DynamicImage::ImageLuma8(gain) => Ok(gain),
		_ => Err("colour gain maps are not supported".into()),
	}
}

/// The gain map put through what the base went through: the base's orientation, then its resize, given by the base's
/// upright size before and after it.
pub fn fit(gain: GrayImage, orientation: u16, base_before: (u32, u32), base_after: (u32, u32)) -> GrayImage {
	let upright = decode::oriented(DynamicImage::ImageLuma8(gain), orientation).into_luma8();
	if base_before == base_after {
		return upright;
	}

	let width = scaled(upright.width(), base_before.0, base_after.0);
	let height = scaled(upright.height(), base_before.1, base_after.1);
	image::imageops::resize(&upright, width, height, FilterType::Lanczos3)
}

fn scaled(side: u32, before: u32, after: u32) -> u32 {
	let rounded = (u64::from(side) * u64::from(after) + u64::from(before) / 2)
		.checked_div(u64::from(before))
		.unwrap_or(u64::from(side));
	u32::try_from(rounded).unwrap_or(u32::MAX).max(1)
}

/// A complete single-image AVIF holding the gain map as monochrome 8-bit full-range AV1.
pub fn encode(gain: &GrayImage, quality: f32, speed: u8, threads: usize) -> Result<Vec<u8>, String> {
	let pool = rayon::ThreadPoolBuilder::new()
		.num_threads(threads.max(1))
		.build()
		.map_err(|err| format!("could not start the encoder threads: {err}"))?;

	let av1 = pool.install(|| encode_av1(gain, quality_to_quantizer(quality), speed))?;
	let header = av1::read_sequence_header(&av1).ok_or("rav1e wrote no AV1 sequence header")?;

	let mut aviffy = Aviffy::new();
	aviffy
		.set_seq_profile(header.profile)
		.set_seq_level_idx(header.level, header.level)
		.set_monochrome(true)
		.set_chroma_subsampling((true, true))
		.set_color_primaries(ColorPrimaries::Unspecified)
		.set_transfer_characteristics(TransferCharacteristics::Unspecified)
		.set_matrix_coefficients(MatrixCoefficients::Bt601)
		.set_full_color_range(true);

	let mut out = Vec::with_capacity(av1.len() + 512);
	aviffy
		.write(&mut out, &av1, None, gain.width(), gain.height(), 8)
		.map_err(|err| format!("could not write the gain map AVIF: {err}"))?;
	Ok(out)
}

fn encode_av1(gain: &GrayImage, quantizer: u8, speed: u8) -> Result<Vec<u8>, String> {
	let (width, height) = (gain.width() as usize, gain.height() as usize);
	let below_80 = quantizer > quality_to_quantizer(80.0);
	let min_tile_size = usize::from(min_tile_size(speed, below_80));
	let tiles = rayon::current_num_threads().min(width * height / min_tile_size.pow(2));

	let config = Config::new().with_encoder_config(EncoderConfig {
		width,
		height,
		time_base: Rational::new(1, 1),
		sample_aspect_ratio: Rational::new(1, 1),
		bit_depth: 8,
		chroma_sampling: ChromaSampling::Cs400,
		chroma_sample_position: ChromaSamplePosition::Unknown,
		pixel_range: PixelRange::Full,
		color_description: Some(ColorDescription {
			color_primaries: rav1e::prelude::ColorPrimaries::Unspecified,
			transfer_characteristics: rav1e::prelude::TransferCharacteristics::Unspecified,
			matrix_coefficients: rav1e::prelude::MatrixCoefficients::BT601,
		}),
		mastering_display: None,
		content_light: None,
		enable_timing_info: false,
		still_picture: true,
		error_resilient: false,
		switch_frame_interval: 0,
		min_key_frame_interval: 0,
		max_key_frame_interval: 0,
		reservoir_frame_delay: None,
		low_latency: false,
		quantizer: usize::from(quantizer),
		min_quantizer: quantizer,
		bitrate: 0,
		tune: Tune::Psychovisual,
		tile_cols: 0,
		tile_rows: 0,
		tiles,
		film_grain_params: None,
		level_idx: None,
		speed_settings: speed_settings(speed, quantizer),
	});

	let mut context: Context<u8> = config
		.new_context()
		.map_err(|err| format!("rav1e refused its settings: {err}"))?;
	let mut frame = context.new_frame();
	frame.planes[0].copy_from_raw_u8(gain.as_raw(), width, 1);
	context
		.send_frame(frame)
		.map_err(|err| format!("rav1e failed: {err}"))?;
	context.flush();

	let mut out = Vec::new();
	loop {
		match context.receive_packet() {
			Ok(mut packet) if packet.frame_type == FrameType::KEY => out.append(&mut packet.data),
			Ok(_) => {}
			Err(EncoderStatus::Encoded | EncoderStatus::LimitReached) => break,
			Err(err) => return Err(format!("rav1e failed: {err}")),
		}
	}
	Ok(out)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn quality_to_quantizer(quality: f32) -> u8 {
	let q = quality / 100.0;
	let x = if q >= 0.82 {
		(1.0 - q) * 2.6
	} else if q > 0.25 {
		q.mul_add(-0.5, 1.0 - 0.125)
	} else {
		1.0 - q
	};
	(x * 255.0).round() as u8
}

fn min_tile_size(speed: u8, below_80: bool) -> u16 {
	let size = match speed {
		0 => 4096,
		1 => 2048,
		2 => 1024,
		3 => 512,
		4 => 256,
		_ => 128,
	};
	if below_80 { size * 2 } else { size }
}

/// The tweaks the base image's own encoder applies to its colour plane, so that a gain map at a given quality and speed
/// is encoded the way the base is.
fn speed_settings(speed: u8, quantizer: u8) -> SpeedSettings {
	let above_55 = quantizer < quality_to_quantizer(55.0);
	let below_80 = quantizer > quality_to_quantizer(80.0);
	let max_block_size = if below_80 { 16 } else { 64 };

	let mut settings = SpeedSettings::from_preset(speed);
	settings.multiref = false;
	settings.rdo_lookahead_frames = 1;
	settings.scene_detection_mode = SceneDetectionSpeed::None;
	settings.motion.include_near_mvs = false;

	settings.fast_deblock = speed >= 7 && !below_80;
	settings.transform.reduced_tx_set = speed == 4 || speed >= 9;
	settings.transform.tx_domain_rate = speed >= 10;
	settings.partition.encode_bottomup = speed <= 2;
	settings.transform.rdo_tx_decision = speed <= 4 && !below_80;
	settings.cdef = above_55 && speed <= 9;
	settings.lrf = above_55 && speed <= 8;
	settings.transform.enable_inter_tx_split = speed >= 9;
	settings.sgr_complexity = if speed <= 2 {
		SGRComplexityLevel::Full
	} else {
		SGRComplexityLevel::Reduced
	};
	settings.motion.use_satd_subpel = false;
	settings.prediction.fine_directional_intra = speed <= 6;
	settings.prediction.prediction_modes = if speed <= 1 {
		PredictionModesSetting::ComplexAll
	} else {
		PredictionModesSetting::Simple
	};

	let (min, max) = match speed {
		0 => (4, 64.min(max_block_size)),
		1 if above_55 => (4, 64.min(max_block_size)),
		2 if above_55 => (4, 32.min(max_block_size)),
		1..=4 => (4, 16),
		5..=8 => (8, 16),
		_ => (16, 16),
	};
	settings.partition.partition_range = PartitionRange::new(block_size(min), block_size(max));
	settings
}

fn block_size(side: u8) -> BlockSize {
	match side {
		4 => BlockSize::BLOCK_4X4,
		8 => BlockSize::BLOCK_8X8,
		16 => BlockSize::BLOCK_16X16,
		32 => BlockSize::BLOCK_32X32,
		_ => BlockSize::BLOCK_64X64,
	}
}

pub async fn score(
	tools: &Toolchain,
	workspace: &Workspace,
	encoded: &[u8],
	reference: &GrayImage,
	cancel: &CancellationToken,
) -> Result<f64, JobError> {
	let path = workspace.file(ENCODED_FILE);
	fs::write(&path, encoded).map_err(|err| JobError::io(IoSide::Destination(&path), &err))?;

	let request = PamRequest {
		alpha_item: false,
		deep: false,
		keep_stored_orientation: false,
	};
	let (decoded, errors) = run_pam(&pam_command(&tools.ffmpeg, &path, request), cancel)
		.await
		.map_err(|err| JobError::verification(err.kind))?;
	if errors > 0 {
		return Err(JobError::verification(format!(
			"the gain map decodes with {errors} errors"
		)));
	}

	if decoded.dimensions() != reference.dimensions() {
		return Err(JobError::verification(format!(
			"the gain map decodes as {:?} but {:?} was encoded",
			decoded.dimensions(),
			reference.dimensions()
		)));
	}

	let reference = DynamicImage::ImageRgb8(DynamicImage::ImageLuma8(reference.clone()).into_rgb8());
	let decoded = DynamicImage::ImageRgb8(decoded.into_rgb8());
	blocking(cancel, move || ssim::score(&reference, &decoded))
		.await?
		.map_err(JobError::verification)
}
