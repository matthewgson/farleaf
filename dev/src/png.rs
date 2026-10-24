//! Just enough PNG: reading what terminals are sent (8-bit gray, RGB, RGBA,
//! any filter) and writing screenshots.

use std::io::Read;

const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Width and height from a PNG's header, if it is one.
pub fn info(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || &data[..8] != SIG || &data[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
    Some((be(16), be(20)))
}

/// Width, height and packed RGB.
pub fn decode(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let (w, h) = info(data).ok_or("not a PNG")?;
    let (depth, ctype) = (data[24], data[25]);
    let bpp = match (depth, ctype) {
        (8, 0) => 1,
        (8, 2) => 3,
        (8, 6) => 4,
        _ => return Err(format!("PNG depth {depth}, color type {ctype} not supported")),
    };
    let mut idat = Vec::new();
    let mut i = 8;
    while i + 8 <= data.len() {
        let n = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        let kind = &data[i + 4..i + 8];
        let body = data.get(i + 8..i + 8 + n).ok_or("truncated PNG")?;
        if kind == b"IDAT" {
            idat.extend_from_slice(body);
        }
        i += 12 + n;
    }
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&idat[..])
        .read_to_end(&mut raw)
        .map_err(|e| format!("PNG data: {e}"))?;
    let stride = w as usize * bpp;
    if raw.len() < (stride + 1) * h as usize {
        return Err("PNG data too short".into());
    }
    let mut px = vec![0u8; stride * h as usize];
    let mut prev = vec![0u8; stride];
    for y in 0..h as usize {
        let f = raw[y * (stride + 1)];
        let src = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        let row = &mut px[y * stride..(y + 1) * stride];
        for x in 0..stride {
            let a = if x >= bpp { row[x - bpp] as i32 } else { 0 };
            let b = prev[x] as i32;
            let c = if x >= bpp { prev[x - bpp] as i32 } else { 0 };
            let pred = match f {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => return Err(format!("PNG filter {f}")),
            };
            row[x] = src[x].wrapping_add(pred as u8);
        }
        prev.copy_from_slice(row);
    }
    Ok((w, h, to_rgb(&px, bpp)))
}

/// Packed RGB from gray (1), RGB (3) or RGBA (4) bytes per pixel.
pub fn to_rgb(px: &[u8], bpp: usize) -> Vec<u8> {
    match bpp {
        3 => px.to_vec(),
        1 => px.iter().flat_map(|&v| [v, v, v]).collect(),
        _ => px.chunks_exact(bpp).flat_map(|p| [p[0], p[1], p[2]]).collect(),
    }
}

/// An RGB PNG, written fast (no filter, light compression).
pub fn encode(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for row in rgb.chunks_exact(w as usize * 3) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(2));
    std::io::Write::write_all(&mut z, &raw).expect("writing to a Vec");
    let idat = z.finish().expect("writing to a Vec");
    let mut out = SIG.to_vec();
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let rgb: Vec<u8> = (0..4 * 3 * 3).map(|i| (i * 7) as u8).collect();
        let png = super::encode(4, 3, &rgb);
        assert_eq!(super::info(&png), Some((4, 3)));
        assert_eq!(super::decode(&png).unwrap(), (4, 3, rgb));
    }
}
