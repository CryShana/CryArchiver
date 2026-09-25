use std::error::Error;
use std::fs::File;
use std::path::Path;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, TaggedFile, TaggedFileExt};
use lofty::ogg::{OggPictureStorage, OpusFile};
use lofty::picture::Picture;
use lofty::probe::Probe;
use lofty::tag::Tag;

pub fn source_pictures(source: &Path) -> Vec<Picture> {
	let Ok(tagged) = read_tags(source) else {
		return Vec::new();
	};

	let mut unique: Vec<Picture> = Vec::new();
	for picture in tagged.tags().iter().flat_map(Tag::pictures) {
		if !unique
			.iter()
			.any(|seen| seen.pic_type() == picture.pic_type() && seen.data() == picture.data())
		{
			unique.push(picture.clone());
		}
	}
	unique
}

pub fn add_pictures(opus: &Path, pictures: Vec<Picture>) -> Result<(), Box<dyn Error + Send + Sync>> {
	if pictures.is_empty() {
		return Ok(());
	}

	let mut file = OpusFile::read_from(&mut File::open(opus)?, ParseOptions::new().read_properties(false))?;
	for picture in pictures {
		file.vorbis_comments_mut().insert_picture(picture, None)?;
	}

	file.save_to_path(opus, WriteOptions::default())?;
	Ok(())
}

fn read_tags(source: &Path) -> Result<TaggedFile, Box<dyn Error + Send + Sync>> {
	Ok(Probe::open(source)?
		.options(ParseOptions::new().read_properties(false))
		.guess_file_type()?
		.read()?)
}
