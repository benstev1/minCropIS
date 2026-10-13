//! Fixed output crop from the largest jitter observed on each axis.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CropRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Centered crop leaving `ceil(max_jitter)` pixels of margin on each side, so
/// every frame can be shifted by up to its jitter without exposing a border.
/// Dimensions are trimmed to even numbers as yuv420p encoding requires.
pub fn compute(width: usize, height: usize, max_jitter_x: f64, max_jitter_y: f64) -> CropRect {
    let margin = |m: f64, size: usize| (m.max(0.0).ceil() as usize).min(size.saturating_sub(2) / 2);
    let (mx, my) = (margin(max_jitter_x, width), margin(max_jitter_y, height));
    CropRect {
        x: mx,
        y: my,
        width: (width - 2 * mx) & !1,
        height: (height - 2 * my) & !1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_jitter_keeps_full_frame() {
        assert_eq!(
            compute(1920, 1080, 0.0, 0.0),
            CropRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn rounds_margin_up_to_whole_pixels() {
        assert_eq!(
            compute(1920, 1080, 3.2, 0.5),
            CropRect {
                x: 4,
                y: 1,
                width: 1912,
                height: 1078
            }
        );
    }

    #[test]
    fn odd_dimensions_become_even() {
        let r = compute(641, 481, 2.0, 2.0);
        assert_eq!((r.width, r.height), (636, 476));
    }
}
