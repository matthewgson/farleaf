//! Turning a rendered strip into the bytes that put it in the terminal, and
//! the Kitty graphics commands that place and delete it.

use flate2::{write::ZlibEncoder, Compression};
use std::ffi::CString;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    /// The terminal reads our shared memory (it is on this machine): only a
    /// name crosses the pty. Never under herdr: it accepts the probe but does
    /// not show images sent this way.
    Shm,
    /// The terminal is elsewhere (SSH): PNG, 8-bit grayscale when the strip
    /// has no color, RGB otherwise. Ghostty decodes PNG with its own robust
    /// decoder, so no zlib-stream quirk can reach its `o=z` crash.
    Png,
    /// herdr: zlib-compressed RGB (`o=z`). herdr inflates every image and
    /// re-sends it raw in its stored format; PNG would be stored as RGBA, a
    /// third more bytes on the wire. herdr 0.9.3 decodes even all-fixed-Huffman
    /// streams exactly (tested 2026-10-14), and Ghostty never sees `o=z` here.
    Zlib,
}

impl Transport {
    /// Bytes per pixel the terminal keeps: Ghostty and herdr store PNGs
    /// decoded to RGBA, raw RGB as it comes.
    pub fn bpp(self) -> usize {
        match self {
            Transport::Png => 4,
            Transport::Shm | Transport::Zlib => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Transport::Shm => "shm",
            Transport::Png => "png",
            Transport::Zlib => "zlib",
        }
    }
}

/// Level 3, never 1: zlib-rs level 1 writes fixed-Huffman blocks, and Ghostty
/// 1.3.1 crashes when such an `o=z` image is placed a second time.
const LEVEL: u32 = 3;

/// One strip, encoded and wrapped in its transmit command.
pub struct Payload {
    pub bytes: Vec<u8>,
    /// Pixels as the terminal will hold them, for its memory budget.
    pub mem: usize,
    shm: Option<CString>,
    sent: AtomicBool,
}

impl Payload {
    /// Once written, the shared memory belongs to the terminal, which
    /// unlinks it after reading.
    pub fn mark_sent(&self) {
        self.sent.store(true, Ordering::Release);
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        if let Some(name) = &self.shm {
            if !self.sent.load(Ordering::Acquire) {
                unsafe { libc::shm_unlink(name.as_ptr()) };
            }
        }
    }
}

/// `rgb` is packed RGB, `w`x`h`. Falls back to PNG if shared memory fails.
///
/// Every command says q=1: no reply on success, but an error comes back, so a
/// refused or evicted image is noticed and sent again instead of staying blank.
pub fn transmit(t: Transport, id: u32, rgb: &[u8], w: u32, h: u32) -> Payload {
    let mem = w as usize * h as usize * t.bpp();
    let mut bytes = Vec::new();
    match t {
        Transport::Shm => {
            if let Some(name) = shm_put(rgb) {
                let ctl = format!("a=t,f=24,t=s,s={w},v={h},S={},i={id},q=1", rgb.len());
                apc(&mut bytes, &ctl, name.as_bytes());
                return Payload {
                    bytes,
                    mem,
                    shm: Some(name),
                    sent: AtomicBool::new(false),
                };
            }
            return transmit(Transport::Png, id, rgb, w, h);
        }
        Transport::Png => {
            let png = png(rgb, w, h);
            chunked(&mut bytes, &format!("a=t,f=100,i={id},q=1"), &png);
        }
        Transport::Zlib => {
            chunked(&mut bytes, &format!("a=t,f=24,o=z,s={w},v={h},i={id},q=1"), &zlib(rgb));
        }
    }
    Payload {
        bytes,
        mem,
        shm: None,
        sent: AtomicBool::new(false),
    }
}

/// Puts placement `p` of image `id`; `at` is its top-left screen cell (row,
/// col), the source pixels shown (x, y, w, h), and the cells they cover
/// (cols, rows). An image has one placement, or several around a popup.
pub fn place(out: &mut Vec<u8>, id: u32, p: u32, at: &[u32; 8]) {
    let [row, col, x, y, w, h, cols, rows] = *at;
    let _ = write!(
        out,
        "\x1b[{};{}H\x1b_Ga=p,i={id},p={p},x={x},y={y},w={w},h={h},c={cols},r={rows},C=1,q=1\x1b\\",
        row + 1,
        col + 1
    );
}

/// Removes placement `p`, keeping the image for a later re-place.
pub fn unplace(out: &mut Vec<u8>, id: u32, p: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=i,i={id},p={p},q=2\x1b\\");
}

/// Removes the image and its data.
pub fn free(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
}

fn apc(out: &mut Vec<u8>, ctl: &str, data: &[u8]) {
    out.extend_from_slice(b"\x1b_G");
    out.extend_from_slice(ctl.as_bytes());
    out.push(b';');
    base64(out, data);
    out.extend_from_slice(b"\x1b\\");
}

