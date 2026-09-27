use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Write as _;

use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::name::{LocalName, Namespace, NamespaceResolver, ResolveResult};
use quick_xml::reader::NsReader;
use quick_xml::{Writer, XmlVersion};

pub const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
pub const TIFF: &str = "http://ns.adobe.com/tiff/1.0/";
pub const EXIF: &str = "http://ns.adobe.com/exif/1.0/";
pub const XMP_NOTE: &str = "http://ns.adobe.com/xmp/note/";
pub const DC: &str = "http://purl.org/dc/elements/1.1/";
pub const PHOTOSHOP: &str = "http://ns.adobe.com/photoshop/1.0/";
pub const CRYARCHIVER: &str = "http://ns.cryarchiver.local/1.0/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
	Text(String),
	LangAlt(String),
	Bag(Vec<String>),
	Seq(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
	pub namespace: &'static str,
	pub prefix: &'static str,
	pub name: &'static str,
	pub value: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmpEdit {
	pub dimensions: Option<(u32, u32)>,
	pub add: Vec<Property>,
	pub extended: Vec<String>,
}

type Name = (String, String);

struct TextOverride {
	depth: usize,
	value: String,
	written: bool,
}

struct Rewriter<'a> {
	edit: &'a XmpEdit,
	missing: Vec<Property>,
	merging: bool,
	writer: Writer<Vec<u8>>,
	stack: Vec<Option<Name>>,
	text_override: Option<TextOverride>,
	skipping_below: Option<usize>,
}

pub fn create(edit: &XmpEdit) -> String {
	let mut body = String::new();
	for packet in &edit.extended {
		body.push_str(inner_descriptions(packet).unwrap_or_default());
	}
	if !edit.add.is_empty() {
		body.push_str(&render(&edit.add));
	}

	format!(
		"<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n <rdf:RDF xmlns:rdf=\"{RDF}\">{body}</rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>"
	)
}

pub fn patch(xmp: &str, edit: &XmpEdit) -> Result<String, quick_xml::Error> {
	let mut present = top_level_properties(xmp)?;
	for packet in &edit.extended {
		present.extend(top_level_properties(packet)?);
	}

	let mut rewriter = Rewriter {
		edit,
		missing: edit
			.add
			.iter()
			.filter(|property| !present.contains(&(property.namespace.to_owned(), property.name.to_owned())))
			.cloned()
			.collect(),
		merging: !edit.extended.is_empty(),
		writer: Writer::new(Vec::with_capacity(xmp.len() + 4096)),
		stack: Vec::new(),
		text_override: None,
		skipping_below: None,
	};
	let mut reader = NsReader::from_str(xmp);

	loop {
		let (resolved, event) = reader.read_resolved_event()?;
		let name = match &event {
			Event::Start(e) | Event::Empty(e) => bound(&resolved, e.local_name()),
			Event::End(e) => bound(&resolved, e.local_name()),
			_ => None,
		};

		if matches!(event, Event::Eof) {
			break;
		}
		if rewriter.skipping(&event, name.as_ref()) {
			continue;
		}

		match event {
			Event::Start(ref start) => rewriter.element(start, name, true, reader.resolver())?,
			Event::Empty(ref start) => rewriter.element(start, name, false, reader.resolver())?,
			Event::Text(_) | Event::GeneralRef(_) | Event::CData(_) if rewriter.text_override.is_some() => {
				rewriter.replace_text()?;
			}
			Event::End(ref end) => rewriter.end(end, name.as_ref())?,
			other => rewriter.writer.write_event(other)?,
		}
	}
	Ok(rewriter.finish())
}

