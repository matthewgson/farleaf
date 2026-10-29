//! Turning a rendered strip into the bytes that put it in the terminal, and
//! the Kitty graphics commands that place and delete it.

use flate2::{write::ZlibEncoder, Compression};
use std::ffi::CString;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    /// The terminal reads our shared memory (it is on this machine): only a
    /// name crosses the pty, and strips are rendered straight into the shared
    /// memory. Never under herdr: it accepts the probe but does not show
    /// images sent this way.
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

/// zlib levels, never 1: zlib-rs level 1 writes fixed-Huffman blocks, and
/// Ghostty 1.3.1 crashes when such an `o=z` image is placed a second time.
/// PNG goes over SSH, where bytes cost more than time; zlib RGB goes to herdr
/// on the same machine, where level 2 is a fifth faster for 3% more bytes
/// (measured on papers and scans, 2026-10-24).
const PNG_LEVEL: u32 = 3;
const ZLIB_LEVEL: u32 = 2;

/// One strip, encoded and wrapped in its transmit command.
pub struct Payload {
    pub bytes: Vec<u8>,
    /// Pixels as the terminal will hold them, for its memory budget.
    pub mem: usize,
    shm: Option<CString>,
    sent: AtomicBool,
}

impl Payload {
    fn new(bytes: Vec<u8>, mem: usize, shm: Option<CString>) -> Payload {
        Payload {
            bytes,
            mem,
            shm,
            sent: AtomicBool::new(false),
        }
    }

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

/// `rgb` is packed RGB, `w`x`h`, for the PNG or zlib transport.
///
/// Every command says q=1: no reply on success, but an error comes back, so a
/// refused or evicted image is noticed and sent again instead of staying blank.
pub fn transmit(t: Transport, id: u32, rgb: &[u8], w: u32, h: u32) -> Payload {
    let mut bytes = Vec::new();
    match t {
        Transport::Zlib => {
            let ctl = format!("a=t,f=24,o=z,s={w},v={h},i={id},q=1");
            chunked(&mut bytes, &ctl, &zlib(rgb, ZLIB_LEVEL));
        }
        Transport::Png | Transport::Shm => {
            chunked(&mut bytes, &format!("a=t,f=100,i={id},q=1"), &png(rgb, w, h));
        }
    }
    Payload::new(bytes, w as usize * h as usize * t.bpp(), None)
}

/// A shared memory object being filled with a strip: render into `pixels()`,
/// then `payload()` hands it to the terminal. Dropped unsent, it is removed.
pub struct ShmBuf {
    name: CString,
    ptr: *mut u8,
    len: usize,
}

// The mapping is ours alone until it is handed over.
unsafe impl Send for ShmBuf {}

impl ShmBuf {
    /// macOS caps names at 31 bytes, so they stay short.
    pub fn new(len: usize) -> Option<ShmBuf> {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let name = CString::new(format!("/fl{}-{n}", std::process::id())).ok()?;
        unsafe {
            let fd = libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600 as libc::c_uint,
            );
            if fd < 0 {
                return None;
            }
            let p = if libc::ftruncate(fd, len as libc::off_t) == 0 {
                let flags = libc::PROT_READ | libc::PROT_WRITE;
                libc::mmap(std::ptr::null_mut(), len.max(1), flags, libc::MAP_SHARED, fd, 0)
            } else {
                libc::MAP_FAILED
            };
            libc::close(fd);
            if p == libc::MAP_FAILED {
                libc::shm_unlink(name.as_ptr());
                return None;
            }
            Some(ShmBuf {
                name,
                ptr: p.cast(),
                len,
            })
        }
    }

    pub fn pixels(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// The command that has the terminal read it as a `w`x`h` RGB image.
    pub fn payload(self, id: u32, w: u32, h: u32) -> Payload {
        let mut bytes = Vec::new();
        let ctl = format!("a=t,f=24,t=s,s={w},v={h},S={},i={id},q=1", self.len);
        apc(&mut bytes, &ctl, self.name.as_bytes());
        unsafe { libc::munmap(self.ptr.cast(), self.len.max(1)) };
        let me = std::mem::ManuallyDrop::new(self);
        // The name moves into the payload, which unlinks it if never sent.
        let name = unsafe { std::ptr::read(&me.name) };
        Payload::new(bytes, w as usize * h as usize * 3, Some(name))
    }
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len.max(1));
            libc::shm_unlink(self.name.as_ptr());
        }
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

