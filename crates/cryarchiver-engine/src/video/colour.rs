use crate::images::decode::deep_pix_fmt;
use crate::probe::Stream;

const HDR_TRANSFERS: &[&str] = &["smpte2084", "arib-std-b67"];
const SCALE_MATRICES: &[&str] = &["bt709", "bt470bg", "smpte170m", "fcc", "smpte240m", "bt2020nc"];
const UNSET: &[&str] = &["unknown", "unspecified", "reserved"];
const RGB_PREFIXES: &[&str] = &[
	"rgb", "bgr", "gbr", "argb", "abgr", "0rgb", "0bgr", "x2rgb", "x2bgr", "pal8",
];
const PAL_RATES: &[f64] = &[25.0, 50.0];
const ALPHA_420: &str = "yuva420p";
/// `geq` registers for the 2×2 block that 4:2:0 shares one chroma sample between: its corner (0 and 1), its four
/// alphas (2 to 5) and their sum (6), from which `BLOCK_COLOUR` is the block's alpha-weighted colour, rounded as
/// `scale` rounds since `geq` would truncate it.
const BLOCK_WEIGHTS: &str = "st(0,X-mod(X,2));st(1,Y-mod(Y,2));st(2,alpha(ld(0),ld(1)));st(3,alpha(ld(0)+1,ld(1)));\
	st(4,alpha(ld(0),ld(1)+1));st(5,alpha(ld(0)+1,ld(1)+1));st(6,ld(2)+ld(3)+ld(4)+ld(5))";
const BLOCK_COLOUR: &str = "round((ld(2)*p(ld(0),ld(1))+ld(3)*p(ld(0)+1,ld(1))+ld(4)*p(ld(0),ld(1)+1)+\
	ld(5)*p(ld(0)+1,ld(1)+1))/ld(6))";
/// Averages each 2×2 block evenly into its chroma sample, which the edge fill relies on. That holds at even sizes
/// only: at an odd width or height `scale` resamples the chroma by a fractional ratio, so a faint edge there still
/// takes in part of a neighbouring transparent block's colour.
const BLOCK_CHROMA: &str = "scale=flags=area";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
	Limited,
	Full,
}

