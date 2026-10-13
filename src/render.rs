//! Sub-pixel shift + crop of RGB frames.

use crate::crop::CropRect;

/// Writes the stabilized crop of `src` into `dst`. Output pixel (u, v) samples the
/// source at (crop.x + u + jitter_x, crop.y + v + jitter_y), undoing the jitter.
pub fn shift_and_crop(
    src: &[u8],
    src_width: usize,
    src_height: usize,
    crop: CropRect,
    jitter_x: f64,
    jitter_y: f64,
    dst: &mut Vec<u8>,
) {
    dst.clear();
    dst.reserve(crop.width * crop.height * 3);

    // The offset is constant across the frame, so split it once into whole and fractional parts.
    let sx = crop.x as f64 + jitter_x;
    let sy = crop.y as f64 + jitter_y;
    let (ix, iy) = (sx.floor() as isize, sy.floor() as isize);
    let (fx, fy) = ((sx - sx.floor()) as f32, (sy - sy.floor()) as f32);
    let weights = [
        (1.0 - fx) * (1.0 - fy),
        fx * (1.0 - fy),
        (1.0 - fx) * fy,
        fx * fy,
    ];

    let clamp = |v: isize, n: usize| v.clamp(0, n as isize - 1) as usize;
    let cols: Vec<(usize, usize)> = (0..crop.width as isize)
        .map(|u| {
            (
                clamp(ix + u, src_width) * 3,
                clamp(ix + u + 1, src_width) * 3,
            )
        })
        .collect();

    for v in 0..crop.height as isize {
        let row0 = clamp(iy + v, src_height) * src_width * 3;
        let row1 = clamp(iy + v + 1, src_height) * src_width * 3;
        for &(c0, c1) in &cols {
            for ch in 0..3 {
                let value = weights[0] * src[row0 + c0 + ch] as f32
                    + weights[1] * src[row0 + c1 + ch] as f32
                    + weights[2] * src[row1 + c0 + ch] as f32
                    + weights[3] * src[row1 + c1 + ch] as f32;
                dst.push((value + 0.5).min(255.0) as u8);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize) -> Vec<u8> {
        (0..h)
            .flat_map(|y| (0..w).flat_map(move |x| [(x * 10) as u8, (y * 10) as u8, 7]))
            .collect()
    }

    #[test]
    fn integer_jitter_samples_shifted_pixels() {
        let src = gradient(10, 8);
        let crop = CropRect {
            x: 2,
            y: 2,
            width: 6,
            height: 4,
        };
        let mut dst = Vec::new();
        shift_and_crop(&src, 10, 8, crop, 1.0, -2.0, &mut dst);
        assert_eq!(dst.len(), 6 * 4 * 3);
        // Output (0,0) samples source (3, 0).
        assert_eq!(&dst[0..3], &[30, 0, 7]);
    }

    #[test]
    fn half_pixel_jitter_interpolates() {
        let src = gradient(10, 8);
        let crop = CropRect {
            x: 2,
            y: 2,
            width: 6,
            height: 4,
        };
        let mut dst = Vec::new();
        shift_and_crop(&src, 10, 8, crop, 0.5, 0.0, &mut dst);
        assert_eq!(&dst[0..3], &[25, 20, 7]);
    }
}
