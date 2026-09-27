use cryarchiver_engine::metadata::iptc::{self, IPTC_RESOURCE};
use cryarchiver_engine::metadata::xmp::{
	CRYARCHIVER, DC, Property, Value, XmpEdit, create, patch, uses_namespace, values,
};

const PACKET_START: &str = "<?xpacket begin='\u{feff}' id='W5M0MpCehiHzreSzNTczkc9d'?>\n<x:xmpmeta xmlns:x='adobe:ns:meta/'>\n<rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>\n";
const PACKET_END: &str = "</rdf:RDF>\n</x:xmpmeta>\n                                        \n<?xpacket end='w'?>";

fn packet(descriptions: &str) -> String {
	format!("{PACKET_START}{descriptions}{PACKET_END}")
}

fn rights(text: &str) -> Property {
	Property {
		namespace: DC,
		prefix: "dc",
		name: "rights",
		value: Value::LangAlt(text.to_owned()),
	}
}

fn subject(words: &[&str]) -> Property {
	Property {
		namespace: DC,
		prefix: "dc",
		name: "subject",
		value: Value::Bag(words.iter().map(|word| (*word).to_owned()).collect()),
	}
}

#[test]
fn orientation_and_dimensions_are_patched_in_attribute_and_element_form() {
	let source = packet(concat!(
		"<rdf:Description rdf:about='' xmlns:tiff='http://ns.adobe.com/tiff/1.0/' tiff:Orientation='6' ",
		"tiff:ImageWidth='1200' tiff:Make='A&amp;B'/>\n",
		"<rdf:Description rdf:about='' xmlns:t='http://ns.adobe.com/tiff/1.0/' ",
		"xmlns:exif='http://ns.adobe.com/exif/1.0/'>\n",
		"<t:ImageLength>800</t:ImageLength><exif:PixelXDimension>1200</exif:PixelXDimension>",
		"<exif:PixelYDimension/></rdf:Description>\n"
	));

	let out = patch(
		&source,
		&XmpEdit {
			dimensions: Some((800, 1200)),
			..XmpEdit::default()
		},
	)
	.unwrap();

	assert!(out.contains("tiff:Orientation=\"1\""), "{out}");
	assert!(out.contains("tiff:ImageWidth=\"800\""), "{out}");
	assert!(out.contains("tiff:Make=\"A&amp;B\""), "{out}");
	assert!(out.contains("<t:ImageLength>1200</t:ImageLength>"), "{out}");
	assert!(
		out.contains("<exif:PixelXDimension>800</exif:PixelXDimension>"),
		"{out}"
	);
	assert!(
		out.contains("<exif:PixelYDimension>1200</exif:PixelYDimension>"),
		"{out}"
	);
	assert!(out.ends_with("</x:xmpmeta>\n<?xpacket end='w'?>"), "{out}");
}

#[test]
fn only_missing_properties_are_added() {
	let source = packet(
		"<rdf:Description rdf:about='' xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:rights><rdf:Alt><rdf:li xml:lang='x-default'>© Test</rdf:li></rdf:Alt></dc:rights></rdf:Description>\n",
	);

	let out = patch(
		&source,
		&XmpEdit {
			add: vec![rights("ignored"), subject(&["テスト", "a & b"])],
			..XmpEdit::default()
		},
	)
	.unwrap();

	assert_eq!(out.matches("<dc:rights>").count(), 1, "{out}");
	assert!(!out.contains("ignored"), "{out}");
	assert!(out.contains("<rdf:li>テスト</rdf:li>"), "{out}");
	assert!(out.contains("<rdf:li>a &amp; b</rdf:li>"), "{out}");
}

#[test]
fn extended_packets_are_merged_and_their_marker_dropped() {
	let main = packet(
		"<rdf:Description rdf:about='' xmlns:xmpNote='http://ns.adobe.com/xmp/note/' xmpNote:HasExtendedXMP='83DE3BB8A00448E560FFAD25263768DF'/>\n",
	);
	let extended = packet(
		"<rdf:Description rdf:about='' xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:description><rdf:Alt><rdf:li xml:lang='x-default'>long text</rdf:li></rdf:Alt></dc:description></rdf:Description>\n",
	);
	let caption = Property {
		namespace: DC,
		prefix: "dc",
		name: "description",
		value: Value::LangAlt("IPTC caption".into()),
	};

	let out = patch(
		&main,
		&XmpEdit {
			add: vec![caption],
			extended: vec![extended],
			..XmpEdit::default()
		},
	)
	.unwrap();

	assert!(!out.contains("HasExtendedXMP"), "{out}");
	assert_eq!(out.matches("<dc:description>").count(), 1, "{out}");
	assert!(out.contains("long text"), "{out}");
	assert!(!out.contains("IPTC caption"), "{out}");
}

