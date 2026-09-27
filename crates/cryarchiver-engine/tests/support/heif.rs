pub fn boxed(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
	let mut bytes = u32::try_from(payload.len() + 8).unwrap().to_be_bytes().to_vec();
	bytes.extend_from_slice(&kind);
	bytes.extend_from_slice(payload);
	bytes
}

pub fn full_box(kind: [u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
	let mut body = ((u32::from(version) << 24) | flags).to_be_bytes().to_vec();
	body.extend_from_slice(payload);
	boxed(kind, &body)
}

pub fn ispe(width: u32, height: u32) -> Vec<u8> {
	full_box(*b"ispe", 0, 0, &[width.to_be_bytes(), height.to_be_bytes()].concat())
}

pub fn irot(quarters: u8) -> Vec<u8> {
	boxed(*b"irot", &[quarters])
}

pub fn imir(axis: u8) -> Vec<u8> {
	boxed(*b"imir", &[axis])
}

pub fn aux_c(urn: &str) -> Vec<u8> {
	full_box(*b"auxC", 0, 0, &[urn.as_bytes(), b"\0"].concat())
}

pub fn colr_nclx(primaries: u16, transfer: u16, matrix: u16, full_range: bool) -> Vec<u8> {
	let mut payload = b"nclx".to_vec();
	for value in [primaries, transfer, matrix] {
		payload.extend_from_slice(&value.to_be_bytes());
	}
	payload.push(if full_range { 0x80 } else { 0 });
	boxed(*b"colr", &payload)
}

pub fn colr_icc(icc: &[u8]) -> Vec<u8> {
	boxed(*b"colr", &[b"prof".as_slice(), icc].concat())
}

pub struct HeifItem {
	pub id: u16,
	pub kind: [u8; 4],
	pub content_type: Option<&'static str>,
	pub data: Vec<u8>,
	pub properties: Vec<(bool, Vec<u8>)>,
}

impl HeifItem {
	pub fn new(id: u16, kind: [u8; 4], data: Vec<u8>) -> Self {
		Self {
			id,
			kind,
			content_type: None,
			data,
			properties: Vec::new(),
		}
	}

	pub fn with(mut self, essential: bool, property: Vec<u8>) -> Self {
		self.properties.push((essential, property));
		self
	}
}

pub struct HeifFile {
	pub brand: [u8; 4],
	pub primary: u16,
	pub items: Vec<HeifItem>,
	pub references: Vec<([u8; 4], u16, Vec<u16>)>,
}

impl HeifFile {
	pub fn build(&self) -> Vec<u8> {
		let ftyp = boxed(
			*b"ftyp",
			&[self.brand.as_slice(), &[0; 4], b"mif1", &self.brand].concat(),
		);
		let unplaced = self.meta(0);
		let mdat_data_start = ftyp.len() + unplaced.len() + 8;
		let meta = self.meta(u32::try_from(mdat_data_start).unwrap());
		let data: Vec<u8> = self.items.iter().flat_map(|item| item.data.clone()).collect();

		[ftyp, meta, boxed(*b"mdat", &data)].concat()
	}

	fn meta(&self, mdat_data_start: u32) -> Vec<u8> {
		let hdlr = full_box(*b"hdlr", 0, 0, &[&[0u8; 4][..], b"pict", &[0; 13]].concat());
		let pitm = full_box(*b"pitm", 0, 0, &self.primary.to_be_bytes());

		let mut infes = u16::try_from(self.items.len()).unwrap().to_be_bytes().to_vec();
		for item in &self.items {
			let mut payload = [&item.id.to_be_bytes()[..], &[0, 0], &item.kind, b"\0"].concat();
			if let Some(content_type) = item.content_type {
				payload.extend_from_slice(content_type.as_bytes());
				payload.push(0);
			}
			infes.extend(full_box(*b"infe", 2, 0, &payload));
		}
		let iinf = full_box(*b"iinf", 0, 0, &infes);

		let mut iloc = vec![0x44, 0x00];
		iloc.extend_from_slice(&u16::try_from(self.items.len()).unwrap().to_be_bytes());
		let mut offset = mdat_data_start;
		for item in &self.items {
			let length = u32::try_from(item.data.len()).unwrap();
			iloc.extend_from_slice(&item.id.to_be_bytes());
			iloc.extend_from_slice(&[0, 0, 0, 1]);
			iloc.extend_from_slice(&offset.to_be_bytes());
			iloc.extend_from_slice(&length.to_be_bytes());
			offset += length;
		}
		let iloc = full_box(*b"iloc", 0, 0, &iloc);

		let mut ipco = Vec::new();
		let mut ipma = u32::try_from(self.items.len()).unwrap().to_be_bytes().to_vec();
		let mut next_index = 1u8;
		for item in &self.items {
			ipma.extend_from_slice(&item.id.to_be_bytes());
			ipma.push(u8::try_from(item.properties.len()).unwrap());
			for (essential, property) in &item.properties {
				ipco.extend_from_slice(property);
				ipma.push(if *essential { 0x80 | next_index } else { next_index });
				next_index += 1;
			}
		}
		let iprp = boxed(
			*b"iprp",
			&[boxed(*b"ipco", &ipco), full_box(*b"ipma", 0, 0, &ipma)].concat(),
		);

		let mut children = [hdlr, pitm, iloc, iinf].concat();
		if !self.references.is_empty() {
			let mut refs = Vec::new();
			for (kind, from, to) in &self.references {
				let mut payload = from.to_be_bytes().to_vec();
				payload.extend_from_slice(&u16::try_from(to.len()).unwrap().to_be_bytes());
				for id in to {
					payload.extend_from_slice(&id.to_be_bytes());
				}
				refs.extend(boxed(*kind, &payload));
			}
			children.extend(full_box(*b"iref", 0, 0, &refs));
		}
		children.extend(iprp);
		full_box(*b"meta", 0, 0, &children)
	}
}

pub fn exif_item(tiff: &[u8]) -> Vec<u8> {
	[&[0u8; 4][..], tiff].concat()
}

fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
	let starts: Vec<usize> = stream
		.windows(3)
		.enumerate()
		.filter(|(_, window)| *window == [0, 0, 1])
		.map(|(at, _)| at + 3)
		.collect();

	starts
		.iter()
		.enumerate()
		.map(|(index, &start)| {
			let end = starts.get(index + 1).map_or(stream.len(), |next| next - 3);
			let mut unit = &stream[start..end];
			while let [rest @ .., 0] = unit {
				unit = rest;
			}
			unit
		})
		.collect()
}

