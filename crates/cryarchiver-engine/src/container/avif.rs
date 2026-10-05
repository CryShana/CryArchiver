use avif_serialize::constants::{ColorPrimaries, MatrixCoefficients, TransferCharacteristics};
use avif_serialize::{AlternateColour, Av1Config, Aviffy, GainMap};

use crate::container::av1::{self, SequenceHeader};
use crate::container::isobmff::{self, Meta, Nclx, Property};
use crate::metadata::gain_map::GainMapMetadata;
use crate::metadata::icc::Cicp;

const UNSPECIFIED_MATRIX: u8 = 2;
const BT601_MATRIX: u8 = 6;
const UNSPECIFIED_CICP: u8 = 2;
const DEFAULT_LEVEL: u8 = 31;
const TMAP_VERSION: u8 = 0;

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
	pub gain_map: Option<&'a GainMapMetadata>,
	/// With a gain map: its AV1 sequence header, when it has a colour description, must also say primaries 2 and
	/// transfer 2, as its `nclx` box does.
	pub gain_map_coded_cicp: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct GainMapInput<'a> {
	/// A complete single-image AVIF holding only the gain map (as the gain map encoder returns it), from which the AV1
	/// data and its sequence header are taken.
	pub encoded: &'a [u8],
	pub metadata: &'a GainMapMetadata,
}

pub fn writable_cicp(cicp: Cicp) -> Cicp {
	Cicp {
		primaries: primaries(cicp.primaries) as u8,
		transfer: transfer(cicp.transfer) as u8,
	}
}

pub fn rewrite(
	encoded: &[u8],
	cicp: Cicp,
	metadata: &AvifMetadata<'_>,
	gain_map: Option<&GainMapInput<'_>>,
) -> Result<Vec<u8>, AvifError> {
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
	let base_matrix = matrix(matrix_value)?;
	av1::patch_cicp(&mut color, cicp.primaries, cicp.transfer);

	let mut aviffy = Aviffy::new();
	aviffy
		.set_width(width)
		.set_height(height)
		.set_bit_depth(header.bit_depth)
		.set_seq_profile(header.profile)
		.set_monochrome(header.monochrome)
		.set_chroma_subsampling(header.subsampling)
		.set_matrix_coefficients(base_matrix)
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

	if let Some(gain_map) = gain_map {
		aviffy.set_gain_map(gain_map_parts(
			gain_map,
			primaries(cicp.primaries),
			base_matrix,
			metadata.icc,
		)?);
	}

	let mut out = Vec::with_capacity(encoded.len() + 4096);
	aviffy
		.write_slice(&mut out, &color, alpha.as_deref())
		.map_err(|err| AvifError(format!("could not write the AVIF: {err}")))?;
	Ok(out)
}

fn gain_map_parts(
	input: &GainMapInput<'_>,
	base_primaries: ColorPrimaries,
	base_matrix: MatrixCoefficients,
	base_icc: Option<&[u8]>,
) -> Result<GainMap, AvifError> {
	let meta = isobmff::parse_meta(input.encoded)?;
	let primary = meta
		.primary
		.ok_or_else(|| AvifError("the gain map encoder output has no primary item".into()))?;
	let av1_data = meta.item_data(input.encoded, primary)?;
	let header = av1::read_sequence_header(&av1_data)
		.ok_or_else(|| AvifError("the gain map item has no AV1 sequence header".into()))?;
	let (width, height) = meta.primary_size().unwrap_or((header.width, header.height));

	let mut tmap_payload = vec![TMAP_VERSION];
	tmap_payload.extend(input.metadata.to_iso());

	Ok(GainMap {
		av1_data,
		width,
		height,
		config: Av1Config {
			seq_profile: header.profile,
			seq_level_idx: header.level,
			bit_depth: header.bit_depth,
			monochrome: header.monochrome,
			chroma_subsampling: header.subsampling,
		},
		matrix_coefficients: matrix(header.matrix.unwrap_or(BT601_MATRIX))?,
		full_range: header.full_range,
		tmap_payload,
		alternate: AlternateColour {
			color_primaries: base_primaries,
			transfer_characteristics: TransferCharacteristics::Smpte2084,
			matrix_coefficients: base_matrix,
			full_range: true,
			icc: base_icc.map(<[u8]>::to_vec),
		},
	})
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

	match expected.gain_map {
		Some(gain_map) => check_gain_map(&meta, bytes, expected, gain_map)?,
		None if meta.has_item_kind(*b"tmap") => return Err("the output has an unexpected gain map".into()),
		None => {}
	}

	if meta.locations.iter().any(|location| location.extents.len() != 1) {
		return Err("an item is split over several extents".into());
	}
	Ok(())
}

