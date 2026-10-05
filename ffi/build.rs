fn main() {
    // rustc's default install name for an Apple cdylib is its absolute build
    // path (`<target>/<profile>/deps/...`). That path is stale as soon as the
    // library is moved, packaged, or restored from a build cache into another
    // directory, and loaders then fail to find it. `@rpath` lets each consumer
    // supply the directory (`-Wl,-rpath,...` or `DYLD_LIBRARY_PATH`).
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "macos" || os == "ios" {
        println!(
            "cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libaster_transport_ffi.dylib"
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
