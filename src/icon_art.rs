// Procedural app icon shared between build.rs (-> .ico embedded into the exe)
// and the app itself (window icon). No binary assets in the repo.
//
// Design: dark rounded tile with three stacked "layers" (presets).

/// Renders the icon as straight (non-premultiplied) RGBA, row-major, top-down.
#[allow(dead_code)]
pub fn render_icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];

    // Shapes in unit coordinates (0..1).
    let tile = RoundRect { cx: 0.5, cy: 0.5, hw: 0.47, hh: 0.47, r: 0.20 };
    let bars = [
        (RoundRect { cx: 0.5, cy: 0.30, hw: 0.30, hh: 0.075, r: 0.075 }, [0x3c, 0xd6, 0xb0]), // teal
        (RoundRect { cx: 0.5, cy: 0.50, hw: 0.30, hh: 0.075, r: 0.075 }, [0xf2, 0xb1, 0x3c]), // amber
        (RoundRect { cx: 0.5, cy: 0.70, hw: 0.30, hh: 0.075, r: 0.075 }, [0xef, 0x6a, 0x5a]), // coral
    ];
    let tile_top = [0x2b, 0x31, 0x3c];
    let tile_bottom = [0x1b, 0x1f, 0x27];

    // 4x4 supersampling for clean edges at 16px.
    const SS: u32 = 4;
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0f32; 4]; // premultiplied accumulation
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / s;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / s;
                    let mut px = [0f32; 4];
                    if tile.contains(u, v) {
                        let c = lerp3(tile_top, tile_bottom, v.clamp(0.0, 1.0));
                        px = [c[0], c[1], c[2], 1.0];
                        for (bar, color) in &bars {
                            if bar.contains(u, v) {
                                px = [
                                    color[0] as f32 / 255.0,
                                    color[1] as f32 / 255.0,
                                    color[2] as f32 / 255.0,
                                    1.0,
                                ];
                            }
                        }
                    }
                    for i in 0..3 {
                        acc[i] += px[i] * px[3];
                    }
                    acc[3] += px[3];
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            let idx = ((y * size + x) * 4) as usize;
            if a > 0.0 {
                for i in 0..3 {
                    out[idx + i] = ((acc[i] / n / a) * 255.0).round().clamp(0.0, 255.0) as u8;
                }
            }
            out[idx + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}

struct RoundRect {
    cx: f32,
    cy: f32,
    hw: f32,
    hh: f32,
    r: f32,
}

impl RoundRect {
    fn contains(&self, u: f32, v: f32) -> bool {
        let qx = (u - self.cx).abs() - (self.hw - self.r);
        let qy = (v - self.cy).abs() - (self.hh - self.r);
        let ox = qx.max(0.0);
        let oy = qy.max(0.0);
        let d = (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - self.r;
        d <= 0.0
    }
}

fn lerp3(a: [u8; 3], b: [u8; 3], t: f32) -> [f32; 3] {
    let f = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t) / 255.0;
    [f(0), f(1), f(2)]
}
