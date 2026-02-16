#[cfg(target_os = "windows")]
fn main() {
    let _ = embed_resource::compile("assets/cliprelay-client.rc", embed_resource::NONE);
}

#[cfg(not(target_os = "windows"))]
fn main() {}
