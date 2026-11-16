//! Draws egui's triangles on the CPU. A settings form is a few thousand small triangles, so
//! this is quick enough, and it keeps a GPU driver (100+ MB) out of the process.
//!
//! Like egui's own renderers: colors are premultiplied sRGB, blended in gamma space.

use egui::epaint::textures::TexturesDelta;
use egui::epaint::{ClippedPrimitive, ImageData, Mesh, Primitive, TextureId, Vertex};
use egui::{Color32, Rect};
use std::collections::HashMap;

/// Vertex positions are snapped to 1/256 px, so shared edges line up exactly and no pixel
/// along them is drawn twice (or skipped).
const SUB: i64 = 256;

type Rgba = [f32; 4];

struct Texture {
    size: [usize; 2],
    pixels: Vec<Color32>,
}

#[derive(Default)]
pub struct Raster {
    textures: HashMap<TextureId, Texture>,
    /// Top-down BGRA rows, as a 32-bit DIB wants them.
    pub pixels: Vec<[u8; 4]>,
    pub width: usize,
    pub height: usize,
}

impl Raster {
    /// New and updated textures; call before `paint`.
    pub fn set_textures(&mut self, delta: &TexturesDelta) {
        for (id, deltas) in &delta.set {
            for d in deltas {
                let ImageData::Color(image) = &d.image;
                match d.pos {
                    None => {
                        self.textures.insert(*id, Texture { size: image.size, pixels: image.pixels.clone() });
                    }
                    Some([x, y]) => {
                        let Some(t) = self.textures.get_mut(id) else { continue };
                        let [w, h] = image.size;
                        for row in 0..h {
                            let dst = (y + row) * t.size[0] + x;
                            t.pixels[dst..dst + w].copy_from_slice(&image.pixels[row * w..(row + 1) * w]);
                        }
                    }
                }
            }
        }
    }

