use std::ops::Range;

use crate::metadata::auxiliary::{self, HDR_GAIN_MAP};
use crate::metadata::{jpeg, xmp};

const MULTICHANNEL: u8 = 0x80;
const USE_BASE_COLOUR_SPACE: u8 = 0x40;
const COMMON_DENOMINATOR: u8 = 0x08;
const BACKWARD_DIRECTION: u8 = 0x04;
const HDR_BASE: &str = "HDR base rendition is not supported";
const DEFAULT_OFFSET: f64 = 1.0 / 64.0;
const MAX_FRACTION_ITERATIONS: usize = 39;
const JPEG_START: [u8; 3] = [0xFF, 0xD8, 0xFF];
const MAX_GAIN_MAP_CANDIDATES: usize = 16;
/// How far into an embedded image its header is read: 16 full-size segments. A run of standalone markers would
/// otherwise make every probe walk to the end of the file.
const MAX_HEADER_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signed {
	pub numerator: i32,
	pub denominator: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsigned {
	pub numerator: u32,
	pub denominator: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
	pub min: Signed, // log2
	pub max: Signed, // log2
	pub gamma: Unsigned,
	pub base_offset: Signed,
	pub alternate_offset: Signed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GainMapMetadata {
	pub use_base_colour_space: bool,
	pub base_headroom: Unsigned,      // log2
	pub alternate_headroom: Unsigned, // log2
	/// One channel, or three (R, G, B) when they differ.
	pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GainMapSource {
	/// The gain map JPEG inside the source file, SOI to EOI.
	pub jpeg: Range<usize>,
	pub metadata: GainMapMetadata,
}

impl Signed {
	fn scaled(self, denominator: u32) -> i64 {
		i64::from(self.numerator) * i64::from(denominator)
	}

	fn write(self, out: &mut Vec<u8>) {
		out.extend_from_slice(&self.numerator.to_be_bytes());
		out.extend_from_slice(&self.denominator.to_be_bytes());
	}
}

impl Unsigned {
	fn scaled(self, denominator: u32) -> u64 {
		u64::from(self.numerator) * u64::from(denominator)
	}

	fn write(self, out: &mut Vec<u8>) {
		out.extend_from_slice(&self.numerator.to_be_bytes());
		out.extend_from_slice(&self.denominator.to_be_bytes());
	}
}

impl Channel {
	fn denominators(&self) -> [u32; 5] {
		[
			self.min.denominator,
			self.max.denominator,
			self.gamma.denominator,
			self.base_offset.denominator,
			self.alternate_offset.denominator,
		]
	}
}

impl GainMapMetadata {
	pub fn parse_iso(payload: &[u8]) -> Result<Self, String> {
		let mut reader = Reader { bytes: payload, at: 0 };
		let minimum_version = reader.u16()?;
		let writer_version = reader.u16()?;
		if minimum_version != 0 {
			return Err(format!(
				"ISO 21496-1 minimum version {minimum_version} is not supported"
			));
		}

		let flags = reader.u8()?;
		if flags & BACKWARD_DIRECTION != 0 {
			return Err(HDR_BASE.into());
		}
		let channel_count = if flags & MULTICHANNEL == 0 { 1 } else { 3 };
		let use_base_colour_space = flags & USE_BASE_COLOUR_SPACE != 0;

		let (base_headroom, alternate_headroom, channels) = if flags & COMMON_DENOMINATOR == 0 {
			let base_headroom = reader.unsigned()?;
			let alternate_headroom = reader.unsigned()?;
			let channels = (0..channel_count)
				.map(|_| {
					Ok(Channel {
						min: reader.signed()?,
						max: reader.signed()?,
						gamma: reader.unsigned()?,
						base_offset: reader.signed()?,
						alternate_offset: reader.signed()?,
					})
				})
				.collect::<Result<Vec<_>, String>>()?;
			(base_headroom, alternate_headroom, channels)
		} else {
			let denominator = reader.u32()?;
			let signed = |numerator| Signed { numerator, denominator };
			let unsigned = |numerator| Unsigned { numerator, denominator };
			let base_headroom = unsigned(reader.u32()?);
			let alternate_headroom = unsigned(reader.u32()?);
			let channels = (0..channel_count)
				.map(|_| {
					Ok(Channel {
						min: signed(reader.i32()?),
						max: signed(reader.i32()?),
						gamma: unsigned(reader.u32()?),
						base_offset: signed(reader.i32()?),
						alternate_offset: signed(reader.i32()?),
					})
				})
				.collect::<Result<Vec<_>, String>>()?;
			(base_headroom, alternate_headroom, channels)
		};

		if writer_version == 0 && reader.at < payload.len() {
			return Err(format!(
				"{} bytes follow the ISO 21496-1 metadata",
				payload.len() - reader.at
			));
		}

		let metadata = Self {
			use_base_colour_space,
			base_headroom,
			alternate_headroom,
			channels,
		};
		metadata.validate()?;
		Ok(metadata)
	}

	pub fn from_xmp(xmp: &str) -> Result<Self, String> {
		if !xmp::values(xmp, HDR_GAIN_MAP, "Version")
			.iter()
			.any(|version| version.trim() == "1.0")
		{
			return Err("the XMP has no hdrgm:Version 1.0".into());
		}

		match xmp::values(xmp, HDR_GAIN_MAP, "BaseRenditionIsHDR")
			.first()
			.map(|value| value.trim())
		{
			None | Some("False") => {}
			Some("True") => return Err(HDR_BASE.into()),
			Some(other) => return Err(format!("hdrgm:BaseRenditionIsHDR is '{other}'")),
		}

		let [base_headroom] = doubles::<1>(xmp, "HDRCapacityMin", 0.0)?;
		let [alternate_headroom] = doubles::<1>(xmp, "HDRCapacityMax", 1.0)?;
		let min = doubles::<3>(xmp, "GainMapMin", 0.0)?;
		let max = doubles::<3>(xmp, "GainMapMax", 1.0)?;
		let gamma = doubles::<3>(xmp, "Gamma", 1.0)?;
		let base_offset = doubles::<3>(xmp, "OffsetSDR", DEFAULT_OFFSET)?;
		let alternate_offset = doubles::<3>(xmp, "OffsetHDR", DEFAULT_OFFSET)?;

		if alternate_headroom <= base_headroom || base_headroom < 0.0 {
			return Err("hdrgm:HDRCapacityMax must exceed hdrgm:HDRCapacityMin, which can't be negative".into());
		}

		for c in 0..3 {
			if max[c] < min[c] || base_offset[c] < 0.0 || alternate_offset[c] < 0.0 || gamma[c] <= 0.0 {
				return Err(format!("the hdrgm values of channel {c} are out of range"));
			}
		}

		let not_a_fraction = |name: &str| format!("hdrgm:{name} can't be stored as a fraction");
		let mut channels = (0..3)
			.map(|c| {
				Ok(Channel {
					min: signed_fraction(min[c]).ok_or_else(|| not_a_fraction("GainMapMin"))?,
					max: signed_fraction(max[c]).ok_or_else(|| not_a_fraction("GainMapMax"))?,
					gamma: unsigned_fraction(gamma[c]).ok_or_else(|| not_a_fraction("Gamma"))?,
					base_offset: signed_fraction(base_offset[c]).ok_or_else(|| not_a_fraction("OffsetSDR"))?,
					alternate_offset: signed_fraction(alternate_offset[c])
						.ok_or_else(|| not_a_fraction("OffsetHDR"))?,
				})
			})
			.collect::<Result<Vec<_>, String>>()?;
		if channels.iter().all(|channel| *channel == channels[0]) {
			channels.truncate(1);
		}

		let metadata = Self {
			use_base_colour_space: true,
			base_headroom: unsigned_fraction(base_headroom).ok_or_else(|| not_a_fraction("HDRCapacityMin"))?,
			alternate_headroom: unsigned_fraction(alternate_headroom)
				.ok_or_else(|| not_a_fraction("HDRCapacityMax"))?,
			channels,
		};
		metadata.validate()?;
		Ok(metadata)
	}

	pub fn validate(&self) -> Result<(), String> {
		if !matches!(self.channels.len(), 1 | 3) {
			return Err(format!("{} gain map channels instead of 1 or 3", self.channels.len()));
		}

		if self.base_headroom.denominator == 0 || self.alternate_headroom.denominator == 0 {
			return Err("a headroom has a zero denominator".into());
		}

		for channel in &self.channels {
			if channel.denominators().contains(&0) {
				return Err("a gain map value has a zero denominator".into());
			}

			if channel.gamma.numerator == 0 {
				return Err("the gain map gamma is zero".into());
			}

			if channel.max.scaled(channel.min.denominator) < channel.min.scaled(channel.max.denominator) {
				return Err("the gain map maximum is below its minimum".into());
			}
		}

		let base = self.base_headroom.scaled(self.alternate_headroom.denominator);
		let alternate = self.alternate_headroom.scaled(self.base_headroom.denominator);
		if alternate <= base {
			return Err("the HDR headroom is not above the base headroom".into());
		}
		Ok(())
	}

	pub fn to_iso(&self) -> Vec<u8> {
		let mut flags = 0;
		if self.channels.len() == 3 {
			flags |= MULTICHANNEL;
		}

		if self.use_base_colour_space {
			flags |= USE_BASE_COLOUR_SPACE;
		}

		let mut out = Vec::with_capacity(21 + 40 * self.channels.len());
		out.extend_from_slice(&[0, 0, 0, 0, flags]);
		self.base_headroom.write(&mut out);
		self.alternate_headroom.write(&mut out);
		for channel in &self.channels {
			channel.min.write(&mut out);
			channel.max.write(&mut out);
			channel.gamma.write(&mut out);
			channel.base_offset.write(&mut out);
			channel.alternate_offset.write(&mut out);
		}
		out
	}
}

pub fn find(bytes: &[u8], mpf: Option<(&[u8], usize)>) -> Option<Result<GainMapSource, String>> {
	let primary = Header::of(bytes);
	if primary.iso.is_none() && !primary.xmp.is_some_and(auxiliary::signals_gain_map) {
		return None;
	}

	let listed = mpf.and_then(|(mpf, mpf_at)| {
		auxiliary::mp_entries(mpf)
			.ok()?
			.iter()
			.filter(|entry| entry.offset != 0)
			.filter_map(|entry| mpf_at.checked_add(usize::try_from(entry.offset).ok()?))
			.take(MAX_GAIN_MAP_CANDIDATES)
			.find(|&start| carries_gain_map(bytes, start))
	});
	if let Some(start) = listed {
		return Some(read(bytes, start));
	}

	Some(embedded_image(bytes).and_then(|start| read(bytes, start)))
}

struct Reader<'a> {
	bytes: &'a [u8],
	at: usize,
}

impl Reader<'_> {
	fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
		let bytes = self
			.bytes
			.get(self.at..self.at + N)
			.ok_or("the ISO 21496-1 metadata is cut off")?;
		self.at += N;
		Ok(bytes.try_into().expect("N bytes"))
	}

	fn u8(&mut self) -> Result<u8, String> {
		Ok(self.take::<1>()?[0])
	}

	fn u16(&mut self) -> Result<u16, String> {
		Ok(u16::from_be_bytes(self.take()?))
	}

	fn u32(&mut self) -> Result<u32, String> {
		Ok(u32::from_be_bytes(self.take()?))
	}

	fn i32(&mut self) -> Result<i32, String> {
		Ok(i32::from_be_bytes(self.take()?))
	}

	fn signed(&mut self) -> Result<Signed, String> {
		Ok(Signed {
			numerator: self.i32()?,
			denominator: self.u32()?,
		})
	}

	fn unsigned(&mut self) -> Result<Unsigned, String> {
		Ok(Unsigned {
			numerator: self.u32()?,
			denominator: self.u32()?,
		})
	}
}

