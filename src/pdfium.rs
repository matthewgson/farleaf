//! Minimal runtime binding to pdfium: only the ~30 functions farleaf calls.
//!
//! pdfium is not thread-safe. Only the render worker (src/worker.rs) touches
//! it, so nothing here locks.

use libloading::Library;
use std::ffi::{c_char, c_int, c_ulong, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

type H = *mut c_void;

struct Fns {
    init: unsafe extern "C" fn(),
    load_mem: unsafe extern "C" fn(*const c_void, usize, *const c_char) -> H,
    close_doc: unsafe extern "C" fn(H),
    last_error: unsafe extern "C" fn() -> c_ulong,
    page_count: unsafe extern "C" fn(H) -> c_int,
    page_size: unsafe extern "C" fn(H, c_int, *mut f64, *mut f64) -> c_int,
    load_page: unsafe extern "C" fn(H, c_int) -> H,
    close_page: unsafe extern "C" fn(H),
    bmp_create: unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void, c_int) -> H,
    bmp_fill: unsafe extern "C" fn(H, c_int, c_int, c_int, c_int, c_ulong) -> c_int,
    bmp_buffer: unsafe extern "C" fn(H) -> *mut c_void,
    bmp_stride: unsafe extern "C" fn(H) -> c_int,
    bmp_destroy: unsafe extern "C" fn(H),
    render: unsafe extern "C" fn(H, H, c_int, c_int, c_int, c_int, c_int, c_int),
    page_to_device:
        unsafe extern "C" fn(H, c_int, c_int, c_int, c_int, c_int, f64, f64, *mut c_int, *mut c_int) -> c_int,
    text_load: unsafe extern "C" fn(H) -> H,
    text_close: unsafe extern "C" fn(H),
    find_start: unsafe extern "C" fn(H, *const u16, c_ulong, c_int) -> H,
    find_next: unsafe extern "C" fn(H) -> c_int,
    find_index: unsafe extern "C" fn(H) -> c_int,
    find_count: unsafe extern "C" fn(H) -> c_int,
    find_close: unsafe extern "C" fn(H),
    count_rects: unsafe extern "C" fn(H, c_int, c_int) -> c_int,
    get_rect: unsafe extern "C" fn(H, c_int, *mut f64, *mut f64, *mut f64, *mut f64) -> c_int,
    bm_first_child: unsafe extern "C" fn(H, H) -> H,
    bm_next_sibling: unsafe extern "C" fn(H, H) -> H,
    bm_title: unsafe extern "C" fn(H, *mut c_void, c_ulong) -> c_ulong,
    bm_dest: unsafe extern "C" fn(H, H) -> H,
    bm_action: unsafe extern "C" fn(H) -> H,
    action_dest: unsafe extern "C" fn(H, H) -> H,
    dest_page: unsafe extern "C" fn(H, H) -> c_int,
    dest_location: unsafe extern "C" fn(H, *mut c_int, *mut c_int, *mut c_int, *mut f32, *mut f32, *mut f32) -> c_int,
}

struct Pdfium {
    _lib: Library,
    f: Fns,
}

// Function pointers and a library handle; safe to share. (Calls still happen
// on one thread only.)
unsafe impl Send for Pdfium {}
unsafe impl Sync for Pdfium {}

static PDFIUM: OnceLock<Pdfium> = OnceLock::new();

const LIB_NAME: &str = if cfg!(target_os = "macos") {
    "libpdfium.dylib"
} else {
    "libpdfium.so"
};

/// Where to look for the library, in order. FARLEAF_PDFIUM names the file
/// itself; the last entry is the repo's vendor/ copy, for `cargo run`.
fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = std::env::var_os("FARLEAF_PDFIUM") {
        v.push(PathBuf::from(p));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.canonicalize().ok()?.parent().map(PathBuf::from))
    {
        v.push(dir.join(LIB_NAME));
        v.push(dir.join("../lib/farleaf").join(LIB_NAME)); // where install.sh puts it
        v.push(dir.join("../lib").join(LIB_NAME));
    }
    if let Some(home) = std::env::var_os("HOME") {
        v.push(PathBuf::from(&home).join(".local/lib/farleaf").join(LIB_NAME));
        v.push(PathBuf::from(home).join(".local/lib").join(LIB_NAME));
    }
    v.push(PathBuf::from(LIB_NAME)); // the dynamic loader's own search path
    let os = if cfg!(target_os = "macos") { "mac" } else { "linux" };
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
    let platform = format!("{os}-{arch}");
    v.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("vendor")
            .join(format!("pdfium-{platform}/lib"))
            .join(LIB_NAME),
    );
    v
}

