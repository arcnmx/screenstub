use std::mem::replace;
use std::num::NonZeroU64;

#[derive(Debug, Copy, Clone, Default)]
pub struct Motion {
    pub x: MotionVector,
    pub y: MotionVector,
}

#[derive(Debug, Copy, Clone, Default)]
pub struct MotionVector {
    magnitude: i64,
    //direction: i8,
}

fn copysign(value: u64, sign: i64) -> i64 {
    let value = value as i64;
    if sign < 0 { -value } else { value }
}

impl MotionVector {
    pub fn clear(&mut self) {
        self.magnitude = 0;
    }

    pub fn truncate(&mut self, resolution: NonZeroU64) -> Option<i64> {
        let abs = self.magnitude.unsigned_abs();
        if abs < resolution.get() {
            return None
        }

        let div = abs / resolution;
        let rounded = div * resolution.get();
        let mod_ = abs - rounded;

        self.magnitude = copysign(mod_, self.magnitude);
        Some(copysign(div, self.magnitude))
    }

    pub fn round_up(&mut self, resolution: NonZeroU64) -> Option<i64> {
        if self.magnitude == 0 {
            return None
        }

        let abs = self.magnitude.unsigned_abs();
        let div = (abs + (resolution.get() - 1)) / resolution;
        let rounded = div * resolution.get();
        let mod_ = rounded - abs;

        self.magnitude = copysign(mod_, -self.magnitude);
        Some(copysign(div, self.magnitude))
    }

    pub fn offset(&mut self, value: i64) {
        self.magnitude += value;
    }

    pub fn append(&mut self, value: i64, resolution: NonZeroU64) -> Option<i64> {
        let signmatch = self.magnitude.signum() == value.signum();
        let res = if signmatch {
            None
        } else {
            self.truncate(resolution)
        };
        self.offset(value);
        res
    }

    /*pub fn append(&mut self, f: i64) -> Option<i64> {
        let sign = f.signum() as i8;
        let res = match sign != self.direction {
        };
        let res = if sign != self.direction {
            let res = self.commit(None);
            self.magnitude = 0;
            res
        } else {
            None
        };

        self.direction = sign;
        self.magnitude = self.magnitude.saturating_add(f.abs());

        res
    }*/

    /*pub fn commit(&mut self, resolution: Option<NonZeroU64>) -> Option<i64> {
        if self.magnitude > 0 {
            let magnitude = replace(&mut self.magnitude, 0);
            let abs = magnitude.unsigned_abs();
            let res = if let Some(resolution) = resolution {
                let div = (abs + resolution.get() - 1) / resolution;
                let rounded = (div * resolution.get()) as i64;
                let md = rounded - abs as i64;

                let left = magnitude - abs as i64;
                self.magnitude = left - md;
                rounded
            } else {
                abs as i64
            };
            Some(if self.direction < 0 { -res } else { res })
        } else {
            None
        }
    }*/
}