impl Rewriter<'_> {
	fn skipping(&mut self, event: &Event<'_>, name: Option<&Name>) -> bool {
		let Some(depth) = self.skipping_below else {
			return false;
		};

		match event {
			Event::Start(_) => self.stack.push(name.cloned()),
			Event::End(_) => {
				self.stack.pop();
				if self.stack.len() == depth {
					self.skipping_below = None;
				}
			}
			_ => {}
		}
		true
	}

	fn element(
		&mut self,
		start: &BytesStart<'_>,
		name: Option<Name>,
		is_start: bool,
		resolver: &NamespaceResolver,
	) -> Result<(), quick_xml::Error> {
		if self.merging && is(name.as_ref(), XMP_NOTE, "HasExtendedXMP") {
			if is_start {
				self.skipping_below = Some(self.stack.len());
				self.stack.push(name);
			}
			return Ok(());
		}

		let is_description = is(name.as_ref(), RDF, "Description");
		let rebuilt = if is_description {
			self.description(start, resolver)?
		} else {
			start.to_owned()
		};
		let value = replacement(name.as_ref(), self.edit).filter(|_| !is_description);

		if is_start {
			if let Some(value) = value {
				self.text_override = Some(TextOverride {
					depth: self.stack.len(),
					value,
					written: false,
				});
			}
			self.writer.write_event(Event::Start(rebuilt))?;
			self.stack.push(name);
		} else if let Some(value) = value {
			self.writer.write_event(Event::Start(rebuilt.borrow()))?;
			self.writer.write_event(Event::Text(BytesText::new(&value)))?;
			self.writer.write_event(Event::End(rebuilt.to_end()))?;
		} else {
			self.writer.write_event(Event::Empty(rebuilt))?;
		}
		Ok(())
	}

	fn description(
		&self,
		start: &BytesStart<'_>,
		resolver: &NamespaceResolver,
	) -> Result<BytesStart<'static>, quick_xml::Error> {
		let mut rebuilt = start.to_owned();
		rebuilt.clear_attributes();

		for attribute in start.attributes() {
			let attribute = attribute.map_err(quick_xml::Error::from)?;
			let (namespace, local) = resolver.resolve_attribute(attribute.key);
			let name = bound(&namespace, local);
			if self.merging && is(name.as_ref(), XMP_NOTE, "HasExtendedXMP") {
				continue;
			}

			match replacement(name.as_ref(), self.edit) {
				Some(value) => rebuilt.push_attribute(Attribute {
					key: attribute.key,
					value: Cow::Owned(value),
				}),
				None => rebuilt.push_attribute(attribute),
			}
		}
		Ok(rebuilt.into_owned())
	}

	fn replace_text(&mut self) -> Result<(), quick_xml::Error> {
		let pending = self.text_override.as_mut().expect("only called while replacing text");
		if !pending.written {
			self.writer.write_event(Event::Text(BytesText::new(&pending.value)))?;
			pending.written = true;
		}
		Ok(())
	}

	fn end(&mut self, end: &BytesEnd<'_>, name: Option<&Name>) -> Result<(), quick_xml::Error> {
		self.stack.pop();
		if let Some(pending) = self.text_override.take_if(|pending| pending.depth == self.stack.len())
			&& !pending.written
		{
			self.writer.write_event(Event::Text(BytesText::new(&pending.value)))?;
		}

		if is(name, RDF, "RDF") {
			let out = self.writer.get_mut();
			for packet in &self.edit.extended {
				out.extend_from_slice(inner_descriptions(packet).unwrap_or_default().as_bytes());
			}
			if !self.missing.is_empty() {
				out.extend_from_slice(render(&self.missing).as_bytes());
			}
		}
		self.writer.write_event(Event::End(end.to_owned()))?;
		Ok(())
	}

	fn finish(self) -> String {
		let mut out = String::from_utf8(self.writer.into_inner()).expect("the input was UTF-8");
		if let Some(end) = out.rfind("<?xpacket end") {
			let keep = out[..end].trim_end().len();
			out.replace_range(keep..end, "\n");
		}
		out
	}
}

pub fn uses_namespace(xmp: &str, namespace: &str) -> bool {
	let mut reader = NsReader::from_str(xmp);
	loop {
		let Ok((resolved, event)) = reader.read_resolved_event() else {
			return false;
		};
		match event {
			Event::Start(ref start) | Event::Empty(ref start) => {
				if bound(&resolved, start.local_name()).is_some_and(|(uri, _)| uri == namespace) {
					return true;
				}
				let in_namespace = start.attributes().flatten().any(|attribute| {
					let (resolved, local) = reader.resolver().resolve_attribute(attribute.key);
					bound(&resolved, local).is_some_and(|(uri, _)| uri == namespace)
				});
				if in_namespace {
					return true;
				}
			}
			Event::Eof => return false,
			_ => {}
		}
	}
}

pub fn values(xmp: &str, namespace: &str, local: &str) -> Vec<String> {
	let mut reader = NsReader::from_str(xmp);
	let mut found = Vec::new();
	let mut text: Option<String> = None;

	loop {
		let Ok((resolved, event)) = reader.read_resolved_event() else {
			return found;
		};
		match event {
			Event::Start(ref start) | Event::Empty(ref start) => {
				let matching = is(bound(&resolved, start.local_name()).as_ref(), namespace, local);
				for attribute in start.attributes().flatten() {
					let (resolved, name) = reader.resolver().resolve_attribute(attribute.key);
					if is(bound(&resolved, name).as_ref(), namespace, local)
						&& let Ok(value) = attribute.normalized_value(XmlVersion::Implicit1_0)
					{
						found.push(value.into_owned());
					}
				}
				text = (matches!(event, Event::Start(_)) && matching).then(String::new);
			}
			Event::Text(ref content) => {
				if let Some(text) = &mut text {
					text.push_str(&content.xml10_content());
				}
			}
			Event::GeneralRef(ref reference) => {
				if let Some(text) = &mut text {
					match reference.resolve_char_ref() {
						Ok(Some(character)) => text.push(character),
						_ => text.push_str(
							quick_xml::escape::resolve_predefined_entity(&reference.xml10_content())
								.unwrap_or_default(),
						),
					}
				}
			}
			Event::End(_) => {
				if let Some(text) = text.take() {
					found.push(text.trim().to_owned());
				}
			}
			Event::Eof => return found,
			_ => {}
		}
	}
}