impl Range {
	pub fn name(self) -> &'static str {
		match self {
			Self::Limited => "tv",
			Self::Full => "pc",
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColourTags {
	pub primaries: String,
	pub transfer: String,
	pub matrix: String,
	pub range: Range,
	/// Where the source's chroma samples sit, when it says.
	pub chroma_location: Option<String>,
}

impl ColourTags {
	pub fn is_hdr(&self) -> bool {
		HDR_TRANSFERS.contains(&self.transfer.as_str())
	}
}

pub fn output_tags(stream: &Stream) -> ColourTags {
	let [primaries, transfer, matrix] = inferred(stream);
	let tagged = |value: Option<&str>, fallback: &str| {
		value
			.filter(|name| !UNSET.contains(name))
			.unwrap_or(fallback)
			.to_owned()
	};

	let primaries = tagged(stream.color_primaries.as_deref(), primaries);
	let matrix = if primaries == "bt2020" { "bt2020nc" } else { matrix };

	if is_rgb(stream) {
		return ColourTags {
			primaries,
			transfer: tagged(stream.color_transfer.as_deref(), transfer),
			matrix: matrix.to_owned(),
			range: Range::Limited,
			chroma_location: None,
		};
	}

	let full = stream.color_range.as_deref() == Some("pc")
		|| stream
			.pix_fmt
			.as_deref()
			.is_some_and(|format| format.starts_with("yuvj"));
	ColourTags {
		primaries,
		transfer: tagged(stream.color_transfer.as_deref(), transfer),
		matrix: tagged(stream.color_space.as_deref(), matrix),
		range: if full { Range::Full } else { Range::Limited },
		chroma_location: stream
			.chroma_location
			.clone()
			.filter(|location| !UNSET.contains(&location.as_str())),
	}
}

/// The scale drops the frames' chroma location unless it is named, so it is passed through.
pub fn conversion(stream: &Stream, tags: &ColourTags, target_pix_fmt: &str) -> Option<String> {
	if stream.pix_fmt.as_deref() == Some(target_pix_fmt) {
		return None;
	}

	let scale = match kept_colour(stream, tags) {
		Some(kept) => format!("scale={kept}"),
		None => format!("scale=out_range={}:out_color_matrix={}", tags.range.name(), tags.matrix),
	};
	Some(format!("{scale},format={target_pix_fmt}"))
}

/// `conversion` to 4:2:0 with alpha, through 4:4:4 so that transparent pixels can first take their visible
/// neighbours' colour. A 4:2:0 source has no full chroma left to fill from.
pub fn alpha_conversion(stream: &Stream, tags: &ColourTags) -> Option<String> {
	if stream.pix_fmt.as_deref() == Some(ALPHA_420) {
		return None;
	}

	let full = conversion(stream, tags, "yuva444p").map_or_else(String::new, |full| format!("{full},"));
	Some(format!("{full}{},{BLOCK_CHROMA},format={ALPHA_420}", edge_fill()))
}

/// Spreads each 2×2 block's visible colour into its fully transparent pixels and gives the block's chroma its
/// alpha-weighted mean (`cr` follows `cb`): otherwise 4:2:0 averages the hidden colour, usually black, into a faint
/// edge.
fn edge_fill() -> String {
	format!(
		"geq=lum='{BLOCK_WEIGHTS};if(gt(alpha(X,Y),0)+eq(ld(6),0),p(X,Y),{BLOCK_COLOUR})':\
		 cb='{BLOCK_WEIGHTS};if(eq(ld(6),0),p(X,Y),{BLOCK_COLOUR})':a='alpha(X,Y)'"
	)
}

/// Scales the picture to `width`×`height` with square pixels, leaving its colour as it is. A transparent picture is
/// scaled premultiplied, or the colour hidden under transparent pixels would darken the partly transparent edges.
pub fn resize(stream: &Stream, tags: &ColourTags, (width, height): (u32, u32), alpha: bool) -> String {
	let kept = kept_colour(stream, tags).map_or_else(String::new, |kept| format!(":{kept}"));
	let scale = format!("scale=w={width}:h={height}:flags=lanczos+accurate_rnd{kept}");

	if alpha {
		format!("premultiply=inplace=1,{scale},unpremultiply=inplace=1,setsar=1")
	} else {
		format!("{scale},setsar=1")
	}
}

/// How an animation's frames are stored: full-range RGB, 8-bit for an 8-bit SDR picture and 16-bit for a deeper or
/// HDR one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameFormat {
	pub pix_fmt: &'static str,
	/// What FFV1 codes per pixel before any compression.
	pub bytes_per_pixel: u64,
}

impl FrameFormat {
	pub fn of(stream: &Stream, tags: &ColourTags, alpha: bool) -> Self {
		let deep = stream.pix_fmt.as_deref().is_some_and(deep_pix_fmt) || tags.is_hdr();
		let (pix_fmt, bytes_per_pixel) = match (deep, alpha) {
			(false, false) => ("bgr0", 3),
			(false, true) => ("bgra", 4),
			(true, false) => ("gbrp16le", 6),
			(true, true) => ("gbrap16le", 8),
		};
		Self {
			pix_fmt,
			bytes_per_pixel,
		}
	}
}

/// The conversion to `format`, tagged with the picture's primaries and transfer. A YUV picture is read with its own
/// range, matrix and chroma location; one whose matrix `scale` can't name is refused rather than miscoloured.
pub fn rgb_frames(stream: &Stream, tags: &ColourTags, format: FrameFormat) -> Result<String, String> {
	let tagged = format!(
		"format={},setparams=field_mode=prog:range=pc:color_primaries={}:color_trc={}:colorspace=gbr",
		format.pix_fmt, tags.primaries, tags.transfer
	);
	if is_rgb(stream) {
		return Ok(tagged);
	}

	if !SCALE_MATRICES.contains(&tags.matrix.as_str()) {
		return Err(format!(
			"the video's colour matrix ({}) can't be converted to RGB, so it can't become an animated AVIF",
			tags.matrix
		));
	}

	let chroma = tags
		.chroma_location
		.as_deref()
		.map_or_else(String::new, |location| format!(":in_chroma_loc={location}"));
	Ok(format!(
		"scale=in_range={}:in_color_matrix={}{chroma}:out_range=pc,{tagged}",
		tags.range.name(),
		tags.matrix
	))
}

/// The `scale` options that keep a YUV picture's range, matrix and chroma location; `None` for RGB.
fn kept_colour(stream: &Stream, tags: &ColourTags) -> Option<String> {
	if is_rgb(stream) {
		return None;
	}

	let range = tags.range.name();
	let chroma = tags.chroma_location.as_deref().map_or_else(String::new, |location| {
		format!(":in_chroma_loc={location}:out_chroma_loc={location}")
	});
	Some(if SCALE_MATRICES.contains(&tags.matrix.as_str()) {
		let matrix = &tags.matrix;
		format!("in_range={range}:out_range={range}:in_color_matrix={matrix}:out_color_matrix={matrix}{chroma}")
	} else {
		format!("in_range={range}:out_range={range}{chroma}")
	})
}

pub fn setparams(tags: &ColourTags) -> String {
	let chroma = tags
		.chroma_location
		.as_deref()
		.map_or_else(String::new, |location| format!(":chroma_location={location}"));
	format!(
		"setparams=field_mode=prog:range={}:color_primaries={}:color_trc={}:colorspace={}{chroma}",
		tags.range.name(),
		tags.primaries,
		tags.transfer,
		tags.matrix
	)
}

fn inferred(stream: &Stream) -> [&'static str; 3] {
	let height = stream.height.unwrap_or(1080);
	if height > 576 {
		return ["bt709", "bt709", "bt709"];
	}

	let pal_rate = stream
		.fps()
		.is_some_and(|fps| PAL_RATES.iter().any(|rate| (fps - rate).abs() < 0.01));
	if height > 480 || pal_rate {
		["bt470bg", "smpte170m", "bt470bg"]
	} else {
		["smpte170m", "smpte170m", "smpte170m"]
	}
}

fn is_rgb(stream: &Stream) -> bool {
	stream.color_space.as_deref() == Some("gbr")
		|| stream
			.pix_fmt
			.as_deref()
			.is_some_and(|format| RGB_PREFIXES.iter().any(|prefix| format.starts_with(prefix)))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn stream(json: &str) -> Stream {
		serde_json::from_str(json).unwrap()
	}

	#[test]
	fn tagged_sources_keep_their_tags_verbatim() {
		let hlg = stream(
			r#"{"index":0,"codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuv420p10le","color_range":"tv",
			"color_space":"bt2020nc","color_transfer":"arib-std-b67","color_primaries":"bt2020"}"#,
		);

		let tags = output_tags(&hlg);

		assert_eq!(tags.transfer, "arib-std-b67");
		assert_eq!(tags.matrix, "bt2020nc");
		assert_eq!(tags.range, Range::Limited);
		assert_eq!(conversion(&hlg, &tags, "yuv420p10le"), None);
		assert_eq!(
			setparams(&tags),
			"setparams=field_mode=prog:range=tv:color_primaries=bt2020:color_trc=arib-std-b67:colorspace=bt2020nc"
		);
	}

