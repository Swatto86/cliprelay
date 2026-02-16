use std::io::Write;

use cliprelay_client::ui_state::{
    clamp_placement_in_rect, load_ui_state_from_path, WindowPlacement, MAX_UI_STATE_BYTES,
};

#[test]
fn clamp_placement_in_rect_handles_negative_coords() {
    // Simulate a left-side monitor in a dual-monitor setup.
    let rect = [-1920, 0, 0, 1080];
    let placement = WindowPlacement {
        x: -5000,
        y: -200,
        w: 10_000,
        h: 10_000,
    };

    let clamped = clamp_placement_in_rect(placement, 300, 200, 16, rect);
    assert!(clamped.w >= 300);
    assert!(clamped.h >= 200);
    assert!(clamped.x >= rect[0]);
    assert!(clamped.x <= rect[2]);
    assert!(clamped.y >= rect[1]);
    assert!(clamped.y <= rect[3]);
}

#[test]
fn load_ui_state_ignores_oversized_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("ui_state.json");

    let mut file = std::fs::File::create(&path).expect("create ui_state.json");
    file.write_all(&vec![b'a'; (MAX_UI_STATE_BYTES as usize) + 1024])
        .expect("write oversized ui_state.json");
    drop(file);

    let err = load_ui_state_from_path(&path).expect_err("oversized file should error");
    let msg = err.to_string();
    assert!(msg.contains("too large"), "unexpected error: {msg}");
}
