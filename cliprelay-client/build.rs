#[cfg(target_os = "windows")]
fn main() {
    use std::path::Path;

    let manifest_path = Path::new("assets/app.manifest");
    let icon_path = Path::new("assets/app-icon-circle-c.ico");

    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!("cargo:rerun-if-changed={}", icon_path.display());

    // Use winres to embed icon (works cross-platform)
    let mut res = winres::WindowsResource::new();
    res.set_icon(icon_path.to_str().unwrap());

    // Windows file properties (Explorer -> Details)
    // Note: Windows doesn't have a standard "Developer" field for PE files; Explorer typically
    // surfaces CompanyName as Developer/Company depending on the view.
    res.set("CompanyName", "Swatto");
    res.set("ProductName", "ClipRelay");
    res.set("FileDescription", "ClipRelay Client");
    res.set("InternalName", "cliprelay-client");
    res.set("OriginalFilename", "cliprelay-client.exe");
    
    if let Err(e) = res.compile() {
        eprintln!("Warning: icon embedding failed: {}", e);
    }

    // Embed manifest via MSVC linker (requires MSVC toolchain)
    // This is critical for Common Controls v6
    let manifest_abs = std::env::current_dir()
        .unwrap()
        .join(manifest_path);
    println!("cargo:rustc-link-arg-bin=cliprelay-client=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=cliprelay-client=/MANIFESTINPUT:{}",
        manifest_abs.display()
    );
}

#[cfg(not(target_os = "windows"))]
fn main() {
    // No-op on non-Windows.
}