/// Kitty's chunked direct transmission: 4096 base64 bytes per command, the
/// control keys on the first one only.
fn chunked(out: &mut Vec<u8>, ctl: &str, data: &[u8]) {
    const RAW: usize = 4096 / 4 * 3;
    out.reserve(data.len() / 3 * 4 + data.len() / RAW * 24 + 64);
    let n = data.len().div_ceil(RAW).max(1);
    for (i, chunk) in data.chunks(RAW).enumerate() {
        let more = u8::from(i + 1 < n);
        out.extend_from_slice(b"\x1b_G");
        if i == 0 {
            out.extend_from_slice(ctl.as_bytes());
            out.push(b',');
        }
        out.extend_from_slice(b"m=");
        out.push(b'0' + more);
        out.push(b';');
        base64(out, chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

fn base64(out: &mut Vec<u8>, data: &[u8]) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut it = data.chunks_exact(3);
    for c in &mut it {
        let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        out.extend_from_slice(&[
            T[(n >> 18) as usize],
            T[(n >> 12 & 63) as usize],
            T[(n >> 6 & 63) as usize],
            T[(n & 63) as usize],
        ]);
    }
    match *it.remainder() {
        [a] => {
            let n = (a as u32) << 16;
            out.extend_from_slice(&[T[(n >> 18) as usize], T[(n >> 12 & 63) as usize], b'=', b'=']);
        }
        [a, b] => {
            let n = (a as u32) << 16 | (b as u32) << 8;
            out.extend_from_slice(&[
                T[(n >> 18) as usize],
                T[(n >> 12 & 63) as usize],
                T[(n >> 6 & 63) as usize],
                b'=',
            ]);
        }
        _ => {}
    }
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut z = ZlibEncoder::new(Vec::with_capacity(data.len() / 16), Compression::new(LEVEL));
    z.write_all(data).expect("writing to a Vec cannot fail");
    z.finish().expect("writing to a Vec cannot fail")
}

/// A minimal PNG: one IDAT, no row filter, grayscale if R=G=B everywhere.
///
/// Measured on real papers: no filter beats Sub, Up and Paeth by 10-20% on
/// rendered text, and RGB PNG is then the size of plain zlib. Whole pages are
/// almost never pure gray (antialiasing leaves a few tinted pixels), but half
/// to three quarters of strips are, and those come out about 3x smaller.
pub fn png(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    let gray = rgb.chunks_exact(3).all(|p| p[0] == p[1] && p[1] == p[2]);
    let row = w as usize * if gray { 1 } else { 3 };
    let mut raw = Vec::with_capacity((row + 1) * h as usize);
    for src in rgb.chunks_exact(w as usize * 3) {
        raw.push(0);
        if gray {
            raw.extend(src.iter().step_by(3));
        } else {
            raw.extend_from_slice(src);
        }
    }
    let mut out = Vec::with_capacity(raw.len() / 8);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, if gray { 0 } else { 2 }, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &zlib(&raw));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(crc32(0xFFFF_FFFF, kind), data) ^ 0xFFFF_FFFF;
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(mut crc: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    for &b in data {
        crc = t[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// Writes `data` to a new POSIX shared memory object and returns its name.
/// macOS caps names at 31 bytes, so they stay short.
fn shm_put(data: &[u8]) -> Option<CString> {
    static N: AtomicU32 = AtomicU32::new(0);
    let name = CString::new(format!(
        "/fl{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
    .ok()?;
    unsafe {
        let fd = libc::shm_open(
            name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600 as libc::c_uint,
        );
        if fd < 0 {
            return None;
        }
        let ok = libc::ftruncate(fd, data.len() as libc::off_t) == 0 && {
            let p = libc::mmap(
                std::ptr::null_mut(),
                data.len(),
                libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            p != libc::MAP_FAILED && {
                std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u8>(), data.len());
                libc::munmap(p, data.len());
                true
            }
        };
        libc::close(fd);
        if !ok {
            libc::shm_unlink(name.as_ptr());
            return None;
        }
    }
    Some(name)
}

/// The 1x1 shared-memory object for the startup probe.
pub fn shm_probe() -> Option<(CString, Vec<u8>)> {
    let name = shm_put(&[0, 0, 0])?;
    let mut cmd = Vec::new();
    apc(&mut cmd, "i=2,a=q,t=s,f=24,s=1,v=1,S=3", name.as_bytes());
    Some((name, cmd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            let mut out = Vec::new();
            base64(&mut out, raw.as_bytes());
            assert_eq!(out, enc.as_bytes());
        }
    }

    #[test]
    fn crc_of_iend_is_standard() {
        assert_eq!(crc32(0xFFFF_FFFF, b"IEND") ^ 0xFFFF_FFFF, 0xAE42_6082);
    }

    #[test]
    fn chunks_carry_control_only_first() {
        let mut out = Vec::new();
        chunked(&mut out, "a=t", &vec![7u8; 5000]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("\x1b_Ga=t,m=1;"));
        assert!(s.contains("\x1b\\\x1b_Gm=0;"));
        assert_eq!(s.matches("\x1b_G").count(), 2);
    }
}
