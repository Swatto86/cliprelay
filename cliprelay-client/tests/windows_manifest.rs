#![cfg(target_os = "windows")]

use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

use windows_sys::Win32::{
    Foundation::HMODULE,
    System::LibraryLoader::{
        FindResourceW, FreeLibrary, LoadLibraryExW, LoadResource, LockResource, SizeofResource,
        LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE,
    },
};

const RT_MANIFEST: u16 = 24;
const MANIFEST_RESOURCE_ID: u16 = 1;

#[test]
fn release_exe_embeds_common_controls_v6_manifest() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let target_dir = temp.path().join("target");

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("client crate has a workspace parent")
        .to_path_buf();

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    let status = Command::new(cargo)
        .current_dir(&workspace_root)
        .args(["build", "-p", "cliprelay-client", "--release"])
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .expect("run cargo build");

    assert!(status.success(), "cargo build failed: {status}");

    let exe_path = target_dir
        .join("release")
        .join("cliprelay-client.exe");
    assert!(exe_path.exists(), "exe not found at {}", exe_path.display());

    let manifest_text = load_exe_manifest(&exe_path)
        .unwrap_or_else(|err| panic!("failed to read embedded manifest: {err}"));

    assert!(
        manifest_text.contains("Microsoft.Windows.Common-Controls"),
        "manifest missing Common Controls dependency"
    );
    assert!(
        manifest_text.contains("version=\"6.0.0.0\""),
        "manifest missing Common Controls v6 version"
    );
}

fn load_exe_manifest(path: &Path) -> Result<String, String> {
    let module = unsafe {
        LoadLibraryExW(
            wide_null(path.as_os_str()).as_ptr(),
            0,
            LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE,
        )
    } as HMODULE;

    if module == 0 {
        return Err("LoadLibraryExW failed".to_string());
    }

    let resource = unsafe {
        FindResourceW(
            module,
            make_int_resource(MANIFEST_RESOURCE_ID),
            make_int_resource(RT_MANIFEST),
        )
    };

    if resource == 0 {
        unsafe { FreeLibrary(module) };
        return Err("RT_MANIFEST resource not found".to_string());
    }

    let size = unsafe { SizeofResource(module, resource) } as usize;
    if size == 0 {
        unsafe { FreeLibrary(module) };
        return Err("RT_MANIFEST resource size was 0".to_string());
    }

    let handle = unsafe { LoadResource(module, resource) };
    if handle == 0 {
        unsafe { FreeLibrary(module) };
        return Err("LoadResource failed".to_string());
    }

    let data_ptr = unsafe { LockResource(handle) } as *const u8;
    if data_ptr.is_null() {
        unsafe { FreeLibrary(module) };
        return Err("LockResource returned null".to_string());
    }

    let bytes = unsafe { std::slice::from_raw_parts(data_ptr, size) };

    // Typically UTF-8, but be robust: fall back to UTF-16LE if needed.
    let text = if let Ok(s) = String::from_utf8(bytes.to_vec()) {
        s
    } else if bytes.len() % 2 == 0 {
        let mut utf16 = Vec::with_capacity(bytes.len() / 2);
        for chunk in bytes.chunks_exact(2) {
            utf16.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        String::from_utf16(&utf16).map_err(|e| format!("manifest UTF-16 decode failed: {e}"))?
    } else {
        return Err("manifest was neither valid UTF-8 nor UTF-16LE".to_string());
    };

    unsafe { FreeLibrary(module) };
    Ok(text)
}

fn wide_null(s: &OsStr) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_wide().collect();
    v.push(0);
    v
}

fn make_int_resource(id: u16) -> *const u16 {
    id as usize as *const u16
}