	#[test]
	fn untagged_sources_are_inferred_from_their_size_and_rate() {
		let ntsc = stream(
			r#"{"index":0,"codec_type":"video","width":720,"height":480,"pix_fmt":"yuv420p","r_frame_rate":"30000/1001"}"#,
		);
		let pal = stream(
			r#"{"index":0,"codec_type":"video","width":720,"height":576,"pix_fmt":"yuv420p","r_frame_rate":"25/1"}"#,
		);
		let cif = stream(
			r#"{"index":0,"codec_type":"video","width":352,"height":288,"pix_fmt":"yuv420p","r_frame_rate":"25/1"}"#,
		);
		let hd = stream(r#"{"index":0,"codec_type":"video","width":1280,"height":720,"pix_fmt":"yuv420p"}"#);

		assert_eq!(output_tags(&ntsc).matrix, "smpte170m");
		assert_eq!(output_tags(&ntsc).primaries, "smpte170m");
		assert_eq!(output_tags(&pal).primaries, "bt470bg");
		assert_eq!(output_tags(&pal).matrix, "bt470bg");
		assert_eq!(output_tags(&cif).primaries, "bt470bg");
		assert_eq!(output_tags(&hd).matrix, "bt709");
		assert_eq!(output_tags(&hd).transfer, "bt709");
	}

	#[test]
	fn a_bt2020_source_without_a_matrix_tag_gets_the_bt2020_matrix() {
		let untagged_matrix = stream(
			r#"{"index":0,"codec_type":"video","width":3840,"height":2160,"pix_fmt":"yuv420p10le",
			"color_transfer":"smpte2084","color_primaries":"bt2020"}"#,
		);
		let untagged = stream(r#"{"index":0,"codec_type":"video","width":3840,"height":2160,"pix_fmt":"yuv420p10le"}"#);
		let rgb = stream(
			r#"{"index":0,"codec_type":"video","width":3840,"height":2160,"pix_fmt":"gbrp16le","color_primaries":"bt2020"}"#,
		);

		assert_eq!(output_tags(&untagged_matrix).matrix, "bt2020nc");
		assert_eq!(output_tags(&untagged).matrix, "bt709");
		assert_eq!(output_tags(&rgb).matrix, "bt2020nc");
	}

	#[test]
	fn only_missing_tags_are_inferred() {
		let nikon = stream(
			r#"{"index":0,"codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuvj420p","color_range":"pc",
			"color_space":"smpte170m","color_primaries":"bt709"}"#,
		);

		let tags = output_tags(&nikon);

		assert_eq!(tags.matrix, "smpte170m");
		assert_eq!(tags.primaries, "bt709");
		assert_eq!(tags.transfer, "bt709");
		assert_eq!(tags.range, Range::Full);
	}