fn top_level_properties(xmp: &str) -> Result<HashSet<Name>, quick_xml::Error> {
	let mut reader = NsReader::from_str(xmp);
	let mut stack: Vec<Option<Name>> = Vec::new();
	let mut found = HashSet::new();

	loop {
		let (resolved, event) = reader.read_resolved_event()?;
		match event {
			Event::Start(ref start) | Event::Empty(ref start) => {
				let name = bound(&resolved, start.local_name());
				let depth = stack.len();
				let parent_is_description = depth >= 2
					&& is(stack[depth - 1].as_ref(), RDF, "Description")
					&& is(stack[depth - 2].as_ref(), RDF, "RDF");
				if parent_is_description && let Some(name) = name.clone() {
					found.insert(name);
				}

				let is_top_description =
					is(name.as_ref(), RDF, "Description") && depth >= 1 && is(stack[depth - 1].as_ref(), RDF, "RDF");
				if is_top_description {
					for attribute in start.attributes().flatten() {
						let (resolved, local) = reader.resolver().resolve_attribute(attribute.key);
						if let Some(attribute_name) = bound(&resolved, local).filter(|(uri, _)| uri != RDF) {
							found.insert(attribute_name);
						}
					}
				}
				if matches!(event, Event::Start(_)) {
					stack.push(name);
				}
			}
			Event::End(_) => {
				stack.pop();
			}
			Event::Eof => return Ok(found),
			_ => {}
		}
	}
}

fn bound(resolved: &ResolveResult<'_>, local: LocalName<'_>) -> Option<Name> {
	match resolved {
		ResolveResult::Bound(Namespace(uri)) => Some(((*uri).to_owned(), local.as_ref().to_owned())),
		_ => None,
	}
}

fn is(name: Option<&Name>, namespace: &str, local: &str) -> bool {
	name.is_some_and(|(uri, name)| uri == namespace && name == local)
}

fn replacement(name: Option<&Name>, edit: &XmpEdit) -> Option<String> {
	let (uri, local) = name?;
	match (uri.as_str(), local.as_str()) {
		(TIFF, "Orientation") => Some("1".to_owned()),
		(TIFF, "ImageWidth") | (EXIF, "PixelXDimension") => edit.dimensions.map(|(width, _)| width.to_string()),
		(TIFF, "ImageLength") | (EXIF, "PixelYDimension") => edit.dimensions.map(|(_, height)| height.to_string()),
		_ => None,
	}
}

fn inner_descriptions(packet: &str) -> Option<&str> {
	let start = packet.find("<rdf:RDF")?;
	let open_end = start + packet[start..].find('>')? + 1;
	let close = packet.rfind("</rdf:RDF>")?;
	(close >= open_end).then(|| &packet[open_end..close])
}

fn render(properties: &[Property]) -> String {
	let mut prefixes: Vec<(&str, &str)> = properties
		.iter()
		.map(|property| (property.prefix, property.namespace))
		.collect();
	prefixes.sort_unstable();
	prefixes.dedup();

	let mut out = String::from("\n  <rdf:Description rdf:about=\"\"");
	for (prefix, namespace) in prefixes {
		let _ = write!(out, "\n    xmlns:{prefix}=\"{namespace}\"");
	}
	out.push('>');

	for property in properties {
		let name = format!("{}:{}", property.prefix, property.name);
		match &property.value {
			Value::Text(text) => {
				let _ = write!(out, "\n   <{name}>{}</{name}>", escape(text));
			}
			Value::LangAlt(text) => {
				let _ = write!(
					out,
					"\n   <{name}>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </{name}>",
					escape(text)
				);
			}
			Value::Bag(items) | Value::Seq(items) => {
				let container = if matches!(property.value, Value::Bag(_)) {
					"Bag"
				} else {
					"Seq"
				};
				let _ = write!(out, "\n   <{name}>\n    <rdf:{container}>");
				for item in items {
					let _ = write!(out, "\n     <rdf:li>{}</rdf:li>", escape(item));
				}
				let _ = write!(out, "\n    </rdf:{container}>\n   </{name}>");
			}
		}
	}
	out.push_str("\n  </rdf:Description>\n ");
	out
}

fn escape(text: &str) -> Cow<'_, str> {
	quick_xml::escape::partial_escape(text)
}
