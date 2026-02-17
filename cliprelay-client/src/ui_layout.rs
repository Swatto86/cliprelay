/// UI sizing constants for the ClipRelay client.
///
/// With egui, DPI scaling is handled automatically. These constants define
/// logical pixel sizes used for window defaults.

/// Default options window width (logical pixels).
pub const OPTIONS_DEFAULT_W_PX: f32 = 680.0;
/// Default options window height (logical pixels).
pub const OPTIONS_DEFAULT_H_PX: f32 = 560.0;
/// Minimum options window width (logical pixels).
pub const OPTIONS_MIN_W_PX: f32 = 560.0;
/// Minimum options window height (logical pixels).
pub const OPTIONS_MIN_H_PX: f32 = 460.0;

/// Default choose-room dialog width.
pub const CHOOSE_ROOM_DEFAULT_W_PX: f32 = 620.0;
/// Choose-room dialog height when a saved config exists.
pub const CHOOSE_ROOM_HAS_SAVED_H_PX: f32 = 320.0;
/// Choose-room dialog height when no saved config exists.
pub const CHOOSE_ROOM_NO_SAVED_H_PX: f32 = 230.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_size_constants_are_reasonable() {
        assert!(OPTIONS_DEFAULT_W_PX >= 600.0);
        assert!(OPTIONS_DEFAULT_H_PX >= 460.0);
        assert!(OPTIONS_MIN_W_PX >= 480.0);
        assert!(OPTIONS_MIN_H_PX >= 360.0);
        assert!(CHOOSE_ROOM_DEFAULT_W_PX >= 520.0);
    }
}
