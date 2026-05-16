fn main() {
    #[cfg(feature = "cbindgen")]
    {
        use std::env;
        use std::path::PathBuf;

        let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
        let out_dir = PathBuf::from(&crate_dir);

        cbindgen::Builder::new()
            .with_crate(&crate_dir)
            .with_config(cbindgen::Config::from_file("cbindgen.toml").unwrap())
            .generate()
            .expect("Unable to generate C bindings")
            .write_to_file(out_dir.join("drevo.h"));
    }
}
