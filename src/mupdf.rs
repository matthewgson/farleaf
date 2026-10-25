//! The PDF engine: MuPDF, compiled into farleaf (build.rs) and called
//! through the small C shim in src/mupdf.c, which turns MuPDF's errors into
//! return values.
//!
//! A document is used by one thread at a time (the render worker). Each of
//! its pages is interpreted once into a display list, which any number of
//! threads can then render at once: that is how strips render in parallel.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::{Arc, OnceLock};

#[repr(C)]
struct Ctx {
    _private: [u8; 0],
}
#[repr(C)]
struct FzDoc {
    _private: [u8; 0],
}
#[repr(C)]
struct FzList {
    _private: [u8; 0],
}

type OutlineFn = extern "C" fn(*mut c_void, *const c_char, c_int, c_int, f32, f32);

extern "C" {
    fn fl_new_context() -> *mut Ctx;
    fn fl_clone_context(base: *mut Ctx) -> *mut Ctx;
    fn fl_open(ctx: *mut Ctx, data: *const u8, len: usize, err: *mut c_char, errlen: c_int) -> *mut FzDoc;
    fn fl_drop_document(ctx: *mut Ctx, doc: *mut FzDoc);
    fn fl_count_pages(ctx: *mut Ctx, doc: *mut FzDoc) -> c_int;
    fn fl_page_size(ctx: *mut Ctx, doc: *mut FzDoc, i: c_int, w: *mut f32, h: *mut f32) -> c_int;
    fn fl_load_list(ctx: *mut Ctx, doc: *mut FzDoc, i: c_int) -> *mut FzList;
    fn fl_drop_list(ctx: *mut Ctx, list: *mut FzList);
    fn fl_list_size(ctx: *mut Ctx, list: *mut FzList, w: *mut f32, h: *mut f32);
    #[allow(clippy::too_many_arguments)]
    fn fl_render(
        ctx: *mut Ctx,
        list: *mut FzList,
        rgb: *mut u8,
        w: c_int,
        h: c_int,
        sx: f32,
        sy: f32,
        tx: f32,
        ty: f32,
    ) -> c_int;
    fn fl_search(
        ctx: *mut Ctx,
        list: *mut FzList,
        needle: *const c_char,
        exact: c_int,
        quads: *mut f32,
        marks: *mut c_int,
        max: c_int,
    ) -> c_int;
    fn fl_outline(ctx: *mut Ctx, doc: *mut FzDoc, f: OutlineFn, user: *mut c_void);
    fn fl_empty_store(ctx: *mut Ctx);
}

/// Forgets what MuPDF has cached (decoded images, glyphs), so the next
/// render is timed cold. For `farleaf --bench`.
pub fn empty_cache() {
    unsafe { fl_empty_store(ctx()) }
}

/// The base context, with the locks that let threads share MuPDF's caches.
struct Base(*mut Ctx);
// Only ever cloned; MuPDF's locks make that safe from any thread.
unsafe impl Send for Base {}
unsafe impl Sync for Base {}

static BASE: OnceLock<Option<Base>> = OnceLock::new();

thread_local! {
    static CTX: Cell<*mut Ctx> = const { Cell::new(std::ptr::null_mut()) };
}

pub fn init() -> Result<(), String> {
    match BASE.get_or_init(|| {
        let ctx = unsafe { fl_new_context() };
        (!ctx.is_null()).then_some(Base(ctx))
    }) {
        Some(_) => Ok(()),
        None => Err("could not start MuPDF".into()),
    }
}

/// This thread's context, a clone of the base one made the first time.
fn ctx() -> *mut Ctx {
    CTX.with(|c| {
        if c.get().is_null() {
            let base = BASE.get().and_then(|b| b.as_ref()).expect("mupdf::init not called");
            c.set(unsafe { fl_clone_context(base.0) });
        }
        c.get()
    })
}

struct DocInner {
    h: *mut FzDoc,
    /// MuPDF reads the document from here; it is never copied.
    _bytes: Vec<u8>,
}

// Used by one thread at a time (Doc is not Sync); pages only keep it alive.
unsafe impl Send for DocInner {}
unsafe impl Sync for DocInner {}

impl Drop for DocInner {
    fn drop(&mut self) {
        unsafe { fl_drop_document(ctx(), self.h) }
    }
}

/// An open document. MuPDF reads it from memory, never from the file, so a
/// LaTeX run rewriting the file underneath cannot change what is on screen.
pub struct Doc {
    inner: Arc<DocInner>,
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

impl Doc {
    pub fn open(bytes: Vec<u8>) -> Result<Doc, String> {
        let mut err = [0 as c_char; 256];
        let h = unsafe { fl_open(ctx(), bytes.as_ptr(), bytes.len(), err.as_mut_ptr(), err.len() as c_int) };
        if h.is_null() {
            let why = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
            return Err(if why.is_empty() {
                "not a PDF, or damaged".into()
            } else {
                why
            });
        }
        Ok(Doc {
            inner: Arc::new(DocInner { h, _bytes: bytes }),
        })
    }

    /// Page sizes in points, after each page's own rotation.
    pub fn page_sizes(&self) -> Vec<(f32, f32)> {
        let n = unsafe { fl_count_pages(ctx(), self.inner.h) }.max(0);
        (0..n)
            .map(|i| {
                let (mut w, mut h) = (612.0, 792.0);
                unsafe { fl_page_size(ctx(), self.inner.h, i, &mut w, &mut h) };
                (w.max(1.0), h.max(1.0))
            })
            .collect()
    }