	#[test]
	fn conversions_keep_matrix_and_range() {
		let full = stream(
			r#"{"index":0,"codec_type":"video","width":640,"height":360,"pix_fmt":"yuvj420p","color_space":"bt709"}"#,
		);
		let tags = output_tags(&full);

		assert_eq!(
			conversion(&full, &tags, "yuv420p10le").as_deref(),
			Some("scale=in_range=pc:out_range=pc:in_color_matrix=bt709:out_color_matrix=bt709,format=yuv420p10le")
		);
	}

	#[test]
	fn the_chroma_location_is_carried_through_the_conversion() {
		let topleft = stream(
			r#"{"index":0,"codec_type":"video","width":1918,"height":802,"pix_fmt":"yuv420p","color_space":"bt709",
			"chroma_location":"topleft"}"#,
		);
		let unspecified = stream(
			r#"{"index":0,"codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuv420p","chroma_location":"unspecified"}"#,
		);
		let tags = output_tags(&topleft);

		assert_eq!(tags.chroma_location.as_deref(), Some("topleft"));
		assert_eq!(
			conversion(&topleft, &tags, "yuv420p10le").as_deref(),
			Some(
				"scale=in_range=tv:out_range=tv:in_color_matrix=bt709:out_color_matrix=bt709:in_chroma_loc=topleft:\
				 out_chroma_loc=topleft,format=yuv420p10le"
			)
		);
		assert!(setparams(&tags).ends_with(":colorspace=bt709:chroma_location=topleft"));
		assert_eq!(output_tags(&unspecified).chroma_location, None);
		assert!(!setparams(&output_tags(&unspecified)).contains("chroma"));
	}

	#[test]
	fn matrices_scale_cannot_name_are_left_to_setparams() {
		let constant_luminance = stream(
			r#"{"index":0,"codec_type":"video","width":3840,"height":2160,"pix_fmt":"yuv422p10le","color_range":"tv",
			"color_space":"bt2020c","color_transfer":"smpte2084","color_primaries":"bt2020"}"#,
		);
		let tags = output_tags(&constant_luminance);

		assert_eq!(tags.matrix, "bt2020c");
		assert_eq!(
			conversion(&constant_luminance, &tags, "yuv420p10le").as_deref(),
			Some("scale=in_range=tv:out_range=tv,format=yuv420p10le")
		);
	}

	#[test]
	fn a_resize_keeps_the_range_matrix_and_chroma_location_and_writes_square_pixels() {
		let full = stream(
			r#"{"index":0,"codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuvj420p","color_space":"bt709",
			"chroma_location":"topleft"}"#,
		);
		let constant_luminance = stream(
			r#"{"index":0,"codec_type":"video","width":3840,"height":2160,"pix_fmt":"yuv422p10le","color_range":"tv",
			"color_space":"bt2020c","color_transfer":"smpte2084","color_primaries":"bt2020"}"#,
		);
		let argb = stream(r#"{"index":0,"codec_type":"video","width":640,"height":360,"pix_fmt":"argb"}"#);

		assert_eq!(
			resize(&full, &output_tags(&full), (640, 360), false),
			"scale=w=640:h=360:flags=lanczos+accurate_rnd:in_range=pc:out_range=pc:in_color_matrix=bt709:\
			 out_color_matrix=bt709:in_chroma_loc=topleft:out_chroma_loc=topleft,setsar=1"
		);
		assert_eq!(
			resize(
				&constant_luminance,
				&output_tags(&constant_luminance),
				(1280, 720),
				false
			),
			"scale=w=1280:h=720:flags=lanczos+accurate_rnd:in_range=tv:out_range=tv,setsar=1"
		);
		assert_eq!(
			resize(&argb, &output_tags(&argb), (320, 180), false),
			"scale=w=320:h=180:flags=lanczos+accurate_rnd,setsar=1"
		);
	}

	#[test]
	fn animation_frames_are_full_range_rgb_from_the_source_range_and_matrix_with_its_primaries_and_transfer() {
		let limited = stream(
			r#"{"index":0,"codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuv420p","color_space":"bt709",
			"chroma_location":"topleft"}"#,
		);
		let hlg = stream(
			r#"{"index":0,"codec_type":"video","width":3840,"height":2160,"pix_fmt":"yuv420p10le","color_range":"tv",
			"color_space":"bt2020nc","color_transfer":"arib-std-b67","color_primaries":"bt2020"}"#,
		);
		let argb = stream(r#"{"index":0,"codec_type":"video","width":640,"height":360,"pix_fmt":"argb"}"#);
		let rgb = |stream: &Stream, alpha: bool| {
			let tags = output_tags(stream);
			rgb_frames(stream, &tags, FrameFormat::of(stream, &tags, alpha)).unwrap()
		};

		assert_eq!(
			rgb(&limited, false),
			"scale=in_range=tv:in_color_matrix=bt709:in_chroma_loc=topleft:out_range=pc,format=bgr0,\
			 setparams=field_mode=prog:range=pc:color_primaries=bt709:color_trc=bt709:colorspace=gbr"
		);
		assert_eq!(
			rgb(&hlg, false),
			"scale=in_range=tv:in_color_matrix=bt2020nc:out_range=pc,format=gbrp16le,\
			 setparams=field_mode=prog:range=pc:color_primaries=bt2020:color_trc=arib-std-b67:colorspace=gbr"
		);
		assert_eq!(
			rgb(&argb, true),
			"format=bgra,setparams=field_mode=prog:range=pc:color_primaries=smpte170m:color_trc=smpte170m:\
			 colorspace=gbr"
		);
	}

	#[test]
	fn animation_frames_keep_the_source_depth_and_hdr() {
		let format = |json: &str, alpha: bool| {
			let stream = stream(json);
			FrameFormat::of(&stream, &output_tags(&stream), alpha)
		};

		let sdr = format(
			r#"{"index":0,"codec_type":"video","height":1080,"pix_fmt":"yuv420p"}"#,
			false,
		);
		let sdr_alpha = format(
			r#"{"index":0,"codec_type":"video","height":1080,"pix_fmt":"yuva420p"}"#,
			true,
		);
		let ten_bit = format(
			r#"{"index":0,"codec_type":"video","height":1080,"pix_fmt":"yuv420p10le"}"#,
			false,
		);
		let pq_8bit = format(
			r#"{"index":0,"codec_type":"video","height":1080,"pix_fmt":"yuv420p","color_transfer":"smpte2084"}"#,
			true,
		);

		assert_eq!((sdr.pix_fmt, sdr.bytes_per_pixel), ("bgr0", 3));
		assert_eq!((sdr_alpha.pix_fmt, sdr_alpha.bytes_per_pixel), ("bgra", 4));
		assert_eq!((ten_bit.pix_fmt, ten_bit.bytes_per_pixel), ("gbrp16le", 6));
		assert_eq!((pq_8bit.pix_fmt, pq_8bit.bytes_per_pixel), ("gbrap16le", 8));
	}

	#[test]
	fn animation_frames_refuse_a_matrix_scale_cannot_read() {
		let constant_luminance = stream(
			r#"{"index":0,"codec_type":"video","width":3840,"height":2160,"pix_fmt":"yuv420p10le","color_range":"tv",
			"color_space":"bt2020c","color_transfer":"smpte2084","color_primaries":"bt2020"}"#,
		);
		let tags = output_tags(&constant_luminance);

		let refused = rgb_frames(
			&constant_luminance,
			&tags,
			FrameFormat::of(&constant_luminance, &tags, false),
		);

		assert!(refused.is_err_and(|reason| reason.contains("bt2020c")));
	}

	#[test]
	fn transparent_pixels_take_their_blocks_colour_before_4_2_0_and_a_4_2_0_source_is_left_alone() {
		let argb = stream(r#"{"index":0,"codec_type":"video","width":640,"height":360,"pix_fmt":"argb"}"#);
		let vp9 = stream(r#"{"index":0,"codec_type":"video","width":640,"height":360,"pix_fmt":"yuva420p"}"#);

		let converted = alpha_conversion(&argb, &output_tags(&argb)).unwrap();

		assert!(
			converted.starts_with("scale=out_range=tv:out_color_matrix=smpte170m,format=yuva444p,geq=lum='"),
			"{converted}"
		);
		assert!(
			converted.ends_with(":a='alpha(X,Y)',scale=flags=area,format=yuva420p"),
			"{converted}"
		);
		assert_eq!(alpha_conversion(&vp9, &output_tags(&vp9)), None);
	}

	#[test]
	fn rgb_sources_get_a_yuv_matrix() {
		let argb = stream(r#"{"index":0,"codec_type":"video","width":640,"height":360,"pix_fmt":"argb"}"#);
		let tags = output_tags(&argb);

		assert_eq!(tags.matrix, "smpte170m");
		assert_eq!(tags.range, Range::Limited);
		assert_eq!(
			conversion(&argb, &tags, "yuva420p").as_deref(),
			Some("scale=out_range=tv:out_color_matrix=smpte170m,format=yuva420p")
		);
	}
}