#[test]
fn a_packet_is_created_when_the_source_has_none() {
	let source_text = Property {
		namespace: CRYARCHIVER,
		prefix: "cryarchiver",
		name: "SourceText",
		value: Value::Bag(vec!["parameters=a cat, <masterpiece>".into()]),
	};

	let out = create(&XmpEdit {
		add: vec![source_text],
		..XmpEdit::default()
	});

	assert!(out.starts_with("<?xpacket begin=\"\u{feff}\""), "{out}");
	assert!(uses_namespace(&out, CRYARCHIVER));
	assert!(!uses_namespace(&out, DC));
	assert_eq!(
		values(&out, "http://www.w3.org/1999/02/22-rdf-syntax-ns#", "li"),
		["parameters=a cat, <masterpiece>"]
	);
}

fn dataset(record: u8, id: u8, data: &[u8]) -> Vec<u8> {
	let mut bytes = vec![0x1C, record, id];
	bytes.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
	bytes.extend_from_slice(data);
	bytes
}

fn irb(iim: &[u8]) -> Vec<u8> {
	let mut bytes = b"8BIM".to_vec();
	bytes.extend_from_slice(&IPTC_RESOURCE.to_be_bytes());
	bytes.extend_from_slice(&[0, 0]);
	bytes.extend_from_slice(&u32::try_from(iim.len()).unwrap().to_be_bytes());
	bytes.extend_from_slice(iim);
	if iim.len() % 2 == 1 {
		bytes.push(0);
	}
	bytes
}

#[test]
fn iptc_text_is_decoded_as_declared_utf8_undeclared_utf8_or_windows_1252() {
	let declared = [
		dataset(1, 90, b"\x1b%G"),
		dataset(2, 25, "テスト".as_bytes()),
		dataset(2, 25, b"keyword2"),
	]
	.concat();
	let undeclared = dataset(2, 120, "caption Ž".as_bytes());
	let latin = dataset(2, 120, b"Caf\xe9 \x8e");

	let declared = iptc::fields(iptc::iim(&irb(&declared)).unwrap()).unwrap();
	let undeclared = iptc::fields(iptc::iim(&irb(&undeclared)).unwrap()).unwrap();
	let latin = iptc::fields(iptc::iim(&irb(&latin)).unwrap()).unwrap();

	assert_eq!(declared.keywords, ["テスト", "keyword2"]);
	assert_eq!(undeclared.caption.as_deref(), Some("caption Ž"));
	assert_eq!(latin.caption.as_deref(), Some("Café Ž"));
}

#[test]
fn iptc_fields_map_to_dublin_core_and_photoshop_properties() {
	let iim = [
		dataset(2, 5, b"Title1"),
		dataset(2, 80, b"Photographer"),
		dataset(2, 116, b"(c) Me"),
		dataset(2, 90, b"Maribor"),
		dataset(2, 55, b"20200102"),
		dataset(2, 60, b"030405+0100"),
		vec![0, 0, 0],
	]
	.concat();

	let properties = iptc::xmp_properties(&iptc::fields(&iim).unwrap());
	let find = |name: &str| {
		properties
			.iter()
			.find(|property| property.name == name)
			.map(|property| &property.value)
	};

	assert_eq!(find("title"), Some(&Value::LangAlt("Title1".into())));
	assert_eq!(find("creator"), Some(&Value::Seq(vec!["Photographer".into()])));
	assert_eq!(find("rights"), Some(&Value::LangAlt("(c) Me".into())));
	assert_eq!(find("City"), Some(&Value::Text("Maribor".into())));
	assert_eq!(
		find("DateCreated"),
		Some(&Value::Text("2020-01-02T03:04:05+01:00".into()))
	);
	assert!(iptc::fields(b"\x1c\x02").is_err());
}
