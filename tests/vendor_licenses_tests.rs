//! The libraries under `static/web/vendor/` are redistributed inside
//! `drevo-server`, so their licence notices ship with it:
//! `static/web/vendor/LICENSES.txt` lists every vendored file with its
//! package, licence and copyright line, followed by the licence text.

use std::fs;
use std::path::Path;

fn vendor_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("static/web/vendor")
}

fn notices() -> String {
    fs::read_to_string(vendor_dir().join("LICENSES.txt"))
        .expect("static/web/vendor/LICENSES.txt must exist")
}

#[test]
fn every_vendored_file_is_listed_with_its_licence() {
    let notices = notices();
    let mut scripts: Vec<String> = fs::read_dir(vendor_dir())
        .expect("static/web/vendor")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".js"))
        .collect();
    scripts.sort();
    assert!(!scripts.is_empty(), "the vendor directory holds libraries");
    for script in scripts {
        assert!(
            notices.contains(&script),
            "LICENSES.txt must have an entry for {script}"
        );
    }
}

#[test]
fn the_notices_name_each_package_and_carry_the_licence_text() {
    let notices = notices();
    for package in [
        "cytoscape 3.30.2",
        "layout-base 2.0.1",
        "cose-base 2.2.0",
        "cytoscape-fcose 2.2.0",
        "webcola 3.4.0",
        "cytoscape-cola 2.5.1",
    ] {
        assert!(
            notices.contains(package),
            "LICENSES.txt must name {package}"
        );
    }
    assert!(
        notices.matches("Copyright").count() >= 6,
        "every package comes with its copyright line"
    );
    assert!(
        notices.contains("Permission is hereby granted, free of charge"),
        "LICENSES.txt must carry the MIT permission notice"
    );
}
