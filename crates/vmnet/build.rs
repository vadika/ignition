fn main() {
    cc::Build::new()
        .file("src/vmnet_shim.c")
        .flag("-fblocks")
        .compile("vmnet_shim");
    println!("cargo:rustc-link-lib=framework=vmnet");
    // libxpc / libdispatch are in libSystem; no extra link needed on macOS.
    println!("cargo:rerun-if-changed=src/vmnet_shim.c");
}
