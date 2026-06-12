fn main() {
    // Link Hypervisor.framework. Emitted here (the crate that actually calls
    // hv_*) so every downstream binary links it transitively. Mirrors libkrun's
    // vmm/build.rs. The binary still needs an ad-hoc codesign with the
    // com.apple.security.hypervisor entitlement at runtime (see scripts/sign.sh).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=Hypervisor");
    }
}
