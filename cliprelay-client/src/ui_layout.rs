#[cfg(target_os = "windows")]
use native_windows_gui as nwg;

/// UI sizing (unscaled base pixels).
///
/// These are intentionally a bit larger than the original defaults so the UI remains usable on
/// typical 1080p/1440p displays.
pub const OPTIONS_DEFAULT_W_PX: i32 = 680;
pub const OPTIONS_DEFAULT_H_PX: i32 = 560;
pub const OPTIONS_MIN_W_PX: i32 = 560;
pub const OPTIONS_MIN_H_PX: i32 = 460;

pub const CHOOSE_ROOM_DEFAULT_W_PX: i32 = 620;
pub const CHOOSE_ROOM_HAS_SAVED_H_PX: i32 = 320;
pub const CHOOSE_ROOM_NO_SAVED_H_PX: i32 = 230;

pub fn options_info_box_flags() -> nwg::TextBoxFlags {
    // The options text includes many lines (including history). We need a vertical scrollbar so
    // users can read it on smaller windows.
    //
    // Note: `nwg::TextBox` is multiline by default (forced ES_MULTILINE).
    nwg::TextBoxFlags::VISIBLE
        | nwg::TextBoxFlags::TAB_STOP
        | nwg::TextBoxFlags::VSCROLL
        | nwg::TextBoxFlags::AUTOVSCROLL
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn options_info_box_is_scrollable() {
        let flags = options_info_box_flags();
        assert!(
            flags.contains(nwg::TextBoxFlags::VSCROLL),
            "options info box must have a vertical scrollbar"
        );
    }

    #[test]
    fn ui_size_constants_are_reasonable() {
        assert!(OPTIONS_DEFAULT_W_PX >= 600);
        assert!(OPTIONS_DEFAULT_H_PX >= 460);
        assert!(OPTIONS_MIN_W_PX >= 480);
        assert!(OPTIONS_MIN_H_PX >= 360);
        assert!(CHOOSE_ROOM_DEFAULT_W_PX >= 520);
    }
}
