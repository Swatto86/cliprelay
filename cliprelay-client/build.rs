#[cfg(target_os = "windows")]
fn main() {
    use std::path::PathBuf;

    let manifest_path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "assets", "app.manifest"]
        .iter()
        .collect();

    println!("cargo:rerun-if-changed={}", manifest_path.display());

    // Embed the manifest via the MSVC linker so builds don't depend on `rc.exe`.
    // This is required for Common Controls v6 activation (and functions like `GetWindowSubclass`).
    println!("cargo:rustc-link-arg-bin=cliprelay-client=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=cliprelay-client=/MANIFESTINPUT:{}",
        manifest_path.display()
    );
}

#[cfg(not(target_os = "windows"))]
fn main() {
    // No-op on non-Windows.
}