struct Header<'a> {
	xmp: Option<&'a str>,
	iso: Option<&'a [u8]>,
}

impl<'a> Header<'a> {
	fn of(image: &'a [u8]) -> Self {
		let mut header = Self { xmp: None, iso: None };
		for segment in jpeg::SegmentWalk::new(image) {
			match segment.marker {
				0xE1 if header.xmp.is_none() => {
					header.xmp = segment
						.payload
						.strip_prefix(jpeg::XMP)
						.and_then(|xmp| std::str::from_utf8(xmp).ok());
				}
				0xE2 if header.iso.is_none() => header.iso = segment.payload.strip_prefix(jpeg::ISO_GAIN_MAP),
				_ => {}
			}
		}
		header
	}

	fn of_embedded(image: &'a [u8]) -> Self {
		Self::of(&image[..image.len().min(MAX_HEADER_BYTES)])
	}

	fn full_iso(&self) -> Option<&'a [u8]> {
		self.iso.filter(|iso| iso.len() > 4)
	}

	fn gain_map_xmp(&self) -> Option<&'a str> {
		self.xmp.filter(|xmp| xmp::uses_namespace(xmp, HDR_GAIN_MAP))
	}

	fn carries_gain_map(&self) -> bool {
		self.full_iso().is_some() || self.gain_map_xmp().is_some()
	}
}

