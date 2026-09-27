use crate::metadata::xmp::{DC, PHOTOSHOP, Property, Value};

pub const IPTC_RESOURCE: u16 = 0x0404;

const DATASET_MARKER: u8 = 0x1C;
const UTF8_DECLARATION: &[u8] = b"\x1b%G";
const CP1252_HIGH: [char; 32] = [
	'\u{20AC}', '\u{81}', '\u{201A}', '\u{192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{2C6}', '\u{2030}',
	'\u{160}', '\u{2039}', '\u{152}', '\u{8D}', '\u{17D}', '\u{8F}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}',
	'\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}', '\u{2DC}', '\u{2122}', '\u{161}', '\u{203A}', '\u{153}', '\u{9D}',
	'\u{17E}', '\u{178}',
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrbResource<'a> {
	pub id: u16,
	pub data: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dataset<'a> {
	record: u8,
	id: u8,
	data: &'a [u8],
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IptcFields {
	pub object_name: Option<String>,
	pub keywords: Vec<String>,
	pub instructions: Option<String>,
	pub date_created: Option<String>,
	pub time_created: Option<String>,
	pub by_line: Vec<String>,
	pub city: Option<String>,
	pub province_state: Option<String>,
	pub country: Option<String>,
	pub headline: Option<String>,
	pub credit: Option<String>,
	pub source: Option<String>,
	pub copyright: Option<String>,
	pub caption: Option<String>,
}

pub fn resources(irb: &[u8]) -> Result<Vec<IrbResource<'_>>, String> {
	let mut found = Vec::new();
	let mut at = 0;

	while at < irb.len() {
		if irb.len() - at < 12 {
			return Err(format!(
				"{} stray bytes at the end of the resource block",
				irb.len() - at
			));
		}
		if &irb[at..at + 4] != b"8BIM" {
			return Err(format!("no '8BIM' signature at byte {at}"));
		}

		let id = u16::from_be_bytes([irb[at + 4], irb[at + 5]]);
		let name_len = (1 + usize::from(irb[at + 6]) + 1) & !1;
		let size_at = at + 6 + name_len;
		let size = irb
			.get(size_at..size_at + 4)
			.map(|bytes| u32::from_be_bytes(bytes.try_into().expect("4 bytes")))
			.and_then(|size| usize::try_from(size).ok())
			.ok_or("truncated resource size")?;
		let data = irb
			.get(size_at + 4..(size_at + 4).saturating_add(size))
			.ok_or("truncated resource data")?;

		found.push(IrbResource { id, data });
		at = size_at + 4 + size + (size & 1);
	}
	Ok(found)
}

pub fn iim(irb: &[u8]) -> Option<&[u8]> {
	resources(irb)
		.ok()?
		.into_iter()
		.find(|resource| resource.id == IPTC_RESOURCE)
		.map(|resource| resource.data)
}

fn datasets(iim: &[u8]) -> Result<Vec<Dataset<'_>>, String> {
	let mut found = Vec::new();
	let mut at = 0;

	while at < iim.len() {
		if iim[at] != DATASET_MARKER {
			if iim[at..].iter().all(|&byte| byte == 0) {
				break;
			}
			return Err(format!("unexpected byte 0x{:02X} at {at}", iim[at]));
		}

		let header = iim.get(at..at + 5).ok_or("truncated dataset header")?;
		let (record, id) = (header[1], header[2]);
		let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
		let (len, start) = if length & 0x8000 == 0 {
			(length, at + 5)
		} else {
			let width = length & 0x7FFF;
			if !(1..=4).contains(&width) {
				return Err("invalid extended dataset length".into());
			}
			let bytes = iim.get(at + 5..at + 5 + width).ok_or("truncated extended length")?;
			let len = bytes.iter().fold(0usize, |len, &byte| (len << 8) | usize::from(byte));
			(len, at + 5 + width)
		};

		let data = iim.get(start..start.saturating_add(len)).ok_or("truncated dataset")?;
		found.push(Dataset { record, id, data });
		at = start + len;
	}
	Ok(found)
}

