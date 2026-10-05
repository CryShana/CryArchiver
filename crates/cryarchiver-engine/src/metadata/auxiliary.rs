use crate::container::isobmff::Meta;
use crate::metadata::{AuxKind, xmp};

const MP_ENTRY_TAG: u16 = 0xB002;
const MP_PRIMARY: u32 = 0x03_0000;
const MP_THUMBNAIL_CLASS: u32 = 0x01_0000;
const MP_PANORAMA: u32 = 0x02_0001;
const MP_DISPARITY: u32 = 0x02_0002;
const MP_MULTI_ANGLE: u32 = 0x02_0003;
pub(super) const HDR_GAIN_MAP: &str = "http://ns.adobe.com/hdr-gain-map/1.0/";
const GOOGLE_DEPTH: &str = "http://ns.google.com/photos/1.0/depthmap/";
const GOOGLE_IMAGE: &str = "http://ns.google.com/photos/1.0/image/";
const GOOGLE_ITEM: &str = "http://ns.google.com/photos/1.0/container/item/";
const GOOGLE_CAMERA: &str = "http://ns.google.com/photos/1.0/camera/";

pub(crate) struct MpEntry {
	pub kind: u32,
	pub size: u32,
	pub offset: u32,
}

pub fn from_mpf(mpf: &[u8]) -> Result<Vec<AuxKind>, String> {
	Ok(mp_entries(mpf)?.iter().filter_map(MpEntry::aux).collect())
}

pub(crate) fn mpf_entry_at(mpf: &[u8], offset: usize) -> Option<AuxKind> {
	mp_entries(mpf)
		.ok()?
		.iter()
		.find(|entry| usize::try_from(entry.offset) == Ok(offset))
		.and_then(MpEntry::aux)
}

impl MpEntry {
	fn aux(&self) -> Option<AuxKind> {
		let kind = self.kind & 0x00FF_FFFF;
		Some(match kind {
			MP_PRIMARY => return None,
			_ if kind & 0xFF_FF00 == MP_THUMBNAIL_CLASS => return None,
			MP_PANORAMA => AuxKind::Other("MPF panorama frame".into()),
			MP_DISPARITY => AuxKind::Depth("MPF disparity image".into()),
			MP_MULTI_ANGLE => AuxKind::Other("MPF multi-angle frame".into()),
			other => AuxKind::Other(format!("MPF image type 0x{other:06X}")),
		})
	}
}

pub fn mpf_images_end(mpf: &[u8]) -> Option<usize> {
	mp_entries(mpf)
		.ok()?
		.iter()
		.filter(|entry| entry.offset != 0)
		.filter_map(|entry| usize::try_from(u64::from(entry.offset) + u64::from(entry.size)).ok())
		.max()
}

pub(crate) fn mp_entries(mpf: &[u8]) -> Result<Vec<MpEntry>, String> {
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

	Ok((0..len / 16)
		.map(|image| start + image * 16)
		.map(|entry| MpEntry {
			kind: u32_at(entry).unwrap_or(0),
			size: u32_at(entry + 4).unwrap_or(0),
			offset: u32_at(entry + 8).unwrap_or(0),
		})
		.collect())
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
	let mut motion_photo = ["MicroVideo", "MotionPhoto"].iter().any(|name| {
		xmp::values(xmp, GOOGLE_CAMERA, name)
			.iter()
			.any(|value| value.trim() == "1")
	});
	for semantic in xmp::values(xmp, GOOGLE_ITEM, "Semantic") {
		match semantic.as_str() {
			"Primary" => {}
			"GainMap" => gain_map_item = true,
			"MotionPhoto" => motion_photo = true,
			"Depth" => found.push(AuxKind::Depth("container item".into())),
			other => found.push(AuxKind::Other(format!("container item '{other}'"))),
		}
	}
	if gain_map_item && !found.iter().any(|aux| matches!(aux, AuxKind::GainMap(_))) {
		found.push(AuxKind::GainMap("container item".into()));
	}
	if motion_photo {
		found.push(AuxKind::MotionVideo("Google motion photo".into()));
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
