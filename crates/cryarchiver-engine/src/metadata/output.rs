use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::metadata::xmp::{self, CRYARCHIVER, Property, Value, XmpEdit};
use crate::metadata::{SourceMetadata, exif, iptc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataMode {
	Full,
	IccOnly,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputMetadata {
	pub exif: Option<Vec<u8>>,
	pub xmp: Option<Vec<u8>>,
	pub warnings: Vec<String>,
}

pub fn build_output(source: &SourceMetadata, mode: MetadataMode, width: u32, height: u32) -> OutputMetadata {
	let mut output = OutputMetadata::default();
	if mode == MetadataMode::IccOnly {
		return output;
	}

	if let Some(mut block) = source.exif.clone() {
		match exif::patch(&mut block, width, height) {
			Ok(patch) => output
				.warnings
				.extend(patch.skipped.into_iter().map(|skipped| format!("EXIF: {skipped}"))),
			Err(err) => output.warnings.push(format!("EXIF copied without changes: {err}")),
		}
		output.exif = Some(block);
	}

	output.xmp = xmp_packet(source, width, height, &mut output.warnings);
	output
}

fn xmp_packet(source: &SourceMetadata, width: u32, height: u32, warnings: &mut Vec<String>) -> Option<Vec<u8>> {
	let edit = XmpEdit {
		dimensions: Some((width, height)),
		add: added_properties(source, warnings),
		extended: source.xmp_extended.clone(),
	};

	let Some(raw) = source.xmp.as_deref() else {
		return (!edit.add.is_empty() || !edit.extended.is_empty()).then(|| xmp::create(&edit).into_bytes());
	};
	let Ok(text) = std::str::from_utf8(raw) else {
		warnings.push("XMP copied without changes: it is not UTF-8".into());
		return Some(raw.to_vec());
	};
	match xmp::patch(text, &edit) {
		Ok(patched) => Some(patched.into_bytes()),
		Err(err) => {
			warnings.push(format!("XMP copied without changes: {err}"));
			Some(raw.to_vec())
		}
	}
}

fn added_properties(source: &SourceMetadata, warnings: &mut Vec<String>) -> Vec<Property> {
	let mut added = Vec::new();

	let iim = source
		.iim
		.as_deref()
		.or_else(|| source.irb.as_deref().and_then(iptc::iim));
	if let Some(iim) = iim {
		match iptc::fields(iim) {
			Ok(fields) => added.extend(iptc::xmp_properties(&fields)),
			Err(err) => warnings.push(format!("IPTC not mapped to XMP: {err}")),
		}
	}
	if let Some(block) = source.irb.as_deref().or(source.iim.as_deref()) {
		added.push(cryarchiver("IPTCIIM", Value::Text(STANDARD.encode(block))));
	}

	if !source.comments.is_empty() {
		added.push(cryarchiver("Comment", Value::Text(source.comments.join("\n"))));
	}
	if !source.text.is_empty() {
		let pairs = source
			.text
			.iter()
			.map(|(key, value)| format!("{key}={value}"))
			.collect();
		added.push(cryarchiver("SourceText", Value::Bag(pairs)));
	}
	added
}

fn cryarchiver(name: &'static str, value: Value) -> Property {
	Property {
		namespace: CRYARCHIVER,
		prefix: "cryarchiver",
		name,
		value,
	}
}