/// Puts placement `p` of image `id` one cell big at screen cell (row, col),
/// below the text (z=-1), where an image on screen covers it: never seen,
/// but a multiplexer that sends images on only once placed sends it.
pub fn place_hidden(out: &mut Vec<u8>, id: u32, p: u32, row: u32, col: u32) {
    let _ = write!(
        out,
        "\x1b[{};{}H\x1b_Ga=p,i={id},p={p},x=0,y=0,w=1,h=1,c=1,r=1,z=-1,C=1,q=1\x1b\\",
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

pub fn base64(out: &mut Vec<u8>, data: &[u8]) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (chunks, rest) = data.as_chunks::<3>();
    for &[a, b, c] in chunks {
        let n = (a as u32) << 16 | (b as u32) << 8 | c as u32;
        out.extend_from_slice(&[
            T[(n >> 18) as usize],
            T[(n >> 12 & 63) as usize],
            T[(n >> 6 & 63) as usize],
            T[(n & 63) as usize],
        ]);
    }
    match *rest {
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

pub fn zlib(data: &[u8], level: u32) -> Vec<u8> {
    let mut z = ZlibEncoder::new(Vec::with_capacity(data.len() / 16), Compression::new(level));
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
    let (w, h) = (w as usize, h as usize);
    // One pass both checks for gray and gathers it; a colored pixel stops it.
    let mut raw = Vec::with_capacity((w + 1) * h);
    let gray = rgb.chunks_exact(w * 3).all(|row| {
        raw.push(0);
        row.as_chunks::<3>().0.iter().all(|&[r, g, b]| {
            raw.push(r);
            r == g && g == b
        })
    });
    if !gray {
        raw.clear();
        raw.reserve((w * 3 + 1) * h);
        for row in rgb.chunks_exact(w * 3) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
    }
    let mut out = Vec::with_capacity(raw.len() / 8);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, if gray { 0 } else { 2 }, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &zlib(&raw, PNG_LEVEL));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    // flate2's CRC uses the CPU's CRC instructions where there are any.
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// The 1x1 shared-memory object for the startup probe.
pub fn shm_probe() -> Option<(CString, Vec<u8>)> {
    let mut buf = ShmBuf::new(3)?;
    buf.pixels().fill(0);
    let me = std::mem::ManuallyDrop::new(buf);
    unsafe { libc::munmap(me.ptr.cast(), me.len) };
    let name = unsafe { std::ptr::read(&me.name) };
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
    fn png_chunks_have_standard_crcs() {
        let mut out = Vec::new();
        png_chunk(&mut out, b"IEND", &[]);
        assert_eq!(out[8..], 0xAE42_6082u32.to_be_bytes());
    }

    #[test]
    fn png_is_gray_only_when_every_pixel_is() {
        let gray = [7u8, 7, 7, 9, 9, 9];
        let color = [7u8, 7, 7, 9, 9, 8];
        assert_eq!(png(&gray, 2, 1)[25], 0);
        assert_eq!(png(&color, 2, 1)[25], 2);
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

    #[test]
    fn shared_memory_holds_what_was_written() {
        let mut buf = ShmBuf::new(6).expect("shared memory");
        buf.pixels().copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        let name = buf.name.clone();
        let p = buf.payload(9, 2, 1);
        assert!(String::from_utf8_lossy(&p.bytes).contains("t=s,s=2,v=1,S=6,i=9"));
        unsafe {
            let fd = libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0);
            assert!(fd >= 0);
            let m = libc::mmap(std::ptr::null_mut(), 6, libc::PROT_READ, libc::MAP_SHARED, fd, 0);
            assert_eq!(std::slice::from_raw_parts(m as *const u8, 6), &[1, 2, 3, 4, 5, 6]);
            libc::munmap(m, 6);
            libc::close(fd);
        }
        drop(p); // never sent: unlinked
        assert!(unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) } < 0);
    }
}