fn check_gain_map(
	meta: &Meta,
	bytes: &[u8],
	expected: &AvifExpectations<'_>,
	gain_map: &GainMapMetadata,
) -> Result<(), String> {
	// libavif looks for the brand among the compatible brands only, never the major brand.
	if !isobmff::brands(bytes).is_some_and(|brands| brands[1..].contains(b"tmap")) {
		return Err("the output has a gain map but 'tmap' is not among its compatible brands".into());
	}

	let items = meta
		.gain_map()
		.ok_or("the gain map is missing or not laid out as libavif reads it (tmap, dimg, altr)")?;

	let tmap_properties: Vec<&Property> = meta.properties_of(items.tmap).collect();
	let tmap_size = tmap_properties.iter().find_map(|property| match property {
		Property::Size { width, height } => Some((*width, *height)),
		_ => None,
	});
	if tmap_size != Some((expected.width, expected.height)) {
		return Err(format!(
			"the tone map item is {tmap_size:?} but {}×{} was expected",
			expected.width, expected.height
		));
	}

	if tmap_properties.iter().any(|property| {
		matches!(property, Property::Rotation(_) | Property::Mirror(_))
			|| matches!(property, Property::Other(kind) if kind == b"clap" || kind == b"pasp")
	}) {
		return Err("the tone map item carries a transform box".into());
	}

	check_gain_map_item(meta, bytes, items.gain_map, expected.gain_map_coded_cicp)?;

	let payload = meta.item_data(bytes, items.tmap).map_err(|err| err.to_string())?;
	let Some((&TMAP_VERSION, iso)) = payload.split_first() else {
		return Err("the tone map item is empty or its version is not 0".into());
	};
	let stored =
		GainMapMetadata::parse_iso(iso).map_err(|err| format!("the gain map metadata can't be read back: {err}"))?;
	if stored != *gain_map {
		return Err("the gain map metadata was not carried over unchanged".into());
	}
	Ok(())
}

fn check_gain_map_item(meta: &Meta, bytes: &[u8], id: u32, coded_cicp: bool) -> Result<(), String> {
	let item = meta.item(id).ok_or("the gain map item is missing")?;
	if item.kind != *b"av01" {
		return Err("the gain map item is not AV1".into());
	}

	if !item.hidden {
		return Err("the gain map item is not hidden".into());
	}

	let properties: Vec<&Property> = meta.properties_of(id).collect();
	let nclx: Vec<Nclx> = properties
		.iter()
		.filter_map(|property| match property {
			Property::Nclx(nclx) => Some(*nclx),
			_ => None,
		})
		.collect();
	let [nclx] = nclx.as_slice() else {
		return Err(format!(
			"expected one nclx colour box on the gain map, found {}",
			nclx.len()
		));
	};
	let unspecified = u16::from(UNSPECIFIED_CICP);
	if (nclx.primaries, nclx.transfer) != (unspecified, unspecified) {
		return Err(format!(
			"the gain map's nclx box says {nclx:?} but primaries 2 and transfer 2 were expected"
		));
	}

	let data = meta.item_data(bytes, id).map_err(|err| err.to_string())?;
	let header = av1::read_sequence_header(&data).ok_or("the gain map item has no AV1 sequence header")?;
	if !header.monochrome && header.subsampling != (false, false) {
		return Err("the gain map is neither monochrome nor 4:4:4".into());
	}

	let channels = if header.monochrome { 1 } else { 3 };
	if !properties.contains(&&Property::PixelDepths(vec![header.bit_depth; channels])) {
		return Err(format!(
			"the gain map's pixi box does not say {channels} channels of {} bits",
			header.bit_depth
		));
	}

	if header.matrix.is_some_and(|matrix| nclx.matrix != u16::from(matrix)) || nclx.full_range != header.full_range {
		return Err(format!(
			"the gain map's nclx box says {nclx:?} but its AV1 sequence header says matrix {:?} and full range {}",
			header.matrix, header.full_range
		));
	}

	check_av1_config(&properties, &header, "gain map item")?;

	if coded_cicp
		&& [header.primaries, header.transfer]
			.into_iter()
			.any(|code| code.is_some_and(|code| code != UNSPECIFIED_CICP))
	{
		return Err(format!(
			"the gain map's AV1 sequence header says primaries {:?}, transfer {:?} but 2 and 2 were expected",
			header.primaries, header.transfer
		));
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
	check_av1_config(&properties, header, "colour item")
}

fn check_av1_config(properties: &[&Property], header: &SequenceHeader, item: &str) -> Result<(), String> {
	let config = properties
		.iter()
		.find_map(|property| match property {
			Property::Av1Config(config) => Some(config.as_slice()),
			_ => None,
		})
		.ok_or_else(|| format!("the {item} has no av1C box"))?;
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
		return Err(format!("the {item}'s av1C box does not match its AV1 sequence header"));
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
