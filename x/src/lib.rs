mod util;
mod bounds;
mod motion;
mod context;
mod events;
mod xinput;

pub use self::bounds::{Bounds, Rect};
pub use self::context::xmain;
pub use self::motion::{Motion, MotionVector};

#[derive(Debug)]
pub enum XEvent {
    Visible(bool),
    Focus(bool),
    Close,
    Input(input_linux::InputEvent),
}

#[derive(Debug)]
pub enum XRequest {
    Quit,
    UnstickHost,
    Grab {
        xcore: bool,
        confine: bool,
        motion: bool,
        devices: Vec<screenstub_config::ConfigInputDevice>,
    },
    Ungrab,
}
