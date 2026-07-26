//! A CPU rasterizer for egui's tessellated output.
//!
//! This exists because of D7: the GUI renders without OpenGL. eframe is not used —
//! its whole job is binding egui to `glow` (OpenGL) or `wgpu`, and both are ruled out.
//! egui hands us triangles; this turns them into pixels with the CPU and nothing else.
//!
//! Two facts about egui drive the whole file:
//!
//!   * **`Color32` is premultiplied alpha.** Vertex colours and the font atlas both
//!     arrive premultiplied, so the blend is a plain source-over on premultiplied
//!     values: `dst = src + dst * (1 - src.a)`. Multiplying by alpha again anywhere
//!     here would darken every glyph edge.
//!   * **Vertices are in points, not pixels.** They are scaled by `pixels_per_point`
//!     at draw time, along with the clip rectangles.
//!
//! Everything is drawn onto an opaque background, so the destination alpha is always
//! 1 and never needs storing.

use std::collections::HashMap;

use egui::epaint::{ImageDelta, Primitive, Vertex};
use egui::{ClippedPrimitive, Color32, ImageData, Rect, TextureId, TexturesDelta};

/// One texture egui has asked us to hold — in practice the font atlas, plus any
/// images the app registers. Stored premultiplied, exactly as egui produced it.
struct Tex {
    w: usize,
    h: usize,
    px: Vec<Color32>,
}

#[derive(Default)]
pub struct Painter {
    textures: HashMap<TextureId, Tex>,
}

