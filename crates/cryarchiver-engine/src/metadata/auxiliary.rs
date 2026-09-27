use crate::container::isobmff::Meta;
use crate::metadata::{AuxKind, xmp};

const MP_ENTRY_TAG: u16 = 0xB002;
const MP_PRIMARY: u32 = 0x03_0000;
const MP_THUMBNAIL_CLASS: u32 = 0x01_0000;
const MP_PANORAMA: u32 = 0x02_0001;
const MP_DISPARITY: u32 = 0x02_0002;
const MP_MULTI_ANGLE: u32 = 0x02_0003;
const HDR_GAIN_MAP: &str = "http://ns.adobe.com/hdr-gain-map/1.0/";
const GOOGLE_DEPTH: &str = "http://ns.google.com/photos/1.0/depthmap/";
const GOOGLE_IMAGE: &str = "http://ns.google.com/photos/1.0/image/";
const GOOGLE_ITEM: &str = "http://ns.google.com/photos/1.0/container/item/";

pub fn from_mpf(mpf: &[u8]) -> Result<Vec<AuxKind>, String> {
	let little = match mpf.get(..4) {
		Some(b"II*\0") => true,
		Some(b"MM\0*") => false,
		_ => return Err("no TIFF header".into()),
	};
	let u16_at = |at: usize| {
		let bytes: [u8; 2] = mpf.get(at..at + 2)?.try_into().ok()?;
		Some(if little {
			u16::from_le_bytes(bytes)
		} else {
			u16::from_be_bytes(bytes)
		})
	};
	let u32_at = |at: usize| {
		let bytes: [u8; 4] = mpf.get(at..at + 4)?.try_into().ok()?;
		Some(if little {
			u32::from_le_bytes(bytes)
		} else {
			u32::from_be_bytes(bytes)
		})
	};
	let offset = |value: u32| usize::try_from(value).ok();

	let ifd = u32_at(4).and_then(offset).ok_or("no first IFD")?;
	let count = usize::from(u16_at(ifd).ok_or("the IFD is outside the block")?);
	let entry = (0..count)
		.map(|index| ifd + 2 + index * 12)
		.find(|&entry| u16_at(entry) == Some(MP_ENTRY_TAG))
		.ok_or("no MPEntry tag")?;
	let len = u32_at(entry + 4).and_then(offset).ok_or("truncated MPEntry tag")?;
	let start = u32_at(entry + 8).and_then(offset).ok_or("truncated MPEntry tag")?;
	if start.checked_add(len).is_none_or(|end| end > mpf.len()) {
		return Err("the MPEntry table is outside the block".into());
	}

	let mut found = Vec::new();
	for image in 0..len / 16 {
		let kind = u32_at(start + image * 16).unwrap_or(0) & 0x00FF_FFFF;
		let aux = match kind {
			MP_PRIMARY => continue,
			_ if kind & 0xFF_FF00 == MP_THUMBNAIL_CLASS => continue,
			MP_PANORAMA => AuxKind::Other("MPF panorama frame".into()),
			MP_DISPARITY => AuxKind::Depth("MPF disparity image".into()),
			MP_MULTI_ANGLE => AuxKind::Other("MPF multi-angle frame".into()),
			other => AuxKind::Other(format!("MPF image type 0x{other:06X}")),
		};
		found.push(aux);
	}
	Ok(found)
}

pub fn from_xmp(xmp: &str) -> Vec<AuxKind> {
	let mut found = Vec::new();
	if xmp::uses_namespace(xmp, HDR_GAIN_MAP) {
		found.push(AuxKind::GainMap("Ultra HDR".into()));
	}
	if xmp::uses_namespace(xmp, GOOGLE_DEPTH) {
		found.push(AuxKind::Depth("Google depth map".into()));
	}
	if xmp::uses_namespace(xmp, GOOGLE_IMAGE) {
		found.push(AuxKind::Other("Google embedded image".into()));
	}
	let mut gain_map_item = false;
	for semantic in xmp::values(xmp, GOOGLE_ITEM, "Semantic") {
		match semantic.as_str() {
			"Primary" => {}
			"GainMap" => gain_map_item = true,
			"Depth" => found.push(AuxKind::Depth("container item".into())),
			other => found.push(AuxKind::Other(format!("container item '{other}'"))),
		}
	}
	if gain_map_item && !found.iter().any(|aux| matches!(aux, AuxKind::GainMap(_))) {
		found.push(AuxKind::GainMap("container item".into()));
	}
	found
}

pub fn from_heif(meta: &Meta) -> Vec<AuxKind> {
	let mut found = Vec::new();
	if meta.has_item_kind(*b"tmap") {
		found.push(AuxKind::GainMap("tmap item".into()));
	}

	for urn in meta.auxiliary_urns() {
		let lower = urn.to_ascii_lowercase();
		let aux = if lower.contains("gainmap") {
			AuxKind::GainMap(urn.to_owned())
		} else if lower.contains("depth") || lower.contains("disparity") || urn == "urn:mpeg:hevc:2015:auxid:2" {
			AuxKind::Depth(urn.to_owned())
		} else {
			AuxKind::Other(urn.to_owned())
		};
		found.push(aux);
	}
	found
}