    /// Page `index`, interpreted into a display list for rendering.
    pub fn page(&self, index: usize) -> Option<Page> {
        let list = unsafe { fl_load_list(ctx(), self.inner.h, index as c_int) };
        if list.is_null() {
            return None;
        }
        let (mut w, mut h) = (0.0, 0.0);
        unsafe { fl_list_size(ctx(), list, &mut w, &mut h) };
        Some(Page {
            list,
            w: w.max(1.0),
            h: h.max(1.0),
            _doc: self.inner.clone(),
        })
    }

    /// The document outline (bookmarks), depth first.
    pub fn outline(&self, sizes: &[(f32, f32)]) -> Vec<Entry> {
        extern "C" fn add(user: *mut c_void, title: *const c_char, depth: c_int, page: c_int, _x: f32, y: f32) {
            let out = unsafe { &mut *(user as *mut Vec<(String, u32, Option<usize>, f32)>) };
            let title = unsafe { CStr::from_ptr(title) }.to_string_lossy();
            let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
            out.push((title, depth.max(0) as u32, usize::try_from(page).ok(), y));
        }
        let mut raw: Vec<(String, u32, Option<usize>, f32)> = Vec::new();
        unsafe { fl_outline(ctx(), self.inner.h, add, (&mut raw as *mut Vec<_>).cast()) };
        raw.into_iter()
            .map(|(title, depth, page, y)| {
                let page = page.filter(|&p| p < sizes.len());
                let frac = page.map_or(0.0, |p| (y / sizes[p].1).clamp(0.0, 1.0));
                Entry {
                    title,
                    depth,
                    page,
                    frac: if frac.is_finite() { frac } else { 0.0 },
                }
            })
            .collect()
    }
}

/// A page, ready to render: its display list, and its size in points.
pub struct Page {
    list: *mut FzList,
    w: f32,
    h: f32,
    /// What the list refers to stays alive as long as it does.
    _doc: Arc<DocInner>,
}

// MuPDF renders a display list from many threads at once, each with its own
// context; dropping it is reference counted under MuPDF's locks.
unsafe impl Send for Page {}
unsafe impl Sync for Page {}

impl Drop for Page {
    fn drop(&mut self) {
        unsafe { fl_drop_list(ctx(), self.list) }
    }
}

impl Page {
    /// Renders a `bw`x`bh` window of the page scaled to `size_w`x`size_h` and
    /// shifted by (`dx`, `dy`) pixels: the window's top-left shows page pixel
    /// (-dx, -dy). Returns packed RGB, white where the page doesn't reach.
    pub fn render(&self, bw: u32, bh: u32, dx: i32, dy: i32, size_w: u32, size_h: u32) -> Vec<u8> {
        let mut out = vec![0u8; bw as usize * bh as usize * 3];
        self.render_into(&mut out, bw, bh, dx, dy, size_w, size_h);
        out
    }

    /// Like `render`, into `out` (packed RGB, `bw`x`bh`), which MuPDF draws
    /// into directly.
    #[allow(clippy::too_many_arguments)]
    pub fn render_into(&self, out: &mut [u8], bw: u32, bh: u32, dx: i32, dy: i32, size_w: u32, size_h: u32) {
        assert_eq!(out.len(), bw as usize * bh as usize * 3);
        let (sx, sy) = (size_w as f32 / self.w, size_h as f32 / self.h);
        unsafe {
            fl_render(
                ctx(),
                self.list,
                out.as_mut_ptr(),
                bw as c_int,
                bh as c_int,
                sx,
                sy,
                dx as f32,
                dy as f32,
            )
        };
    }

    /// Every match of `needle` on this page, each as one or more rectangles
    /// in page fractions (x0, y0, x1, y1), origin top-left.
    pub fn find(&self, needle: &str, match_case: bool) -> Vec<Vec<[f32; 4]>> {
        const MAX: usize = 4096;
        let Ok(needle) = CString::new(needle) else {
            return Vec::new();
        };
        let mut quads = vec![0f32; MAX * 8];
        let mut marks = vec![0 as c_int; MAX];
        let n = unsafe {
            fl_search(
                ctx(),
                self.list,
                needle.as_ptr(),
                c_int::from(match_case),
                quads.as_mut_ptr(),
                marks.as_mut_ptr(),
                MAX as c_int,
            )
        }
        .clamp(0, MAX as c_int) as usize;
        let mut out: Vec<Vec<[f32; 4]>> = Vec::new();
        for i in 0..n {
            let q = &quads[i * 8..i * 8 + 8];
            let xs = [q[0], q[2], q[4], q[6]];
            let ys = [q[1], q[3], q[5], q[7]];
            let min = |v: [f32; 4]| v.into_iter().fold(f32::MAX, f32::min);
            let max = |v: [f32; 4]| v.into_iter().fold(f32::MIN, f32::max);
            let r = [min(xs) / self.w, min(ys) / self.h, max(xs) / self.w, max(ys) / self.h];
            match out.last_mut() {
                Some(hit) if marks[i] == 0 => hit.push(r),
                _ => out.push(vec![r]),
            }
        }
        out
    }
}