fn image_at(bytes: &[u8], start: usize) -> Option<&[u8]> {
	bytes.get(start..).filter(|image| image.starts_with(&JPEG_START))
}

fn carries_gain_map(bytes: &[u8], start: usize) -> bool {
	image_at(bytes, start).is_some_and(|image| Header::of_embedded(image).carries_gain_map())
}

fn embedded_image(bytes: &[u8]) -> Result<usize, String> {
	let primary_end = jpeg::image_end(bytes).map_err(|err| format!("the primary image is broken ({err})"))?;
	let mut starts = bytes[primary_end..]
		.windows(JPEG_START.len())
		.enumerate()
		.filter(|(_, window)| *window == JPEG_START)
		.map(|(position, _)| primary_end + position)
		.peekable();
	let first = *starts.peek().ok_or("no gain map image follows the primary image")?;

	Ok(starts
		.take(MAX_GAIN_MAP_CANDIDATES)
		.find(|&start| carries_gain_map(bytes, start))
		.unwrap_or(first))
}

fn read(bytes: &[u8], start: usize) -> Result<GainMapSource, String> {
	let image = image_at(bytes, start).ok_or("the gain map image is outside the file")?;
	let end = jpeg::image_end(image).map_err(|err| format!("the gain map image is broken ({err})"))?;
	let header = Header::of_embedded(image);

	let metadata = match (header.full_iso(), header.gain_map_xmp()) {
		(Some(iso), _) => GainMapMetadata::parse_iso(iso)?,
		(None, Some(xmp)) => GainMapMetadata::from_xmp(xmp)?,
		(None, None) => return Err("the gain map image has no gain map metadata".into()),
	};
	Ok(GainMapSource {
		jpeg: start..start + end,
		metadata,
	})
}

