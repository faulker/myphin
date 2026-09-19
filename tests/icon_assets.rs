//! The app icon ships in three forms and each has to stay in place: main.rs embeds the
//! PNG for the Dock, Dioxus.toml points at the ICNS for bundling, and the UI links the SVG.

use std::fs;
use std::path::Path;

/// Reads a file under the repo's assets folder.
fn asset(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join(name);
    fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn icon_png_is_a_1024_square() {
    let png = asset("icon.png");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    // IHDR is the first chunk: width and height are big-endian u32 at offsets 16 and 20.
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
    assert_eq!((width, height), (1024, 1024));
}

#[test]
fn icon_icns_has_the_icns_magic() {
    assert_eq!(&asset("icon.icns")[..4], b"icns");
}

#[test]
fn icon_svg_uses_the_brand_accent() {
    let svg = String::from_utf8(asset("icon.svg")).unwrap();
    assert!(svg.starts_with("<svg"));
    assert!(svg.contains("#e0a14a"), "gold accent missing from the mark");
}

#[test]
fn bundle_config_points_at_the_icns() {
    let toml =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Dioxus.toml")).unwrap();
    assert!(toml.contains("[bundle]"));
    assert!(toml.contains("assets/icon.icns"));
}