pub fn init() -> Result<(), String> {
    if PDFIUM.get().is_some() {
        return Ok(());
    }
    let mut tried = Vec::new();
    for path in candidates() {
        let is_bare = path.components().count() == 1;
        if !is_bare && !path.exists() {
            continue;
        }
        match unsafe { Library::new(&path) } {
            Ok(lib) => {
                let f = unsafe { bind(&lib) }.map_err(|e| format!("{}: {e}", path.display()))?;
                unsafe { (f.init)() };
                let _ = PDFIUM.set(Pdfium { _lib: lib, f });
                return Ok(());
            }
            Err(e) => tried.push(format!("  {}: {e}", path.display())),
        }
    }
    Err(format!(
        "could not load pdfium ({LIB_NAME}).\n\
         Reinstall with install.sh (https://github.com/matthewgson/farleaf), or fetch it with\n\
         scripts/fetch-pdfium.sh and copy lib/{LIB_NAME} to ~/.local/lib/farleaf/,\n\
         or point FARLEAF_PDFIUM at the file.{}{}",
        if tried.is_empty() { "" } else { "\nTried:\n" },
        tried.join("\n")
    ))
}

unsafe fn bind(lib: &Library) -> Result<Fns, libloading::Error> {
    macro_rules! sym {
        ($name:literal) => {
            *lib.get(concat!($name, "\0").as_bytes())?
        };
    }
    Ok(Fns {
        init: sym!("FPDF_InitLibrary"),
        load_mem: sym!("FPDF_LoadMemDocument64"),
        close_doc: sym!("FPDF_CloseDocument"),
        last_error: sym!("FPDF_GetLastError"),
        page_count: sym!("FPDF_GetPageCount"),
        page_size: sym!("FPDF_GetPageSizeByIndex"),
        load_page: sym!("FPDF_LoadPage"),
        close_page: sym!("FPDF_ClosePage"),
        bmp_create: sym!("FPDFBitmap_CreateEx"),
        bmp_fill: sym!("FPDFBitmap_FillRect"),
        bmp_buffer: sym!("FPDFBitmap_GetBuffer"),
        bmp_stride: sym!("FPDFBitmap_GetStride"),
        bmp_destroy: sym!("FPDFBitmap_Destroy"),
        render: sym!("FPDF_RenderPageBitmap"),
        page_to_device: sym!("FPDF_PageToDevice"),
        text_load: sym!("FPDFText_LoadPage"),
        text_close: sym!("FPDFText_ClosePage"),
        find_start: sym!("FPDFText_FindStart"),
        find_next: sym!("FPDFText_FindNext"),
        find_index: sym!("FPDFText_GetSchResultIndex"),
        find_count: sym!("FPDFText_GetSchCount"),
        find_close: sym!("FPDFText_FindClose"),
        count_rects: sym!("FPDFText_CountRects"),
        get_rect: sym!("FPDFText_GetRect"),
        bm_first_child: sym!("FPDFBookmark_GetFirstChild"),
        bm_next_sibling: sym!("FPDFBookmark_GetNextSibling"),
        bm_title: sym!("FPDFBookmark_GetTitle"),
        bm_dest: sym!("FPDFBookmark_GetDest"),
        bm_action: sym!("FPDFBookmark_GetAction"),
        action_dest: sym!("FPDFAction_GetDest"),
        dest_page: sym!("FPDFDest_GetDestPageIndex"),
        dest_location: sym!("FPDFDest_GetLocationInPage"),
    })
}

fn f() -> &'static Fns {
    &PDFIUM.get().expect("pdfium::init not called").f
}

