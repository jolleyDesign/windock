//! The Windock logo, drawn in code so building needs no image files or tools.
//! Used by build.rs for the .exe icon and by the settings window.
//!
//! A blue tile with a frosted dock bar and three icons, the middle one magnified.

/// Straight-alpha RGBA pixels, top-down, `size`×`size`.
pub fn rgba(size: u32) -> Vec<u8> {
    let n = size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (px, py) = ((x as f32 + 0.5) / n, (y as f32 + 0.5) / n);
            // Coverage from a signed distance, antialiased over one pixel.
            let cover = |d: f32| (0.5 - d * n).clamp(0.0, 1.0);
            let mut c = [0f32; 4]; // premultiplied
            let mut over = |rgb: [f32; 3], a: f32| {
                for i in 0..3 {
                    c[i] = rgb[i] * a + c[i] * (1.0 - a);
                }
                c[3] = a + c[3] * (1.0 - a);
            };

            let (top, bottom) = ([0.39, 0.55, 1.0], [0.27, 0.20, 0.78]);
            let bg = [0, 1, 2].map(|i| top[i] + (bottom[i] - top[i]) * py);
            over(bg, cover(rounded_rect(px, py, (0.5, 0.5), (0.45, 0.45), 0.2)));
            // The dock bar, and three icons sitting on it.
            over([1.0; 3], 0.32 * cover(rounded_rect(px, py, (0.5, 0.735), (0.31, 0.06), 0.06)));
            for (cx, side, alpha) in [(0.28, 0.15, 0.85), (0.5, 0.22, 1.0), (0.72, 0.15, 0.85)] {
                let center = (cx, 0.70 - side / 2.0);
                over([1.0; 3], alpha * cover(rounded_rect(px, py, center, (side / 2.0, side / 2.0), side * 0.28)));
            }

            let a = c[3];
            let channel = |v: f32| if a > 0.0 { (v / a * 255.0).round().clamp(0.0, 255.0) as u8 } else { 0 };
            out.extend([channel(c[0]), channel(c[1]), channel(c[2]), (a * 255.0).round() as u8]);
        }
    }
    out
}

/// Signed distance from a point to a rounded rectangle (negative inside).
fn rounded_rect(px: f32, py: f32, center: (f32, f32), half: (f32, f32), radius: f32) -> f32 {
    let qx = (px - center.0).abs() - (half.0 - radius);
    let qy = (py - center.1).abs() - (half.1 - radius);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
}
