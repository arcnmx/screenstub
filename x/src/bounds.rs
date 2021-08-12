use screenstub_config::ConfigRect;

#[derive(Debug, Clone, Copy, Default)]
pub struct Bounds {
    pub lower: i32,
    pub upper: i32,
    pub size: i32,
}

impl Bounds {
    pub fn new(l: f64, u: f64) -> Self {
        let scale = 0x7fff as f64;
        let lower = (l * scale) as i32;
        let upper = (u * scale) as i32;

        Self {
            lower,
            upper,
            size: upper - lower,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Rect {
    pub x: Bounds,
    pub y: Bounds,
}

impl From<ConfigRect> for Rect {
    fn from(rect: ConfigRect) -> Self {
        Self {
            x: Bounds::new(rect.left, rect.right),
            y: Bounds::new(rect.top, rect.bottom),
        }
    }
}
