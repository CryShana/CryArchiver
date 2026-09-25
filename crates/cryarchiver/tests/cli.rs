use std::process::Command;

#[test]
fn prints_version() {
	let out = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.arg("--version")
		.output()
		.unwrap();

	assert!(out.status.success());
	assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "cryarchiver 0.1.0");
}