    /// Textures egui is done with; call after `paint`.
    pub fn free_textures(&mut self, delta: &TexturesDelta) {
        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    pub fn paint(&mut self, width: usize, height: usize, pixels_per_point: f32, primitives: &[ClippedPrimitive], clear: Color32) {
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.resize(width * height, [clear.b(), clear.g(), clear.r(), 255]);
        for p in primitives {
            if let Primitive::Mesh(mesh) = &p.primitive {
                self.mesh(mesh, p.clip_rect, pixels_per_point);
            }
        }
    }

    fn mesh(&mut self, mesh: &Mesh, clip: Rect, ppp: f32) {
        let Some(tex) = self.textures.get(&mesh.texture_id) else { return };
        // Rounded the same way egui_glow rounds its scissor rectangle.
        let px = |v: f32, max: usize| ((v * ppp).round().max(0.0) as usize).min(max);
        let clip = [px(clip.min.x, self.width), px(clip.min.y, self.height), px(clip.max.x, self.width), px(clip.max.y, self.height)];
        if clip[0] >= clip[2] || clip[1] >= clip[3] {
            return;
        }
        for tri in mesh.indices.as_chunks::<3>().0.iter() {
            let v = [tri[0], tri[1], tri[2]].map(|i| &mesh.vertices[i as usize]);
            triangle(&mut self.pixels, self.width, tex, v, ppp, clip);
        }
    }
}

/// Twice the signed area of (a, b, p); positive when p is on the inside of edge a→b.
fn edge(a: [i64; 2], b: [i64; 2], p: [i64; 2]) -> i64 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

/// Tie-break for pixel centres exactly on an edge. Two triangles sharing an edge walk it in
/// opposite directions, so exactly one of them claims those pixels.
fn owns_edge(a: [i64; 2], b: [i64; 2]) -> bool {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    dy < 0 || (dy == 0 && dx > 0)
}

fn triangle(pixels: &mut [[u8; 4]], stride: usize, tex: &Texture, mut v: [&Vertex; 3], ppp: f32, clip: [usize; 4]) {
    let scale = ppp * SUB as f32;
    let mut p = v.map(|v| [(v.pos.x * scale).round() as i64, (v.pos.y * scale).round() as i64]);
    let area = edge(p[0], p[1], p[2]);
    if area == 0 {
        return;
    }
    if area < 0 {
        p.swap(1, 2);
        v.swap(1, 2);
    }
    let area = area.abs() as f32;

    let bound = |axis: usize, lo: usize, hi: usize| {
        let min = p.iter().map(|q| q[axis]).min().unwrap_or(0).div_euclid(SUB);
        let max = p.iter().map(|q| q[axis]).max().unwrap_or(0).div_euclid(SUB) + 1;
        (min.max(lo as i64), max.min(hi as i64))
    };
    let (x0, x1) = bound(0, clip[0], clip[2]);
    let (y0, y1) = bound(1, clip[1], clip[3]);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    // Edge i is opposite vertex i, so its value (over `area`) is vertex i's weight.
    let edges = [(p[1], p[2]), (p[2], p[0]), (p[0], p[1])];
    let first = [x0 * SUB + SUB / 2, y0 * SUB + SUB / 2];
    let mut row = [0; 3];
    let mut step_x = [0; 3];
    let mut step_y = [0; 3];
    for (i, &(a, b)) in edges.iter().enumerate() {
        row[i] = edge(a, b, first) - if owns_edge(a, b) { 0 } else { 1 };
        step_x[i] = -(b[1] - a[1]) * SUB;
        step_y[i] = (b[0] - a[0]) * SUB;
    }

    let colors = v.map(|v| rgba(v.color));
    // In texel units, offset so that texel centres land on whole numbers.
    let uvs = v.map(|v| [v.uv.x * tex.size[0] as f32 - 0.5, v.uv.y * tex.size[1] as f32 - 0.5]);
    // Most triangles are one solid colour: shade them once.
    let flat = (v[0].color == v[1].color && v[0].color == v[2].color && v[0].uv == v[1].uv && v[0].uv == v[2].uv).then(|| mul(colors[0], tex.sample(uvs[0])));

    for y in y0..y1 {
        let line = &mut pixels[y as usize * stride..(y as usize + 1) * stride];
        let mut w = row;
        let mut entered = false;
        for x in x0..x1 {
            if w[0] >= 0 && w[1] >= 0 && w[2] >= 0 {
                entered = true;
                let src = flat.unwrap_or_else(|| {
                    let l = w.map(|w| w as f32 / area);
                    let lerp = |a: f32, b: f32, c: f32| a * l[0] + b * l[1] + c * l[2];
                    let color = std::array::from_fn(|i| lerp(colors[0][i], colors[1][i], colors[2][i]));
                    let uv = std::array::from_fn(|i| lerp(uvs[0][i], uvs[1][i], uvs[2][i]));
                    mul(color, tex.sample(uv))
                });
                blend(&mut line[x as usize], src);
            } else if entered {
                break; // triangles are convex: nothing more on this row
            }
            for i in 0..3 {
                w[i] += step_x[i];
            }
        }
        for i in 0..3 {
            row[i] += step_y[i];
        }
    }
}

impl Texture {
    /// Bilinear, clamped to the edges.
    fn sample(&self, [u, v]: [f32; 2]) -> Rgba {
        let [w, h] = self.size;
        if w == 0 || h == 0 {
            return [255.0; 4];
        }
        let (x, y) = (u.floor(), v.floor());
        let (fx, fy) = (u - x, v - y);
        let at = |dx: i64, dy: i64| {
            let x = (x as i64 + dx).clamp(0, w as i64 - 1) as usize;
            let y = (y as i64 + dy).clamp(0, h as i64 - 1) as usize;
            rgba(self.pixels[y * w + x])
        };
        let top = mix(at(0, 0), at(1, 0), fx);
        let bottom = mix(at(0, 1), at(1, 1), fx);
        mix(top, bottom, fy)
    }
}

fn rgba(c: Color32) -> Rgba {
    [c.r() as f32, c.g() as f32, c.b() as f32, c.a() as f32]
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

fn mul(a: Rgba, b: Rgba) -> Rgba {
    std::array::from_fn(|i| a[i] * b[i] / 255.0)
}

/// Premultiplied "over" onto a BGRA pixel.
fn blend(dst: &mut [u8; 4], src: Rgba) {
    let keep = 1.0 - src[3] / 255.0;
    if keep <= 0.002 {
        *dst = [src[2] as u8, src[1] as u8, src[0] as u8, 255];
        return;
    }
    let over = |s: f32, d: u8| (s + d as f32 * keep).round().min(255.0) as u8;
    *dst = [over(src[2], dst[0]), over(src[1], dst[1]), over(src[0], dst[2]), 255];
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::TextureOptions;
    use egui::epaint::WHITE_UV;
    use egui::{ColorImage, pos2};

    fn raster_with_white_texture() -> Raster {
        let mut r = Raster::default();
        let mut delta = TexturesDelta::default();
        let image = ColorImage::new([2, 2], vec![Color32::WHITE; 4]);
        delta.set.insert(TextureId::default(), [egui::epaint::ImageDelta::full(image, TextureOptions::LINEAR)].into());
        r.set_textures(&delta);
        r
    }

    fn rect_mesh(rect: Rect, color: Color32) -> ClippedPrimitive {
        let mut mesh = Mesh::default();
        mesh.add_colored_rect(rect, color);
        for v in &mut mesh.vertices {
            v.uv = WHITE_UV;
        }
        ClippedPrimitive { clip_rect: Rect::EVERYTHING, primitive: Primitive::Mesh(mesh) }
    }

    #[test]
    fn fills_a_rect_exactly() {
        let mut r = raster_with_white_texture();
        let rect = Rect::from_min_max(pos2(2.0, 1.0), pos2(6.0, 5.0));
        r.paint(8, 6, 1.0, &[rect_mesh(rect, Color32::RED)], Color32::BLACK);
        for y in 0..6 {
            for x in 0..8 {
                let inside = (2..6).contains(&x) && (1..5).contains(&y);
                let expect = if inside { [0, 0, 255, 255] } else { [0, 0, 0, 255] };
                assert_eq!(r.pixels[y * 8 + x], expect, "pixel {x},{y}");
            }
        }
    }

    #[test]
    fn translucent_square_has_no_seam_on_its_diagonal() {
        let mut r = raster_with_white_texture();
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(4.0, 4.0));
        // Half-transparent white, premultiplied.
        let color = Color32::from_rgba_premultiplied(128, 128, 128, 128);
        r.paint(4, 4, 1.0, &[rect_mesh(rect, color)], Color32::BLACK);
        assert!(r.pixels.iter().all(|&p| p == [128, 128, 128, 255]), "{:?}", r.pixels);
    }

    #[test]
    fn respects_clip_rect_and_scale() {
        let mut r = raster_with_white_texture();
        let mut prim = rect_mesh(Rect::from_min_max(pos2(0.0, 0.0), pos2(4.0, 4.0)), Color32::GREEN);
        prim.clip_rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 4.0));
        r.paint(8, 8, 2.0, &[prim], Color32::BLACK);
        let green = r.pixels.iter().filter(|&&p| p == [0, 255, 0, 255]).count();
        assert_eq!(green, 2 * 8);
    }
}