fn without_emulation_prevention(unit: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(unit.len());
	let mut zeros = 0;
	for &byte in unit {
		if zeros >= 2 && byte == 3 {
			zeros = 0;
			continue;
		}
		out.push(byte);
		zeros = if byte == 0 { zeros + 1 } else { 0 };
	}
	out
}

fn nal_type(unit: &[u8]) -> u8 {
	(unit[0] >> 1) & 0x3F
}

pub fn hevc_item(dir: &std::path::Path, width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
	let path = dir.join(format!("tile-{width}x{height}.hevc"));
	let source = format!("testsrc2=s={width}x{height}");
	super::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		source.as_str(),
		"-frames:v",
		"1",
		"-pix_fmt",
		"yuv420p",
		"-c:v",
		"libx265",
		"-x265-params",
		"keyint=1:log-level=none",
		"-f",
		"hevc",
		path.to_str().unwrap(),
	]);
	let stream = std::fs::read(path).unwrap();
	let units = nal_units(&stream);
	let find = |kind: u8| *units.iter().find(|unit| nal_type(unit) == kind).unwrap();
	let (vps, sps, pps) = (find(32), find(33), find(34));

	let profile_tier_level = &without_emulation_prevention(sps)[3..15];
	let mut config = vec![1];
	config.extend_from_slice(profile_tier_level);
	config.extend_from_slice(&[0xF0, 0x00, 0xFC, 0xFD, 0xF8, 0xF8, 0x00, 0x00, 0x0F, 3]);
	for (kind, unit) in [(32u8, vps), (33, sps), (34, pps)] {
		config.push(0x80 | kind);
		config.extend_from_slice(&1u16.to_be_bytes());
		config.extend_from_slice(&u16::try_from(unit.len()).unwrap().to_be_bytes());
		config.extend_from_slice(unit);
	}

	let mut samples = Vec::new();
	for unit in units.iter().filter(|unit| nal_type(unit) < 32) {
		samples.extend_from_slice(&u32::try_from(unit.len()).unwrap().to_be_bytes());
		samples.extend_from_slice(unit);
	}
	(boxed(*b"hvcC", &config), samples)
}

pub fn heic_still(
	dir: &std::path::Path,
	width: u32,
	height: u32,
	transforms: Vec<Vec<u8>>,
	exif: Option<&[u8]>,
) -> Vec<u8> {
	let (config, samples) = hevc_item(dir, width, height);
	let mut image = HeifItem::new(1, *b"hvc1", samples)
		.with(true, config)
		.with(false, ispe(width, height));
	for transform in transforms {
		image = image.with(true, transform);
	}

	let mut items = vec![image];
	let mut references = Vec::new();
	if let Some(tiff) = exif {
		items.push(HeifItem::new(2, *b"Exif", exif_item(tiff)));
		references.push((*b"cdsc", 2, vec![1]));
	}
	HeifFile {
		brand: *b"heic",
		primary: 1,
		items,
		references,
	}
	.build()
}

pub fn heic_grid(dir: &std::path::Path, tile_width: u32, tile_height: u32) -> Vec<u8> {
	let (config, samples) = hevc_item(dir, tile_width, tile_height);
	let mut items: Vec<HeifItem> = (1..=4)
		.map(|id| {
			HeifItem::new(id, *b"hvc1", samples.clone())
				.with(true, config.clone())
				.with(false, ispe(tile_width, tile_height))
		})
		.collect();

	let (width, height) = (tile_width * 2, tile_height * 2);
	let mut grid = vec![0, 0, 1, 1];
	grid.extend_from_slice(&u16::try_from(width).unwrap().to_be_bytes());
	grid.extend_from_slice(&u16::try_from(height).unwrap().to_be_bytes());
	items.push(HeifItem::new(5, *b"grid", grid).with(false, ispe(width, height)));

	HeifFile {
		brand: *b"heic",
		primary: 5,
		items,
		references: vec![(*b"dimg", 5, vec![1, 2, 3, 4])],
	}
	.build()
}
