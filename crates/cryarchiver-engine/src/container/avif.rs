use avif_serialize::Aviffy;
use avif_serialize::constants::{ColorPrimaries, MatrixCoefficients, TransferCharacteristics};

use crate::container::av1::{self, SequenceHeader};
use crate::container::isobmff::{self, Meta, Property};
use crate::metadata::icc::Cicp;

const UNSPECIFIED_MATRIX: u8 = 2;
const DEFAULT_LEVEL: u8 = 31;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct AvifError(String);

impl From<isobmff::IsobmffError> for AvifError {
	fn from(err: isobmff::IsobmffError) -> Self {
		Self(err.to_string())
	}
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AvifMetadata<'a> {
	pub icc: Option<&'a [u8]>,
	pub exif: Option<&'a [u8]>,
	pub xmp: Option<&'a [u8]>,
}

#[derive(Debug, Clone, Copy)]
pub struct AvifExpectations<'a> {
	pub width: u32,
	pub height: u32,
	pub alpha: bool,
	pub cicp: Cicp,
	pub metadata: AvifMetadata<'a>,
}

pub fn writable_cicp(cicp: Cicp) -> Cicp {
	Cicp {
		primaries: primaries(cicp.primaries) as u8,
		transfer: transfer(cicp.transfer) as u8,
	}
}

pub fn rewrite(encoded: &[u8], cicp: Cicp, metadata: &AvifMetadata<'_>) -> Result<Vec<u8>, AvifError> {
	let cicp = writable_cicp(cicp);
	let meta = isobmff::parse_meta(encoded)?;
	let primary = meta
		.primary
		.ok_or_else(|| AvifError("the encoder output has no primary item".into()))?;
	let mut color = meta.item_data(encoded, primary)?;
	let alpha = meta.alpha_item().map(|id| meta.item_data(encoded, id)).transpose()?;

	let header = av1::read_sequence_header(&color)
		.ok_or_else(|| AvifError("the colour item has no AV1 sequence header".into()))?;
	let alpha_level = alpha
		.as_deref()
		.and_then(av1::read_sequence_header)
		.map_or(DEFAULT_LEVEL, |alpha| alpha.level);
	let (width, height) = meta.primary_size().unwrap_or((header.width, header.height));
	let matrix_value = header
		.matrix
		.or_else(|| meta.primary_nclx().and_then(|nclx| u8::try_from(nclx.matrix).ok()))
		.unwrap_or(UNSPECIFIED_MATRIX);
	av1::patch_cicp(&mut color, cicp.primaries, cicp.transfer);

	let mut aviffy = Aviffy::new();
	aviffy
		.set_width(width)
		.set_height(height)
		.set_bit_depth(header.bit_depth)
		.set_seq_profile(header.profile)
		.set_monochrome(header.monochrome)
		.set_chroma_subsampling(header.subsampling)
		.set_matrix_coefficients(matrix(matrix_value)?)
		.set_full_color_range(header.full_range)
		.set_color_primaries(primaries(cicp.primaries))
		.set_transfer_characteristics(transfer(cicp.transfer))
		.set_premultiplied_alpha(meta.premultiplied())
		.set_seq_level_idx(header.level, alpha_level);
	if let Some(icc) = metadata.icc {
		aviffy.set_icc_profile(icc.to_vec());
	}
	if let Some(exif) = metadata.exif {
		aviffy.set_exif(exif.to_vec());
	}
	if let Some(xmp) = metadata.xmp {
		aviffy.set_xmp(xmp.to_vec());
	}

	let mut out = Vec::with_capacity(encoded.len() + 4096);
	aviffy
		.write_slice(&mut out, &color, alpha.as_deref())
		.map_err(|err| AvifError(format!("could not write the AVIF: {err}")))?;
	Ok(out)
}

fn primaries(value: u8) -> ColorPrimaries {
	match value {
		1 => ColorPrimaries::Bt709,
		6 => ColorPrimaries::Bt601,
		9 => ColorPrimaries::Bt2020,
		11 => ColorPrimaries::DciP3,
		12 => ColorPrimaries::DisplayP3,
		_ => ColorPrimaries::Unspecified,
	}
}

fn transfer(value: u8) -> TransferCharacteristics {
	match value {
		1 => TransferCharacteristics::Bt709,
		6 => TransferCharacteristics::Bt601,
		7 => TransferCharacteristics::Smpte240,
		8 => TransferCharacteristics::Linear,
		9 => TransferCharacteristics::Log,
		10 => TransferCharacteristics::LogSqrt,
		11 => TransferCharacteristics::Iec61966,
		13 => TransferCharacteristics::Srgb,
		14 => TransferCharacteristics::Bt2020_10,
		15 => TransferCharacteristics::Bt2020_12,
		16 => TransferCharacteristics::Smpte2084,
		17 => TransferCharacteristics::Smpte428,
		18 => TransferCharacteristics::Hlg,
		_ => TransferCharacteristics::Unspecified,
	}
}