impl Painter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply egui's texture changes. `pos = None` replaces the whole texture; `Some`
    /// patches a sub-rectangle, which is what egui does when the font atlas grows a
    /// glyph — reallocating the atlas on every new character would be wasteful.
    pub fn update_textures(&mut self, delta: &TexturesDelta) {
        for (id, image) in &delta.set {
            self.set(*id, image);
        }
        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    fn set(&mut self, id: TextureId, delta: &ImageDelta) {
        let (w, h, px) = match &delta.image {
            ImageData::Color(img) => (img.size[0], img.size[1], img.pixels.clone()),
            // Font atlases arrive as coverage; `srgba_pixels` converts to premultiplied
            // colour. `None` asks egui for its own default gamma rather than guessing.
            ImageData::Font(img) => (
                img.size[0],
                img.size[1],
                img.srgba_pixels(None).collect::<Vec<_>>(),
            ),
        };

        match delta.pos {
            None => {
                self.textures.insert(id, Tex { w, h, px });
            }
            Some([ox, oy]) => {
                if let Some(tex) = self.textures.get_mut(&id) {
                    for row in 0..h {
                        let dy = oy + row;
                        if dy >= tex.h {
                            break;
                        }
                        for col in 0..w {
                            let dx = ox + col;
                            if dx >= tex.w {
                                break;
                            }
                            tex.px[dy * tex.w + dx] = px[row * w + col];
                        }
                    }
                }
            }
        }
    }

    /// Rasterize a frame into `buf`, a `width * height` framebuffer of `0x00RRGGBB`
    /// as softbuffer expects.
    pub fn paint(
        &self,
        buf: &mut [u32],
        width: usize,
        height: usize,
        pixels_per_point: f32,
        primitives: &[ClippedPrimitive],
    ) {
        for ClippedPrimitive {
            clip_rect,
            primitive,
        } in primitives
        {
            let mesh = match primitive {
                Primitive::Mesh(m) => m,
                // Callbacks are the escape hatch for a GPU renderer to draw custom
                // content. There is no GPU here, so there is nothing sensible to do.
                Primitive::Callback(_) => continue,
            };
            if mesh.indices.is_empty() {
                continue;
            }
            let Some(tex) = self.textures.get(&mesh.texture_id) else {
                continue;
            };
            let clip = scale_rect(*clip_rect, pixels_per_point, width, height);
            if clip.is_empty() {
                continue;
            }

            for tri in mesh.indices.chunks_exact(3) {
                let (a, b, c) = (
                    &mesh.vertices[tri[0] as usize],
                    &mesh.vertices[tri[1] as usize],
                    &mesh.vertices[tri[2] as usize],
                );
                self.triangle(buf, width, pixels_per_point, &clip, tex, a, b, c);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn triangle(
        &self,
        buf: &mut [u32],
        stride: usize,
        ppp: f32,
        clip: &ClipBox,
        tex: &Tex,
        a: &Vertex,
        b: &Vertex,
        c: &Vertex,
    ) {
        let (ax, ay) = (a.pos.x * ppp, a.pos.y * ppp);
        let (bx, by) = (b.pos.x * ppp, b.pos.y * ppp);
        let (cx, cy) = (c.pos.x * ppp, c.pos.y * ppp);

        // Twice the signed area. Zero means degenerate — no pixels to fill. The sign
        // tells us the winding, and dividing by it below normalises either direction,
        // so both windings rasterize without a separate case.
        let area = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
        if area.abs() < f32::EPSILON {
            return;
        }
        let inv_area = 1.0 / area;

        // Compare in i64 and bail *before* the usize casts: a triangle entirely off the
        // left/top edge has a negative max, and `negative as usize` wraps to ~1.8e19 —
        // sailing past the min>=max guard and straight into an out-of-bounds panic.
        let min_xi = (ax.min(bx).min(cx).floor() as i64).max(clip.x0 as i64);
        let max_xi = (ax.max(bx).max(cx).ceil() as i64).min(clip.x1 as i64);
        let min_yi = (ay.min(by).min(cy).floor() as i64).max(clip.y0 as i64);
        let max_yi = (ay.max(by).max(cy).ceil() as i64).min(clip.y1 as i64);
        if min_xi >= max_xi || min_yi >= max_yi {
            return;
        }
        let (min_x, max_x) = (min_xi as usize, max_xi as usize);
        let (min_y, max_y) = (min_yi as usize, max_yi as usize);

        for y in min_y..max_y {
            let py = y as f32 + 0.5;
            for x in min_x..max_x {
                let px = x as f32 + 0.5;

                // Barycentric weights via edge functions, normalised by the signed
                // area so they sum to 1 and are all non-negative inside the triangle.
                let w0 = ((bx - px) * (cy - py) - (by - py) * (cx - px)) * inv_area;
                let w1 = ((cx - px) * (ay - py) - (cy - py) * (ax - px)) * inv_area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }

                let u = w0 * a.uv.x + w1 * b.uv.x + w2 * c.uv.x;
                let v = w0 * a.uv.y + w1 * b.uv.y + w2 * c.uv.y;
                let texel = sample(tex, u, v);
                if texel[3] == 0.0 {
                    continue;
                }

                let vc = [
                    w0 * a.color.r() as f32 + w1 * b.color.r() as f32 + w2 * c.color.r() as f32,
                    w0 * a.color.g() as f32 + w1 * b.color.g() as f32 + w2 * c.color.g() as f32,
                    w0 * a.color.b() as f32 + w1 * b.color.b() as f32 + w2 * c.color.b() as f32,
                    w0 * a.color.a() as f32 + w1 * b.color.a() as f32 + w2 * c.color.a() as f32,
                ];

                // Both operands premultiplied, so this is a straight modulate.
                let sr = vc[0] * texel[0] / 255.0;
                let sg = vc[1] * texel[1] / 255.0;
                let sb = vc[2] * texel[2] / 255.0;
                let sa = vc[3] * texel[3] / 255.0 / 255.0;
                if sa <= 0.0 {
                    continue;
                }

                let idx = y * stride + x;
                let dst = buf[idx];
                let dr = ((dst >> 16) & 0xFF) as f32;
                let dg = ((dst >> 8) & 0xFF) as f32;
                let db = (dst & 0xFF) as f32;

                let inv = 1.0 - sa;
                let r = (sr + dr * inv).clamp(0.0, 255.0) as u32;
                let g = (sg + dg * inv).clamp(0.0, 255.0) as u32;
                let bl = (sb + db * inv).clamp(0.0, 255.0) as u32;
                buf[idx] = (r << 16) | (g << 8) | bl;
            }
        }
    }
}

/// Bilinear sample, returned as premultiplied RGBA in 0..255.
///
/// Bilinear rather than nearest because the window can sit at a fractional scale
/// factor, where nearest makes text shimmer as the atlas is sampled off-centre.
fn sample(tex: &Tex, u: f32, v: f32) -> [f32; 4] {
    if tex.w == 0 || tex.h == 0 {
        return [0.0; 4];
    }
    let x = (u * tex.w as f32 - 0.5).clamp(0.0, tex.w as f32 - 1.0);
    let y = (v * tex.h as f32 - 0.5).clamp(0.0, tex.h as f32 - 1.0);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(tex.w - 1);
    let y1 = (y0 + 1).min(tex.h - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;

    let p = |px: usize, py: usize| -> [f32; 4] {
        let c = tex.px[py * tex.w + px];
        [
            c.r() as f32,
            c.g() as f32,
            c.b() as f32,
            c.a() as f32,
        ]
    };
    let (p00, p10, p01, p11) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));

    let mut out = [0.0f32; 4];
    for i in 0..4 {
        let top = p00[i] + (p10[i] - p00[i]) * fx;
        let bot = p01[i] + (p11[i] - p01[i]) * fx;
        out[i] = top + (bot - top) * fy;
    }
    out
}

struct ClipBox {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl ClipBox {
    fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }
}

fn scale_rect(r: Rect, ppp: f32, width: usize, height: usize) -> ClipBox {
    let x0 = (r.min.x * ppp).floor().max(0.0) as usize;
    let y0 = (r.min.y * ppp).floor().max(0.0) as usize;
    let x1 = ((r.max.x * ppp).ceil().max(0.0) as usize).min(width);
    let y1 = ((r.max.y * ppp).ceil().max(0.0) as usize).min(height);
    ClipBox { x0, y0, x1, y1 }
}