fn doubles<const N: usize>(xmp: &str, name: &str, default: f64) -> Result<[f64; N], String> {
	let parsed = xmp::values(xmp, HDR_GAIN_MAP, name)
		.iter()
		.map(|value| {
			value
				.trim()
				.parse::<f64>()
				.map_err(|_| format!("hdrgm:{name} '{value}' is not a number"))
		})
		.collect::<Result<Vec<_>, String>>()?;

	match parsed.len() {
		0 => Ok([default; N]),
		1 => Ok([parsed[0]; N]),
		count => parsed
			.try_into()
			.map_err(|_| format!("hdrgm:{name} has {count} values")),
	}
}

fn signed_fraction(value: f64) -> Option<Signed> {
	let (numerator, denominator) = fraction(value.abs(), i32::MAX.unsigned_abs())?;
	let numerator = i32::try_from(numerator).ok()?;
	Some(Signed {
		numerator: if value < 0.0 { -numerator } else { numerator },
		denominator,
	})
}

fn unsigned_fraction(value: f64) -> Option<Unsigned> {
	let (numerator, denominator) = fraction(value, u32::MAX)?;
	Some(Unsigned { numerator, denominator })
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn fraction(value: f64, max_numerator: u32) -> Option<(u32, u32)> {
	if value.is_nan() || value < 0.0 || value > f64::from(max_numerator) {
		return None;
	}

	let max_denominator = if value <= 1.0 {
		u32::MAX
	} else {
		(f64::from(max_numerator) / value).floor() as u32
	};
	let mut denominator = 1u32;
	let mut previous_denominator = 0u32;
	let mut remainder = value - value.floor();

	for _ in 0..MAX_FRACTION_ITERATIONS {
		let exact = f64::from(denominator) * value;
		let numerator = exact.round() as u32;
		if exact - f64::from(numerator) == 0.0 {
			return Some((numerator, denominator));
		}

		remainder = 1.0 / remainder;
		let next = f64::from(previous_denominator) + remainder.floor() * f64::from(denominator);
		if next > f64::from(max_denominator) {
			return Some((numerator, denominator));
		}
		previous_denominator = denominator;
		denominator = next as u32;
		remainder -= remainder.floor();
	}
	Some(((f64::from(denominator) * value).round() as u32, denominator))
}

#[cfg(test)]
mod tests {
	use super::*;

	const WORKED_EXAMPLE: [u8; 61] = [
		0x00, 0x00, 0x00, 0x00, 0x40, //
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, //
		0x00, 0x00, 0x00, 0x17, 0x00, 0x00, 0x00, 0x0a, //
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, //
		0x00, 0x00, 0x00, 0x17, 0x00, 0x00, 0x00, 0x0a, //
		0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, //
		0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x40, //
		0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x40, //
	];

	fn signed(numerator: i32, denominator: u32) -> Signed {
		Signed { numerator, denominator }
	}

	fn unsigned(numerator: u32, denominator: u32) -> Unsigned {
		Unsigned { numerator, denominator }
	}

	fn channel(max: Signed) -> Channel {
		Channel {
			min: signed(0, 1),
			max,
			gamma: unsigned(1, 1),
			base_offset: signed(1, 64),
			alternate_offset: signed(1, 64),
		}
	}

	fn phone() -> GainMapMetadata {
		GainMapMetadata {
			use_base_colour_space: true,
			base_headroom: unsigned(0, 1),
			alternate_headroom: unsigned(23, 10),
			channels: vec![channel(signed(23, 10))],
		}
	}

	const XMP_HEAD: &str = "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:hdrgm='http://ns.adobe.com/hdr-gain-map/1.0/' hdrgm:Version='1.0'";
	const XMP_TAIL: &str = "</rdf:RDF></x:xmpmeta>";

	fn xmp(attributes: &str, children: &str) -> String {
		format!("{XMP_HEAD} {attributes}>{children}</rdf:Description>{XMP_TAIL}")
	}

	fn seq(name: &str, values: [&str; 3]) -> String {
		let items = values.map(|value| format!("<rdf:li>{value}</rdf:li>")).concat();
		format!("<hdrgm:{name}><rdf:Seq>{items}</rdf:Seq></hdrgm:{name}>")
	}

	#[test]
	fn the_worked_example_is_written_byte_exact_and_read_back() {
		assert_eq!(phone().to_iso(), WORKED_EXAMPLE);
		assert_eq!(GainMapMetadata::parse_iso(&WORKED_EXAMPLE), Ok(phone()));
	}

	#[test]
	fn three_channels_round_trip_with_the_multichannel_flag() {
		let metadata = GainMapMetadata {
			channels: vec![channel(signed(2, 1)), channel(signed(23, 10)), channel(signed(1, 2))],
			..phone()
		};

		let payload = metadata.to_iso();

		assert_eq!(payload.len(), 141);
		assert_eq!(payload[4], 0xC0);
		assert_eq!(GainMapMetadata::parse_iso(&payload), Ok(metadata));
	}

	#[test]
	fn the_legacy_common_denominator_layout_is_expanded() {
		let mut legacy = vec![0, 0, 0, 0, 0x48];
		for value in [64u32, 0, 147, 0, 147, 64, 1, 1] {
			legacy.extend_from_slice(&value.to_be_bytes());
		}
		let expected = GainMapMetadata {
			use_base_colour_space: true,
			base_headroom: unsigned(0, 64),
			alternate_headroom: unsigned(147, 64),
			channels: vec![Channel {
				min: signed(0, 64),
				max: signed(147, 64),
				gamma: unsigned(64, 64),
				base_offset: signed(1, 64),
				alternate_offset: signed(1, 64),
			}],
		};

		assert_eq!(GainMapMetadata::parse_iso(&legacy), Ok(expected.clone()));
		assert_eq!(GainMapMetadata::parse_iso(&expected.to_iso()), Ok(expected));
	}

	#[test]
	fn unsupported_or_invalid_payloads_are_refused() {
		let mut newer = WORKED_EXAMPLE;
		newer[1] = 1;
		let mut backward = WORKED_EXAMPLE;
		backward[4] |= 0x04;
		let mut zero_denominator = WORKED_EXAMPLE;
		zero_denominator[60] = 0;
		let inverted = GainMapMetadata {
			channels: vec![Channel {
				min: signed(3, 1),
				..channel(signed(23, 10))
			}],
			..phone()
		};
		let trailing = [WORKED_EXAMPLE.as_slice(), &[0]].concat();
		let mut newer_writer = trailing.clone();
		newer_writer[3] = 1;

		assert!(GainMapMetadata::parse_iso(&newer).is_err());
		assert!(
			GainMapMetadata::parse_iso(&backward)
				.unwrap_err()
				.contains("HDR base rendition is not supported")
		);
		assert!(GainMapMetadata::parse_iso(&zero_denominator).is_err());
		assert!(GainMapMetadata::parse_iso(&inverted.to_iso()).is_err());
		assert!(inverted.validate().is_err());
		assert!(GainMapMetadata::parse_iso(&trailing).is_err());
		assert_eq!(GainMapMetadata::parse_iso(&newer_writer), Ok(phone()));
		assert!(GainMapMetadata::parse_iso(&WORKED_EXAMPLE[..60]).is_err());
	}

	#[test]
	fn doubles_become_fractions_like_libavif() {
		let third = unsigned_fraction(1.0 / 3.0).unwrap();
		let large = 1000.0 * std::f64::consts::PI;
		let approximated = signed_fraction(large).unwrap();
		let max_denominator = (f64::from(i32::MAX) / large).floor();

		assert_eq!(signed_fraction(0.0), Some(signed(0, 1)));
		assert_eq!(unsigned_fraction(0.0), Some(unsigned(0, 1)));
		assert_eq!(signed_fraction(1.0 / 64.0), Some(signed(1, 64)));
		assert_eq!(unsigned_fraction(2.3), Some(unsigned(23, 10)));
		assert_eq!(signed_fraction(-0.5), Some(signed(-1, 2)));
		assert!((f64::from(third.numerator) / f64::from(third.denominator) - 1.0 / 3.0).abs() < 1e-9);
		assert!(f64::from(approximated.denominator) <= max_denominator);
		assert!(
			(f64::from(approximated.numerator) / f64::from(approximated.denominator) - large).abs() < 1e-6,
			"{approximated:?}"
		);
		assert_eq!(unsigned_fraction(-1.0), None);
		assert_eq!(signed_fraction(f64::NAN), None);
	}

	#[test]
	fn xmp_attributes_are_read() {
		let packet = xmp(
			"hdrgm:GainMapMin='0' hdrgm:GainMapMax='2.3' hdrgm:Gamma='1' hdrgm:OffsetSDR='0.015625' hdrgm:OffsetHDR='0.015625' hdrgm:HDRCapacityMin='0' hdrgm:HDRCapacityMax='2.3' hdrgm:BaseRenditionIsHDR='False'",
			"",
		);

		assert_eq!(GainMapMetadata::from_xmp(&packet), Ok(phone()));
	}

	#[test]
	fn xmp_sequences_give_three_channels_unless_they_are_equal() {
		let different = xmp(
			"hdrgm:HDRCapacityMax='2.3'",
			&[seq("GainMapMax", ["2", "2.3", "0.5"]), seq("Gamma", ["1", "1", "1"])].concat(),
		);
		let equal = xmp(
			"hdrgm:HDRCapacityMax='2.3'",
			&[
				seq("GainMapMax", ["2.3", "2.3", "2.3"]),
				seq("OffsetSDR", ["0.015625"; 3]),
			]
			.concat(),
		);

		let three = GainMapMetadata::from_xmp(&different).unwrap();

		assert_eq!(
			three.channels,
			[channel(signed(2, 1)), channel(signed(23, 10)), channel(signed(1, 2))]
		);
		assert_eq!(GainMapMetadata::from_xmp(&equal), Ok(phone()));
	}

	#[test]
	fn missing_xmp_fields_take_the_defaults() {
		let packet = xmp("hdrgm:GainMapMax='2.3' hdrgm:HDRCapacityMax='2.3'", "");
		let bare = xmp("", "");

		assert_eq!(GainMapMetadata::from_xmp(&packet), Ok(phone()));
		assert_eq!(
			GainMapMetadata::from_xmp(&bare),
			Ok(GainMapMetadata {
				alternate_headroom: unsigned(1, 1),
				channels: vec![channel(signed(1, 1))],
				..phone()
			})
		);
	}

	#[test]
	fn an_hdr_base_rendition_in_xmp_is_refused() {
		let packet = xmp(
			"hdrgm:GainMapMax='2.3' hdrgm:HDRCapacityMax='2.3' hdrgm:BaseRenditionIsHDR='True'",
			"",
		);
		let not_ultra_hdr = "<x:xmpmeta xmlns:x='adobe:ns:meta/'/>";

		assert!(
			GainMapMetadata::from_xmp(&packet)
				.unwrap_err()
				.contains("HDR base rendition is not supported")
		);
		assert!(GainMapMetadata::from_xmp(not_ultra_hdr).is_err());
	}
}