pub fn fields(iim: &[u8]) -> Result<IptcFields, String> {
	let datasets = datasets(iim)?;
	let declared_utf8 = datasets
		.iter()
		.any(|dataset| dataset.record == 1 && dataset.id == 90 && dataset.data == UTF8_DECLARATION);
	let mut fields = IptcFields::default();

	for dataset in datasets.iter().filter(|dataset| dataset.record == 2) {
		let text = decode(dataset.data, declared_utf8);
		match dataset.id {
			5 => fields.object_name = Some(text),
			25 => fields.keywords.push(text),
			40 => fields.instructions = Some(text),
			55 => fields.date_created = Some(text),
			60 => fields.time_created = Some(text),
			80 => fields.by_line.push(text),
			90 => fields.city = Some(text),
			95 => fields.province_state = Some(text),
			101 => fields.country = Some(text),
			105 => fields.headline = Some(text),
			110 => fields.credit = Some(text),
			115 => fields.source = Some(text),
			116 => fields.copyright = Some(text),
			120 => fields.caption = Some(text),
			_ => {}
		}
	}
	Ok(fields)
}

fn decode(data: &[u8], declared_utf8: bool) -> String {
	let text = match std::str::from_utf8(data) {
		Ok(text) => text.to_owned(),
		Err(_) if declared_utf8 => String::from_utf8_lossy(data).into_owned(),
		Err(_) => data
			.iter()
			.map(|&byte| match byte {
				0x80..=0x9F => CP1252_HIGH[usize::from(byte - 0x80)],
				_ => char::from(byte),
			})
			.collect(),
	};
	text.trim_end_matches('\0').to_owned()
}

pub fn xmp_properties(fields: &IptcFields) -> Vec<Property> {
	let mut properties = Vec::new();
	let mut push = |namespace, prefix, name, value| {
		properties.push(Property {
			namespace,
			prefix,
			name,
			value,
		});
	};

	if !fields.keywords.is_empty() {
		push(DC, "dc", "subject", Value::Bag(fields.keywords.clone()));
	}
	if let Some(caption) = &fields.caption {
		push(DC, "dc", "description", Value::LangAlt(caption.clone()));
	}
	if !fields.by_line.is_empty() {
		push(DC, "dc", "creator", Value::Seq(fields.by_line.clone()));
	}
	if let Some(copyright) = &fields.copyright {
		push(DC, "dc", "rights", Value::LangAlt(copyright.clone()));
	}
	if let Some(title) = &fields.object_name {
		push(DC, "dc", "title", Value::LangAlt(title.clone()));
	}

	let texts = [
		("Headline", &fields.headline),
		("Credit", &fields.credit),
		("Source", &fields.source),
		("City", &fields.city),
		("State", &fields.province_state),
		("Country", &fields.country),
		("Instructions", &fields.instructions),
	];
	for (name, value) in texts {
		if let Some(value) = value {
			push(PHOTOSHOP, "photoshop", name, Value::Text(value.clone()));
		}
	}
	if let Some(date) = xmp_date(fields.date_created.as_deref(), fields.time_created.as_deref()) {
		push(PHOTOSHOP, "photoshop", "DateCreated", Value::Text(date));
	}
	properties
}

fn xmp_date(date: Option<&str>, time: Option<&str>) -> Option<String> {
	let date = date.filter(|date| date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()))?;
	let day = format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8]);

	let Some(time) = time.filter(|time| time.len() >= 6 && time.is_ascii()) else {
		return Some(day);
	};
	let clock = format!("{}:{}:{}", &time[..2], &time[2..4], &time[4..6]);
	let zone = match &time[6..] {
		zone if zone.len() == 5 => format!("{}:{}", &zone[..3], &zone[3..]),
		_ => String::new(),
	};
	Some(format!("{day}T{clock}{zone}"))
}