/// An open document. pdfium reads it from memory, never from the file, so a
/// LaTeX run rewriting the file underneath cannot corrupt what is on screen.
pub struct Doc {
    h: H,
    _bytes: Vec<u8>,
}

impl Doc {
    pub fn open(bytes: Vec<u8>) -> Result<Doc, String> {
        let h = unsafe { (f().load_mem)(bytes.as_ptr().cast(), bytes.len(), std::ptr::null()) };
        if h.is_null() {
            let why = match unsafe { (f().last_error)() } {
                2 => "file not found or unreadable",
                3 => "not a PDF, or damaged",
                4 => "password protected",
                5 => "unsupported security scheme",
                6 => "page not found",
                _ => "unknown error",
            };
            return Err(why.into());
        }
        Ok(Doc { h, _bytes: bytes })
    }

    /// Page sizes in points, after the page's own /Rotate.
    pub fn page_sizes(&self) -> Vec<(f32, f32)> {
        let n = unsafe { (f().page_count)(self.h) }.max(0);
        (0..n)
            .map(|i| {
                let (mut w, mut h) = (0.0, 0.0);
                unsafe { (f().page_size)(self.h, i, &mut w, &mut h) };
                (w.max(1.0) as f32, h.max(1.0) as f32)
            })
            .collect()
    }

    /// The document outline (bookmarks), depth first. Malformed outlines
    /// (loops, absurd sizes) are cut short rather than trusted.
    pub fn outline(&self, sizes: &[(f32, f32)]) -> Vec<Entry> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![(unsafe { (f().bm_first_child)(self.h, std::ptr::null_mut()) }, 0u32)];
        while let Some((bm, depth)) = stack.pop() {
            if bm.is_null() || out.len() >= 20_000 || !seen.insert(bm as usize) {
                continue;
            }
            // Siblings after this entry's children: push the sibling first.
            stack.push((unsafe { (f().bm_next_sibling)(self.h, bm) }, depth));
            if depth < 32 {
                stack.push((unsafe { (f().bm_first_child)(self.h, bm) }, depth + 1));
            }
            let (page, frac) = self.target(bm, sizes).map_or((None, 0.0), |(p, y)| (Some(p), y));
            out.push(Entry {
                title: unsafe { title(bm) },
                depth,
                page,
                frac,
            });
        }
        out
    }

    /// Where bookmark `bm` goes: a page, and how far down it (0 = top).
    fn target(&self, bm: H, sizes: &[(f32, f32)]) -> Option<(usize, f32)> {
        unsafe {
            let mut dest = (f().bm_dest)(self.h, bm);
            if dest.is_null() {
                let action = (f().bm_action)(bm);
                if action.is_null() {
                    return None;
                }
                dest = (f().action_dest)(self.h, action);
                if dest.is_null() {
                    return None;
                }
            }
            let page = usize::try_from((f().dest_page)(self.h, dest)).ok()?;
            let h = sizes.get(page)?.1;
            let (mut has_x, mut has_y, mut has_zoom) = (0, 0, 0);
            let (mut x, mut y, mut zoom) = (0.0f32, 0.0f32, 0.0f32);
            let ok = (f().dest_location)(dest, &mut has_x, &mut has_y, &mut has_zoom, &mut x, &mut y, &mut zoom);
            // PDF y runs up from the bottom of the page.
            let frac = if ok != 0 && has_y != 0 {
                (1.0 - y / h).clamp(0.0, 1.0)
            } else {
                0.0
            };
            Some((page, frac))
        }
    }

    pub fn page(&self, index: usize) -> Option<Page> {
        let h = unsafe { (f().load_page)(self.h, index as c_int) };
        (!h.is_null()).then_some(Page { h })
    }
}

impl Drop for Doc {
    fn drop(&mut self) {
        unsafe { (f().close_doc)(self.h) }
    }
}

/// One outline entry: its title, nesting depth (0 = top level), and where
/// it goes, if that can be resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub title: String,
    pub depth: u32,
    pub page: Option<usize>,
    pub frac: f32,
}

