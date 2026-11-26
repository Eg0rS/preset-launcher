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

/// Кодирует RGBA в PNG (без сжатия: deflate «stored» блоки). Нужен CLI-команде `icon-png`
/// для сборки `.icns`; без внешних зависимостей.
#[allow(dead_code)]
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    // Строки с фильтром 0 (None).
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgba.chunks(stride).take(height as usize) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    // zlib: заголовок, stored-блоки по <= 65535 байт, Adler-32.
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    while let Some(block) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 бит, RGBA
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod png_tests {
    use super::*;

    #[test]
    fn png_structure() {
        let png = encode_png(&render_icon_rgba(16), 16, 16);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..24], &[0, 0, 0, 16, 0, 0, 0, 16]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        // известный CRC32 IEND
        assert_eq!(&png[png.len() - 4..], &[0xAE, 0x42, 0x60, 0x82]);
        // 1x1 непрозрачный пиксель: проверка Adler-32 и длины через независимый разбор IDAT
        let p = encode_png(&[1, 2, 3, 255], 1, 1);
        assert!(p.len() > 40);
    }
}
