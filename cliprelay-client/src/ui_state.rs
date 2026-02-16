use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Defensive bound: `ui_state.json` is expected to be tiny.
///
/// This prevents pathological reads if the file is corrupted or replaced.
pub const MAX_UI_STATE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SavedUiState {
    #[serde(default)]
    pub send: Option<WindowPlacement>,
    #[serde(default)]
    pub options: Option<WindowPlacement>,
    #[serde(default)]
    pub popup: Option<WindowPlacement>,
    /// Persisted global hotkey label (e.g. "Ctrl+Shift+V").
    /// `None` or `"None"` means hotkey is disabled.
    #[serde(default)]
    pub hotkey: Option<String>,
}

#[derive(Debug)]
pub enum UiStateLoadError {
    Metadata(io::Error),
    TooLarge { size: u64, max: u64 },
    Read(io::Error),
    Parse(serde_json::Error),
}

impl std::fmt::Display for UiStateLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiStateLoadError::Metadata(e) => write!(f, "metadata read failed: {e}"),
            UiStateLoadError::TooLarge { size, max } => {
                write!(f, "file too large: {size} bytes (max {max})")
            }
            UiStateLoadError::Read(e) => write!(f, "read failed: {e}"),
            UiStateLoadError::Parse(e) => write!(f, "parse failed: {e}"),
        }
    }
}

impl std::error::Error for UiStateLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UiStateLoadError::Metadata(e) => Some(e),
            UiStateLoadError::Read(e) => Some(e),
            UiStateLoadError::Parse(e) => Some(e),
            UiStateLoadError::TooLarge { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum UiStateSaveError {
    Serialize(serde_json::Error),
    WriteTmp(io::Error),
    Rename(io::Error),
}

impl std::fmt::Display for UiStateSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiStateSaveError::Serialize(e) => write!(f, "serialize failed: {e}"),
            UiStateSaveError::WriteTmp(e) => write!(f, "tmp write failed: {e}"),
            UiStateSaveError::Rename(e) => write!(f, "rename failed: {e}"),
        }
    }
}

impl std::error::Error for UiStateSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UiStateSaveError::Serialize(e) => Some(e),
            UiStateSaveError::WriteTmp(e) => Some(e),
            UiStateSaveError::Rename(e) => Some(e),
        }
    }
}

pub fn ui_state_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("ClipRelay");
    let _ = fs::create_dir_all(&dir);
    dir.join("ui_state.json")
}

pub fn parse_ui_state_json(data: &str) -> Result<SavedUiState, serde_json::Error> {
    serde_json::from_str::<SavedUiState>(data)
}

pub fn load_ui_state_from_path(path: &Path) -> Result<SavedUiState, UiStateLoadError> {
    let meta = fs::metadata(path).map_err(UiStateLoadError::Metadata)?;
    if meta.len() > MAX_UI_STATE_BYTES {
        return Err(UiStateLoadError::TooLarge {
            size: meta.len(),
            max: MAX_UI_STATE_BYTES,
        });
    }

    let data = fs::read_to_string(path).map_err(UiStateLoadError::Read)?;
    parse_ui_state_json(&data).map_err(UiStateLoadError::Parse)
}

pub fn load_ui_state() -> SavedUiState {
    let path = ui_state_path();
    load_ui_state_from_path(&path).unwrap_or_default()
}

pub fn save_ui_state_to_path(path: &Path, state: &SavedUiState) -> Result<(), UiStateSaveError> {
    let tmp = path.with_extension("json.tmp");
    let payload = serde_json::to_string_pretty(state).map_err(UiStateSaveError::Serialize)?;
    fs::write(&tmp, payload.as_bytes()).map_err(UiStateSaveError::WriteTmp)?;

    if path.exists() {
        let _ = fs::remove_file(path);
    }

    fs::rename(&tmp, path).map_err(UiStateSaveError::Rename)?;
    Ok(())
}

pub fn save_ui_state_with_retry(state: &SavedUiState) -> Result<(), UiStateSaveError> {
    const MAX_ATTEMPTS: u32 = 3;
    const BACKOFF_BASE_MS: u64 = 50;

    let path = ui_state_path();

    let mut last_err: Option<UiStateSaveError> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match save_ui_state_to_path(&path, state) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                if attempt >= MAX_ATTEMPTS {
                    break;
                }
                let backoff_ms = BACKOFF_BASE_MS.saturating_mul(1_u64 << (attempt - 1));
                std::thread::sleep(Duration::from_millis(backoff_ms));
            }
        }
    }

    Err(last_err.expect("retry loop sets last_err"))
}

/// Clamp a window placement into a given monitor rectangle.
///
/// `rect` is `[left, top, right, bottom]` in virtual-screen coordinates.
pub fn clamp_placement_in_rect(
    placement: WindowPlacement,
    min_w: u32,
    min_h: u32,
    margin_px: i32,
    rect: [i32; 4],
) -> WindowPlacement {
    let [left, top, right, bottom] = rect;
    let monitor_w = (right - left).max(200);
    let monitor_h = (bottom - top).max(200);

    let max_w = (monitor_w - margin_px * 2).max(200) as u32;
    let max_h = (monitor_h - margin_px * 2).max(200) as u32;

    let w = placement.w.clamp(min_w, max_w);
    let h = placement.h.clamp(min_h, max_h);

    let min_x = left;
    let min_y = top;
    let max_x = (right - w as i32).max(left);
    let max_y = (bottom - h as i32).max(top);

    let x = placement.x.clamp(min_x, max_x);
    let y = placement.y.clamp(min_y, max_y);

    WindowPlacement { x, y, w, h }
}
