//! Give `sb-tray.exe` a manifest that asks for Common Controls 6.
//!
//! Without it Windows hands the app the 1990s-style slider and checkbox. MSVC's
//! linker can write the manifest itself from a dependency string, so this needs
//! no resource compiler and no `.rc` file. Other toolchains (the MinGW
//! cross-build used for CI-style checks) skip it and get the classic controls,
//! which work the same.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "msvc" {
        println!("cargo:rustc-link-arg-bin=sb-tray=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=sb-tray=/MANIFESTDEPENDENCY:type='win32' \
             name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
             processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
}