fn matrix(value: u8) -> Result<MatrixCoefficients, AvifError> {
	Ok(match value {
		0 => MatrixCoefficients::Rgb,
		1 => MatrixCoefficients::Bt709,
		2 => MatrixCoefficients::Unspecified,
		6 => MatrixCoefficients::Bt601,
		8 => MatrixCoefficients::Ycgco,
		9 => MatrixCoefficients::Bt2020Ncl,
		10 => MatrixCoefficients::Bt2020Cl,
		other => {
			return Err(AvifError(format!(
				"matrix coefficients {other} can't be written to AVIF"
			)));
		}
	})
}

pub fn check(bytes: &[u8], expected: &AvifExpectations<'_>) -> Result<(), String> {
	if !isobmff::brands(bytes).is_some_and(|brands| brands.contains(b"avif")) {
		return Err("the output is not branded 'avif'".into());
	}

	let meta = isobmff::parse_meta(bytes).map_err(|err| err.to_string())?;
	let primary = meta.primary_item().ok_or("the output has no primary item")?;
	if primary.kind != *b"av01" {
		return Err("the primary item is not AV1".into());
	}
	let color = meta.item_data(bytes, primary.id).map_err(|err| err.to_string())?;
	let header = av1::read_sequence_header(&color).ok_or("the colour item has no AV1 sequence header")?;

	check_geometry(&meta, &header, expected)?;
	check_colour(&meta, primary.id, &header, expected)?;
	check_metadata(&meta, bytes, &expected.metadata)?;

	if meta.locations.iter().any(|location| location.extents.len() != 1) {
		return Err("an item is split over several extents".into());
	}
	Ok(())
}

fn check_geometry(meta: &Meta, header: &SequenceHeader, expected: &AvifExpectations<'_>) -> Result<(), String> {
	let size = meta.primary_size();
	if size != Some((expected.width, expected.height))
		|| (header.width, header.height) != (expected.width, expected.height)
	{
		return Err(format!(
			"the output is {size:?} but {}×{} was expected",
			expected.width, expected.height
		));
	}
	if meta.alpha_item().is_some() != expected.alpha {
		return Err(if expected.alpha {
			"the alpha channel is missing".into()
		} else {
			"the output has an unexpected alpha channel".into()
		});
	}
	Ok(())
}

fn check_colour(
	meta: &Meta,
	primary: u32,
	header: &SequenceHeader,
	expected: &AvifExpectations<'_>,
) -> Result<(), String> {
	let properties: Vec<&Property> = meta.properties_of(primary).collect();
	if properties
		.iter()
		.any(|property| matches!(property, Property::Rotation(_) | Property::Mirror(_)))
	{
		return Err("the output carries a rotation or mirror box".into());
	}

	let nclx: Vec<_> = properties
		.iter()
		.filter_map(|property| match property {
			Property::Nclx(nclx) => Some(*nclx),
			_ => None,
		})
		.collect();
	let [nclx] = nclx.as_slice() else {
		return Err(format!("expected one nclx colour box, found {}", nclx.len()));
	};
	let matrix_matches = header.matrix.is_none_or(|matrix| nclx.matrix == u16::from(matrix));
	let wanted = (
		u16::from(expected.cicp.primaries),
		u16::from(expected.cicp.transfer),
		header.full_range,
	);
	if (nclx.primaries, nclx.transfer, nclx.full_range) != wanted || !matrix_matches {
		return Err(format!(
			"the nclx box says {nclx:?} but primaries {}, transfer {}, matrix {:?} and full range {} were expected",
			expected.cicp.primaries, expected.cicp.transfer, header.matrix, header.full_range
		));
	}

	if meta.primary_icc() != expected.metadata.icc {
		return Err("the ICC profile was not carried over unchanged".into());
	}
	check_av1_config(&properties, header)
}

fn check_av1_config(properties: &[&Property], header: &SequenceHeader) -> Result<(), String> {
	let config = properties
		.iter()
		.find_map(|property| match property {
			Property::Av1Config(config) => Some(config.as_slice()),
			_ => None,
		})
		.ok_or("the colour item has no av1C box")?;
	let [_, profile_level, flags, ..] = config else {
		return Err("the av1C box is too short".into());
	};

	let profile = profile_level >> 5;
	let level = profile_level & 0x1F;
	let monochrome = flags & 0x10 != 0;
	let subsampling = (flags & 0x08 != 0, flags & 0x04 != 0);
	if (profile, level, monochrome, subsampling)
		!= (header.profile, header.level, header.monochrome, header.subsampling)
	{
		return Err("the av1C box does not match the AV1 sequence header".into());
	}
	Ok(())
}

fn check_metadata(meta: &Meta, bytes: &[u8], expected: &AvifMetadata<'_>) -> Result<(), String> {
	let exif = meta.exif_tiff(bytes).transpose().map_err(|err| err.to_string())?;
	if exif.as_deref() != expected.exif {
		return Err(if expected.exif.is_some() {
			"the EXIF item is missing or changed".into()
		} else {
			"the output has an unexpected EXIF item".into()
		});
	}

	let xmp = meta.xmp(bytes).transpose().map_err(|err| err.to_string())?;
	if xmp.as_deref() != expected.xmp {
		return Err(if expected.xmp.is_some() {
			"the XMP item is missing or changed".into()
		} else {
			"the output has an unexpected XMP item".into()
		});
	}
	Ok(())
}
