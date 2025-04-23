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
