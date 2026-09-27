pub const ALPHA_URNS: &[&str] = &[
	"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha",
	"urn:mpeg:hevc:2015:auxid:1",
];
pub const XMP_CONTENT_TYPE: &str = "application/rdf+xml";

// EXIF orientations 1..=8 as 2×2 matrices acting on (x, y) with y pointing down.
const EXIF_MATRICES: [[i8; 4]; 8] = [
	[1, 0, 0, 1],
	[-1, 0, 0, 1],
	[-1, 0, 0, -1],
	[1, 0, 0, -1],
	[0, 1, 1, 0],
	[0, -1, 1, 0],
	[0, -1, -1, 0],
	[0, 1, -1, 0],
];
const ROTATE_ANTICLOCKWISE: [i8; 4] = [0, 1, -1, 0];
const FLIP_TOP_BOTTOM: [i8; 4] = [1, 0, 0, -1];
const FLIP_LEFT_RIGHT: [i8; 4] = [-1, 0, 0, 1];
const IDENTITY: [i8; 4] = [1, 0, 0, 1];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct IsobmffError(String);

impl IsobmffError {
	fn new(message: impl Into<String>) -> Self {
		Self(message.into())
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxRef<'a> {
	pub kind: [u8; 4],
	pub body: &'a [u8],
	pub start: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nclx {
	pub primaries: u16,
	pub transfer: u16,
	pub matrix: u16,
	pub full_range: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Property {
	Rotation(u8),
	Mirror(u8),
	Auxiliary(String),
	Nclx(Nclx),
	Icc(Vec<u8>),
	Size { width: u32, height: u32 },
	PixelDepths(Vec<u8>),
	Av1Config(Vec<u8>),
	Other([u8; 4]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertyLink {
	pub essential: bool,
	pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
	pub id: u32,
	pub kind: [u8; 4],
	pub content_type: Option<String>,
	pub properties: Vec<PropertyLink>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
	pub offset: u64,
	pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
	pub item: u32,
	pub construction: u8,
	pub extents: Vec<Extent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
	pub kind: [u8; 4],
	pub from: u32,
	pub to: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
	pub handler: Option<[u8; 4]>,
	pub primary: Option<u32>,
	pub items: Vec<Item>,
	pub properties: Vec<Property>,
	pub references: Vec<Reference>,
	pub locations: Vec<Location>,
	pub idat: Vec<u8>,
}

struct Reader<'a> {
	data: &'a [u8],
	at: usize,
}

impl<'a> Reader<'a> {
	fn new(data: &'a [u8]) -> Self {
		Self { data, at: 0 }
	}

	fn take(&mut self, len: usize) -> Result<&'a [u8], IsobmffError> {
		let end = self
			.at
			.checked_add(len)
			.filter(|end| *end <= self.data.len())
			.ok_or_else(|| IsobmffError::new("truncated box"))?;
		let bytes = &self.data[self.at..end];
		self.at = end;
		Ok(bytes)
	}

	fn u8(&mut self) -> Result<u8, IsobmffError> {
		Ok(self.take(1)?[0])
	}

	fn u16(&mut self) -> Result<u16, IsobmffError> {
		Ok(u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes")))
	}

	fn u32(&mut self) -> Result<u32, IsobmffError> {
		Ok(u32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")))
	}

	fn uint(&mut self, len: usize) -> Result<u64, IsobmffError> {
		Ok(self
			.take(len)?
			.iter()
			.fold(0u64, |value, &byte| (value << 8) | u64::from(byte)))
	}

	fn id(&mut self, wide: bool) -> Result<u32, IsobmffError> {
		if wide { self.u32() } else { self.u16().map(u32::from) }
	}

	fn fourcc(&mut self) -> Result<[u8; 4], IsobmffError> {
		Ok(self.take(4)?.try_into().expect("4 bytes"))
	}

	fn c_string(&mut self) -> String {
		let rest = &self.data[self.at..];
		let len = rest.iter().position(|&byte| byte == 0).unwrap_or(rest.len());
		self.at += (len + 1).min(rest.len());
		String::from_utf8_lossy(&rest[..len]).into_owned()
	}

	fn rest(&self) -> &'a [u8] {
		&self.data[self.at..]
	}

	fn remaining(&self) -> usize {
		self.data.len() - self.at
	}
}

pub fn boxes(data: &[u8]) -> Result<Vec<BoxRef<'_>>, IsobmffError> {
	let mut reader = Reader::new(data);
	let mut found = Vec::new();

	while reader.remaining() >= 8 {
		let start = reader.at;
		let size = reader.u32()?;
		let kind = reader.fourcc()?;
		let (header, total) = match size {
			0 => (8, (data.len() - start) as u64),
			1 => (16, reader.uint(8)?),
			size => (8, u64::from(size)),
		};

		let body_len = usize::try_from(total)
			.ok()
			.and_then(|total| total.checked_sub(header))
			.ok_or_else(|| IsobmffError::new(format!("invalid size {total} for the '{}' box", name(kind))))?;
		let body = reader
			.take(body_len)
			.map_err(|_| IsobmffError::new(format!("truncated: the '{}' box runs past the end", name(kind))))?;
		found.push(BoxRef { kind, body, start });
	}
	Ok(found)
}

pub fn find_box(data: &[u8], kind: [u8; 4]) -> Result<Option<BoxRef<'_>>, IsobmffError> {
	Ok(boxes(data)?.into_iter().find(|found| found.kind == kind))
}

pub fn brands(file: &[u8]) -> Option<Vec<[u8; 4]>> {
	if file.get(4..8)? != b"ftyp" {
		return None;
	}

	let size = usize::try_from(u32::from_be_bytes(file.get(..4)?.try_into().ok()?)).ok()?;
	let body = file.get(8..size)?;
	let major: [u8; 4] = body.get(..4)?.try_into().ok()?;
	let compatible = body.get(8..)?.as_chunks::<4>().0.iter().copied();
	Some(std::iter::once(major).chain(compatible).collect())
}

pub fn parse_meta(file: &[u8]) -> Result<Meta, IsobmffError> {
	let meta = find_box(file, *b"meta")?.ok_or_else(|| IsobmffError::new("no 'meta' box"))?;
	let children = meta
		.body
		.get(4..)
		.ok_or_else(|| IsobmffError::new("truncated 'meta' box"))?;
	let mut parsed = Meta::default();

	for child in boxes(children)? {
		let mut reader = Reader::new(child.body);
		match &child.kind {
			b"hdlr" => {
				reader.take(8)?;
				parsed.handler = Some(reader.fourcc()?);
			}
			b"pitm" => {
				let version = reader.u32()? >> 24;
				parsed.primary = Some(reader.id(version != 0)?);
			}
			b"iinf" => parsed.items = item_infos(&mut reader)?,
			b"iref" => parsed.references = references(&mut reader)?,
			b"iloc" => parsed.locations = locations(&mut reader)?,
			b"idat" => parsed.idat = child.body.to_vec(),
			b"iprp" => parsed.properties = item_properties(child.body, &mut parsed.items)?,
			_ => {}
		}
	}
	Ok(parsed)
}

fn item_infos(reader: &mut Reader<'_>) -> Result<Vec<Item>, IsobmffError> {
	let version = reader.u32()? >> 24;
	reader.id(version != 0)?;

	let mut items = Vec::new();
	for entry in boxes(reader.rest())?.into_iter().filter(|entry| &entry.kind == b"infe") {
		let mut infe = Reader::new(entry.body);
		let version = infe.u32()? >> 24;
		if version < 2 {
			continue;
		}

		let id = infe.id(version != 2)?;
		infe.u16()?;
		let kind = infe.fourcc()?;
		infe.c_string();
		let content_type = (&kind == b"mime").then(|| infe.c_string());
		items.push(Item {
			id,
			kind,
			content_type,
			properties: Vec::new(),
		});
	}
	Ok(items)
}

fn references(reader: &mut Reader<'_>) -> Result<Vec<Reference>, IsobmffError> {
	let wide = reader.u32()? >> 24 != 0;

	let mut found = Vec::new();
	for entry in boxes(reader.rest())? {
		let mut refs = Reader::new(entry.body);
		let from = refs.id(wide)?;
		let count = refs.u16()?;
		let to = (0..count).map(|_| refs.id(wide)).collect::<Result<_, _>>()?;
		found.push(Reference {
			kind: entry.kind,
			from,
			to,
		});
	}
	Ok(found)
}

fn locations(reader: &mut Reader<'_>) -> Result<Vec<Location>, IsobmffError> {
	let version = reader.u32()? >> 24;
	let sizes = reader.u8()?;
	let more_sizes = reader.u8()?;
	let (offset_size, length_size) = (usize::from(sizes >> 4), usize::from(sizes & 0x0F));
	let base_size = usize::from(more_sizes >> 4);
	let index_size = if version == 0 {
		0
	} else {
		usize::from(more_sizes & 0x0F)
	};
	let count = if version < 2 {
		u32::from(reader.u16()?)
	} else {
		reader.u32()?
	};

	let mut found = Vec::new();
	for _ in 0..count {
		let item = reader.id(version >= 2)?;
		let construction = if version == 0 {
			0
		} else {
			u8::try_from(reader.u16()? & 0x0F).expect("4 bits")
		};
		reader.u16()?;
		let base = reader.uint(base_size)?;

		let mut extents = Vec::new();
		for _ in 0..reader.u16()? {
			reader.uint(index_size)?;
			let offset = base.saturating_add(reader.uint(offset_size)?);
			let length = reader.uint(length_size)?;
			extents.push(Extent { offset, length });
		}
		found.push(Location {
			item,
			construction,
			extents,
		});
	}
	Ok(found)
}

fn item_properties(iprp: &[u8], items: &mut [Item]) -> Result<Vec<Property>, IsobmffError> {
	let mut properties = Vec::new();

	for child in boxes(iprp)? {
		match &child.kind {
			b"ipco" => {
				for property in boxes(child.body)? {
					properties.push(parse_property(property)?);
				}
			}
			b"ipma" => associate(child.body, items)?,
			_ => {}
		}
	}
	Ok(properties)
}

fn parse_property(property: BoxRef<'_>) -> Result<Property, IsobmffError> {
	let mut reader = Reader::new(property.body);
	Ok(match &property.kind {
		b"irot" => Property::Rotation(reader.u8()? & 0x03),
		b"imir" => Property::Mirror(reader.u8()? & 0x01),
		b"auxC" => {
			reader.u32()?;
			Property::Auxiliary(reader.c_string())
		}
		b"colr" => match &reader.fourcc()? {
			b"nclx" => Property::Nclx(Nclx {
				primaries: reader.u16()?,
				transfer: reader.u16()?,
				matrix: reader.u16()?,
				full_range: reader.u8()? & 0x80 != 0,
			}),
			b"prof" | b"rICC" => Property::Icc(reader.rest().to_vec()),
			_ => Property::Other(property.kind),
		},
		b"ispe" => {
			reader.u32()?;
			Property::Size {
				width: reader.u32()?,
				height: reader.u32()?,
			}
		}
		b"pixi" => {
			reader.u32()?;
			let channels = usize::from(reader.u8()?);
			Property::PixelDepths(reader.take(channels)?.to_vec())
		}
		b"av1C" => Property::Av1Config(property.body.to_vec()),
		_ => Property::Other(property.kind),
	})
}

fn associate(ipma: &[u8], items: &mut [Item]) -> Result<(), IsobmffError> {
	let mut reader = Reader::new(ipma);
	let version_and_flags = reader.u32()?;
	let (version, wide_index) = (version_and_flags >> 24, version_and_flags & 1 == 1);

	for _ in 0..reader.u32()? {
		let id = reader.id(version >= 1)?;
		let mut links = Vec::new();
		for _ in 0..reader.u8()? {
			let link = if wide_index {
				let value = reader.u16()?;
				PropertyLink {
					essential: value & 0x8000 != 0,
					index: usize::from(value & 0x7FFF),
				}
			} else {
				let value = reader.u8()?;
				PropertyLink {
					essential: value & 0x80 != 0,
					index: usize::from(value & 0x7F),
				}
			};
			links.push(link);
		}

		if let Some(item) = items.iter_mut().find(|item| item.id == id) {
			item.properties.extend(links);
		}
	}
	Ok(())
}

impl Meta {
	pub fn item(&self, id: u32) -> Option<&Item> {
		self.items.iter().find(|item| item.id == id)
	}

	pub fn primary_item(&self) -> Option<&Item> {
		self.primary.and_then(|id| self.item(id))
	}

	pub fn properties_of(&self, id: u32) -> impl Iterator<Item = &Property> {
		self.item(id)
			.into_iter()
			.flat_map(|item| &item.properties)
			.filter_map(|link| link.index.checked_sub(1).and_then(|index| self.properties.get(index)))
	}

	pub fn has_item_kind(&self, kind: [u8; 4]) -> bool {
		self.items.iter().any(|item| item.kind == kind)
	}

	pub fn item_data(&self, file: &[u8], id: u32) -> Result<Vec<u8>, IsobmffError> {
		let location = self
			.locations
			.iter()
			.find(|location| location.item == id)
			.ok_or_else(|| IsobmffError::new(format!("item {id} has no location")))?;
		let source: &[u8] = match location.construction {
			0 => file,
			1 => &self.idat,
			other => return Err(IsobmffError::new(format!("unsupported construction method {other}"))),
		};

		let mut data = Vec::new();
		for extent in &location.extents {
			let bytes = extent_bytes(source, *extent)
				.ok_or_else(|| IsobmffError::new(format!("item {id} lies outside the file")))?;
			data.extend_from_slice(bytes);
		}
		Ok(data)
	}

	pub fn exif_tiff(&self, file: &[u8]) -> Option<Result<Vec<u8>, IsobmffError>> {
		let item = self.items.iter().find(|item| &item.kind == b"Exif")?;
		Some(self.item_data(file, item.id).and_then(|data| {
			let offset = data
				.get(..4)
				.map(|prefix| u32::from_be_bytes(prefix.try_into().expect("4 bytes")))
				.ok_or_else(|| IsobmffError::new("short Exif item"))?;
			usize::try_from(offset)
				.ok()
				.and_then(|offset| data.get(offset.checked_add(4)?..))
				.map(<[u8]>::to_vec)
				.ok_or_else(|| IsobmffError::new("the Exif item's TIFF offset is outside the item"))
		}))
	}

	pub fn xmp(&self, file: &[u8]) -> Option<Result<Vec<u8>, IsobmffError>> {
		let item = self
			.items
			.iter()
			.find(|item| &item.kind == b"mime" && item.content_type.as_deref() == Some(XMP_CONTENT_TYPE))?;
		Some(self.item_data(file, item.id))
	}

	pub fn primary_size(&self) -> Option<(u32, u32)> {
		self.properties_of(self.primary?).find_map(|property| match property {
			Property::Size { width, height } => Some((*width, *height)),
			_ => None,
		})
	}

	pub fn primary_icc(&self) -> Option<&[u8]> {
		self.properties_of(self.primary?).find_map(|property| match property {
			Property::Icc(icc) => Some(icc.as_slice()),
			_ => None,
		})
	}

	pub fn primary_nclx(&self) -> Option<Nclx> {
		self.properties_of(self.primary?).find_map(|property| match property {
			Property::Nclx(nclx) => Some(*nclx),
			_ => None,
		})
	}

	pub fn primary_orientation(&self) -> u16 {
		let Some(primary) = self.primary else {
			return 1;
		};

		let transform = self.properties_of(primary).fold(IDENTITY, |transform, property| {
			let step = match property {
				Property::Rotation(quarters) => (0..*quarters).fold(IDENTITY, |m, _| multiply(ROTATE_ANTICLOCKWISE, m)),
				Property::Mirror(0) => FLIP_TOP_BOTTOM,
				Property::Mirror(_) => FLIP_LEFT_RIGHT,
				_ => return transform,
			};
			multiply(step, transform)
		});
		EXIF_MATRICES
			.iter()
			.position(|matrix| *matrix == transform)
			.and_then(|index| u16::try_from(index + 1).ok())
			.unwrap_or(1)
	}

	pub fn alpha_item(&self) -> Option<u32> {
		let primary = self.primary?;
		self.references
			.iter()
			.filter(|reference| &reference.kind == b"auxl" && reference.to.contains(&primary))
			.map(|reference| reference.from)
			.find(|&from| {
				self.properties_of(from)
					.any(|property| matches!(property, Property::Auxiliary(urn) if ALPHA_URNS.contains(&urn.as_str())))
			})
	}

	pub fn premultiplied(&self) -> bool {
		self.references.iter().any(|reference| &reference.kind == b"prem")
	}

	pub fn auxiliary_urns(&self) -> Vec<&str> {
		self.items
			.iter()
			.flat_map(|item| self.properties_of(item.id))
			.filter_map(|property| match property {
				Property::Auxiliary(urn) if !ALPHA_URNS.contains(&urn.as_str()) => Some(urn.as_str()),
				_ => None,
			})
			.collect()
	}

	pub fn extent_problem(&self, file_len: u64) -> Option<String> {
		self.locations
			.iter()
			.filter(|location| location.construction == 0)
			.flat_map(|location| location.extents.iter().map(move |extent| (location.item, extent)))
			.find_map(|(item, extent)| {
				let end = extent.offset.saturating_add(extent.length);
				(end > file_len || extent.offset > file_len).then(|| {
					format!(
						"truncated: item {item} ends {} bytes past the end of the file",
						end.max(extent.offset) - file_len
					)
				})
			})
	}
}

fn extent_bytes(source: &[u8], extent: Extent) -> Option<&[u8]> {
	let start = usize::try_from(extent.offset).ok()?;
	if extent.length == 0 {
		return source.get(start..);
	}

	let end = start.checked_add(usize::try_from(extent.length).ok()?)?;
	source.get(start..end)
}

fn multiply(a: [i8; 4], b: [i8; 4]) -> [i8; 4] {
	[
		a[0] * b[0] + a[1] * b[2],
		a[0] * b[1] + a[1] * b[3],
		a[2] * b[0] + a[3] * b[2],
		a[2] * b[1] + a[3] * b[3],
	]
}

fn name(kind: [u8; 4]) -> String {
	String::from_utf8_lossy(&kind).into_owned()
}
