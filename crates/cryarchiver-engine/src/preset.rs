use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

pub const VIDEO_MEAN_RANGE: RangeInclusive<f64> = 80.0..=99.0;
pub const VIDEO_FLOOR_MIN: f64 = 70.0;
pub const IMAGE_RANGE: RangeInclusive<f64> = 50.0..=95.0;
pub const AUDIO_STEREO_RANGE: RangeInclusive<u32> = 64..=320;
pub const AUDIO_STEP_KBPS: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum Speed {
	Slow,
	#[default]
	Moderate,
	Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VmafOverride {
	pub mean: f64,
	pub floor: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Overrides {
	pub video: Option<VmafOverride>,
	pub image: Option<f64>,
	pub audio_stereo_kbps: Option<u32>,
}

impl Overrides {
	pub fn is_empty(&self) -> bool {
		*self == Self::default()
	}

	/// Every value clamped into its range and rounded to the quality cache key's resolution (0.1, or the 8 kbps
	/// step). A value that is not a finite number is dropped.
	#[must_use]
	pub fn normalized(&self) -> Self {
		let video = self
			.video
			.filter(|video| video.mean.is_finite() && video.floor.is_finite())
			.map(|video| {
				let mean = tenths(video.mean.clamp(*VIDEO_MEAN_RANGE.start(), *VIDEO_MEAN_RANGE.end()));
				VmafOverride {
					mean,
					floor: tenths(video.floor.clamp(VIDEO_FLOOR_MIN, mean)),
				}
			});
		let image = self
			.image
			.filter(|image| image.is_finite())
			.map(|image| tenths(image.clamp(*IMAGE_RANGE.start(), *IMAGE_RANGE.end())));
		let audio_stereo_kbps = self.audio_stereo_kbps.map(|kbps| {
			let stepped = kbps.saturating_add(AUDIO_STEP_KBPS / 2) / AUDIO_STEP_KBPS * AUDIO_STEP_KBPS;
			stepped.clamp(*AUDIO_STEREO_RANGE.start(), *AUDIO_STEREO_RANGE.end())
		});

		Self {
			video,
			image,
			audio_stereo_kbps,
		}
	}

	/// The normalized overrides the engine applies at `speed`. FAST never measures video quality, so a stored video
	/// override stays in the settings but takes no effect there.
	#[must_use]
	pub fn effective(&self, speed: Speed) -> Self {
		let mut effective = self.normalized();
		if speed == Speed::Fast {
			effective.video = None;
		}
		effective
	}

	/// One entry per field that is set: `video VMAF 90/85`, `image 80`, `audio 160 kbps`.
	#[must_use]
	pub fn describe(&self) -> Vec<String> {
		[
			self.video
				.map(|video| format!("video VMAF {}/{}", video.mean, video.floor)),
			self.image.map(|image| format!("image {image}")),
			self.audio_stereo_kbps.map(|kbps| format!("audio {kbps} kbps")),
		]
		.into_iter()
		.flatten()
		.collect()
	}

	pub fn validate(&self) -> Result<(), String> {
		if let Some(video) = self.video {
			if !VIDEO_MEAN_RANGE.contains(&video.mean) {
				return Err(format!(
					"the video VMAF mean must be between {} and {}",
					VIDEO_MEAN_RANGE.start(),
					VIDEO_MEAN_RANGE.end()
				));
			}

			if !(VIDEO_FLOOR_MIN..=video.mean).contains(&video.floor) {
				return Err(format!(
					"the video VMAF floor must be between {VIDEO_FLOOR_MIN} and the mean ({})",
					video.mean
				));
			}
		}

		if let Some(image) = self.image
			&& !IMAGE_RANGE.contains(&image)
		{
			return Err(format!(
				"the image SSIMULACRA2 target must be between {} and {}",
				IMAGE_RANGE.start(),
				IMAGE_RANGE.end()
			));
		}

		if let Some(kbps) = self.audio_stereo_kbps
			&& (!AUDIO_STEREO_RANGE.contains(&kbps) || kbps % AUDIO_STEP_KBPS != 0)
		{
			return Err(format!(
				"the stereo audio bitrate must be between {} and {} kbps, in steps of {AUDIO_STEP_KBPS}",
				AUDIO_STEREO_RANGE.start(),
				AUDIO_STEREO_RANGE.end()
			));
		}

		Ok(())
	}
}

fn tenths(value: f64) -> f64 {
	(value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
	use super::*;

	fn video(mean: f64, floor: f64) -> Overrides {
		Overrides {
			video: Some(VmafOverride { mean, floor }),
			..Overrides::default()
		}
	}

	fn image(target: f64) -> Overrides {
		Overrides {
			image: Some(target),
			..Overrides::default()
		}
	}

	fn audio(kbps: u32) -> Overrides {
		Overrides {
			audio_stereo_kbps: Some(kbps),
			..Overrides::default()
		}
	}

	fn reason(overrides: Overrides) -> String {
		overrides.validate().expect_err("the override is out of range")
	}

	#[test]
	fn no_overrides_are_empty_and_valid() {
		assert!(Overrides::default().is_empty());
		assert!(!image(80.0).is_empty());
		assert_eq!(Overrides::default().validate(), Ok(()));
	}

	#[test]
	fn video_overrides_accept_the_range_edges_and_name_the_range() {
		for valid in [video(80.0, 70.0), video(99.0, 99.0), video(90.0, 90.0)] {
			assert_eq!(valid.validate(), Ok(()), "{valid:?}");
		}

		assert!(reason(video(79.9, 70.0)).contains("between 80 and 99"));
		assert!(reason(video(99.1, 90.0)).contains("between 80 and 99"));
		assert!(reason(video(f64::NAN, 70.0)).contains("between 80 and 99"));
		assert!(reason(video(90.0, 69.9)).contains("between 70 and the mean (90)"));
		assert!(reason(video(90.0, 90.1)).contains("between 70 and the mean (90)"));
	}

	#[test]
	fn image_overrides_accept_the_range_edges_and_name_the_range() {
		assert_eq!(image(50.0).validate(), Ok(()));
		assert_eq!(image(95.0).validate(), Ok(()));

		assert!(reason(image(49.9)).contains("between 50 and 95"));
		assert!(reason(image(95.1)).contains("between 50 and 95"));
	}

	#[test]
	fn audio_overrides_accept_the_range_edges_in_steps_of_eight() {
		assert_eq!(audio(64).validate(), Ok(()));
		assert_eq!(audio(320).validate(), Ok(()));
		assert_eq!(audio(160).validate(), Ok(()));

		for invalid in [56, 328, 100] {
			assert!(
				reason(audio(invalid)).contains("between 64 and 320 kbps, in steps of 8"),
				"{invalid}"
			);
		}
	}

	#[test]
	fn normalizing_clamps_into_the_ranges_and_rounds_to_the_key_resolution() {
		assert_eq!(video(120.0, 60.0).normalized(), video(99.0, 70.0));
		assert_eq!(video(50.0, 90.0).normalized(), video(80.0, 80.0));
		assert_eq!(video(90.04, 85.06).normalized(), video(90.0, 85.1));
		assert_eq!(video(90.0, 95.0).normalized(), video(90.0, 90.0));
		assert_eq!(image(200.0).normalized(), image(95.0));
		assert_eq!(image(10.0).normalized(), image(50.0));
		assert_eq!(image(80.04).normalized(), image(80.0));
		assert_eq!(image(62.46).normalized(), image(62.5));
		assert_eq!(audio(1000).normalized(), audio(320));
		assert_eq!(audio(0).normalized(), audio(64));
		assert_eq!(audio(99).normalized(), audio(96));
		assert_eq!(audio(100).normalized(), audio(104));
		assert_eq!(audio(u32::MAX).normalized(), audio(320));
	}

	#[test]
	fn normalizing_keeps_valid_values_and_drops_values_that_are_not_numbers() {
		for valid in [
			video(93.0, 89.0),
			video(92.5, 87.5),
			image(62.5),
			audio(160),
			Overrides::default(),
		] {
			assert_eq!(valid.normalized(), valid);
		}

		assert_eq!(video(f64::NAN, 80.0).normalized(), Overrides::default());
		assert_eq!(video(90.0, f64::NAN).normalized(), Overrides::default());
		assert_eq!(image(f64::NAN).normalized(), Overrides::default());
		assert_eq!(video(f64::INFINITY, 80.0).normalized(), Overrides::default());
		assert_eq!(video(95.0, 91.0).normalized().validate(), Ok(()));
		assert_eq!(video(120.0, 10.0).normalized().validate(), Ok(()));
	}

	#[test]
	fn only_overrides_that_take_effect_are_effective() {
		let custom = Overrides {
			video: Some(VmafOverride {
				mean: 90.04,
				floor: 85.0,
			}),
			image: Some(80.04),
			audio_stereo_kbps: None,
		};

		assert_eq!(custom.effective(Speed::Moderate), custom.normalized());
		assert_eq!(custom.effective(Speed::Slow), custom.normalized());
		assert_eq!(custom.effective(Speed::Fast), image(80.0));
		assert!(video(90.0, 85.0).effective(Speed::Fast).is_empty());
		assert!(!video(90.0, 85.0).effective(Speed::Moderate).is_empty());
		assert!(image(f64::NAN).effective(Speed::Moderate).is_empty());
	}

	#[test]
	fn describing_lists_only_the_fields_that_are_set() {
		assert_eq!(Overrides::default().describe(), Vec::<String>::new());
		assert_eq!(video(90.0, 85.0).describe(), vec!["video VMAF 90/85".to_owned()]);
		assert_eq!(
			Overrides {
				image: Some(80.0),
				audio_stereo_kbps: Some(160),
				..video(90.0, 85.0)
			}
			.describe(),
			vec![
				"video VMAF 90/85".to_owned(),
				"image 80".to_owned(),
				"audio 160 kbps".to_owned(),
			]
		);
	}

	#[test]
	fn overrides_read_records_that_lack_a_field() {
		let read: Overrides = serde_json::from_str(r#"{"image":60.0}"#).unwrap();

		assert_eq!(read, image(60.0));
		assert_eq!(
			serde_json::from_str::<Overrides>(&serde_json::to_string(&video(90.0, 85.0)).unwrap()).unwrap(),
			video(90.0, 85.0)
		);
	}
}