/// A bookmark's title (UTF-16LE in pdfium), on one line.
unsafe fn title(bm: H) -> String {
    let n = (f().bm_title)(bm, std::ptr::null_mut(), 0) as usize;
    if n < 2 {
        return String::new();
    }
    let mut buf = vec![0u16; n / 2];
    (f().bm_title)(bm, buf.as_mut_ptr().cast(), (buf.len() * 2) as c_ulong);
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let t = String::from_utf16_lossy(&buf[..end]);
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub struct Page {
    h: H,
}

const BGR: c_int = 2;
const FPDF_ANNOT: c_int = 0x01;

impl Page {
    /// Renders a `bw`x`bh` window of the page scaled to `size_w`x`size_h` and
    /// shifted by (`dx`, `dy`) pixels: the window's top-left shows page pixel
    /// (-dx, -dy). Returns packed RGB, white where the page doesn't reach.
    pub fn render(&self, bw: u32, bh: u32, dx: i32, dy: i32, size_w: u32, size_h: u32) -> Vec<u8> {
        let (w, h) = (bw as usize, bh as usize);
        let mut out = vec![0u8; w * h * 3];
        unsafe {
            let bmp = (f().bmp_create)(bw as c_int, bh as c_int, BGR, std::ptr::null_mut(), 0);
            if bmp.is_null() {
                out.fill(255);
                return out;
            }
            (f().bmp_fill)(bmp, 0, 0, bw as c_int, bh as c_int, 0xFFFF_FFFF);
            (f().render)(bmp, self.h, dx, dy, size_w as c_int, size_h as c_int, 0, FPDF_ANNOT);
            let stride = (f().bmp_stride)(bmp) as usize;
            let buf = std::slice::from_raw_parts((f().bmp_buffer)(bmp) as *const u8, stride * h);
            for (src, dst) in buf.chunks_exact(stride).zip(out.chunks_exact_mut(w * 3)) {
                for (&[b, g, r], d) in src[..w * 3].as_chunks::<3>().0.iter().zip(dst.as_chunks_mut::<3>().0) {
                    *d = [r, g, b];
                }
            }
            (f().bmp_destroy)(bmp);
        }
        out
    }

    /// Every match of `needle` on this page, each as one or more rectangles
    /// in page fractions (x0, y0, x1, y1), origin top-left.
    pub fn find(&self, needle: &str, match_case: bool) -> Vec<Vec<[f32; 4]>> {
        const SCALE: c_int = 1_000_000;
        let wide: Vec<u16> = needle.encode_utf16().chain(std::iter::once(0)).collect();
        let mut out = Vec::new();
        unsafe {
            let tp = (f().text_load)(self.h);
            if tp.is_null() {
                return out;
            }
            let sh = (f().find_start)(tp, wide.as_ptr(), if match_case { 1 } else { 0 }, 0);
            if !sh.is_null() {
                while (f().find_next)(sh) != 0 {
                    let (start, count) = ((f().find_index)(sh), (f().find_count)(sh));
                    let n = (f().count_rects)(tp, start, count);
                    let mut rects = Vec::new();
                    for i in 0..n.max(0) {
                        let (mut l, mut t, mut r, mut b) = (0.0, 0.0, 0.0, 0.0);
                        if (f().get_rect)(tp, i, &mut l, &mut t, &mut r, &mut b) == 0 {
                            continue;
                        }
                        let (mut x0, mut y0, mut x1, mut y1) = (0, 0, 0, 0);
                        (f().page_to_device)(self.h, 0, 0, SCALE, SCALE, 0, l, t, &mut x0, &mut y0);
                        (f().page_to_device)(self.h, 0, 0, SCALE, SCALE, 0, r, b, &mut x1, &mut y1);
                        let s = SCALE as f32;
                        rects.push([
                            x0.min(x1) as f32 / s,
                            y0.min(y1) as f32 / s,
                            x0.max(x1) as f32 / s,
                            y0.max(y1) as f32 / s,
                        ]);
                    }
                    if !rects.is_empty() {
                        out.push(rects);
                    }
                }
                (f().find_close)(sh);
            }
            (f().text_close)(tp);
        }
        out
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        unsafe { (f().close_page)(self.h) }
    }
}
