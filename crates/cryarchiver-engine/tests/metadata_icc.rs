use cryarchiver_engine::metadata::icc::{Cicp, StandardSpace, cicp_for, classify, colour_space, problem};
use moxcms::ColorProfile;

fn encoded(profile: &ColorProfile) -> Vec<u8> {
	profile.encode().unwrap()
}

#[test]
fn standard_profiles_map_to_cicp() {
	assert_eq!(classify(&encoded(&ColorProfile::new_srgb())), Some(StandardSpace::Srgb));
	assert_eq!(
		classify(&encoded(&ColorProfile::new_display_p3())),
		Some(StandardSpace::DisplayP3)
	);
	assert_eq!(
		classify(&encoded(&ColorProfile::new_bt2020())),
		Some(StandardSpace::Rec2020Bt709)
	);
	assert_eq!(cicp_for(&encoded(&ColorProfile::new_srgb())), Cicp::SRGB);
	assert_eq!(
		cicp_for(&encoded(&ColorProfile::new_display_p3())),
		Cicp {
			primaries: 12,
			transfer: 13
		}
	);
}

#[test]
fn other_profiles_stay_unspecified() {
	for profile in [
		ColorProfile::new_adobe_rgb(),
		ColorProfile::new_pro_photo_rgb(),
		ColorProfile::new_bt2020_pq(),
		ColorProfile::new_gray_with_gamma(2.2),
	] {
		let icc = encoded(&profile);
		assert_eq!(classify(&icc), None);
		assert_eq!(cicp_for(&icc), Cicp::UNSPECIFIED);
	}
}

#[test]
fn broken_profiles_are_described() {
	let mut icc = encoded(&ColorProfile::new_srgb());
	assert_eq!(problem(&icc), None);
	assert_eq!(colour_space(&icc), Some(*b"RGB "));
	assert_eq!(
		colour_space(&encoded(&ColorProfile::new_gray_with_gamma(2.2))),
		Some(*b"GRAY")
	);

	assert!(problem(&icc[..100]).unwrap().contains("128-byte header"));

	icc[..4].copy_from_slice(&u32::MAX.to_be_bytes());
	assert!(problem(&icc).unwrap().contains("declares 4294967295 bytes"));

	let mut unsigned = encoded(&ColorProfile::new_srgb());
	unsigned[36..40].copy_from_slice(b"xxxx");
	assert!(problem(&unsigned).unwrap().contains("acsp"));
}
