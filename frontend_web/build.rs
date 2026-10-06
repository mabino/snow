fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The JS glue is consumed by the Emscripten linker; cargo does not track
    // link-arg files, so re-run the build when it changes.
    println!("cargo:rerun-if-changed=src/web.js");

    // Build scripts run on the host: select the flags by target and profile.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("emscripten") {
        let glue = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("src/web.js");
        println!("cargo:rustc-link-arg-bin=snow_web=--js-library={}", glue.display());
        if std::env::var("PROFILE").as_deref() == Ok("debug") {
            println!("cargo:rustc-link-arg-bin=snow_web=-sASSERTIONS=2");
            println!("cargo:rustc-link-arg-bin=snow_web=-sSTACK_OVERFLOW_CHECK=2");
        }
    }
}
