use global_hotkey::hotkey::{Code, HotKey, Modifiers};

pub const TOGGLE_HOTKEY: HotKey = HotKey {
    id: 1,
    key: Code::Space,
    mods: Modifiers::ALT,
};

#[derive(Copy, Clone, Debug)]
pub enum ResizeDirection {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

pub const MIN_WIDTH: u16 = 10;
pub const MIN_HEIGHT: u16 = 5;

pub const DEFAULT_WIDTH: u16 = 40;
pub const DEFAULT_HEIGHT: u16 = 10;

pub const DEFAULT_X: u16 = 10;
pub const DEFAULT_Y: u16 = 5;
