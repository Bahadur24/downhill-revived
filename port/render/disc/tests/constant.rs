//! The game core's copy of the USA disc's value (port/hle/online_menu.c us_files_sha256: the core
//! is C without Rust) is the crate's.

#[test]
fn online_menu_c_has_the_crates_value() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hle/online_menu.c");
    let text = std::fs::read_to_string(path).unwrap();
    let line = text
        .lines()
        .find(|l| l.contains("us_files_sha256[] ="))
        .expect("online_menu.c has us_files_sha256");
    assert_eq!(line.split('"').nth(1), Some(dhd_disc::USA_FILES_SHA256));
}
