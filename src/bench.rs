//! `farleaf --bench FILE`: where the time goes. Opens the file, then renders
//! its first pages strip by strip, as farleaf would for a window of
//! FARLEAF_BENCH_GEOM (columns x rows x cell width x cell height; by default
//! 131x48x19x42, half of a 5K display), and times each stage.
//!
//! FARLEAF_BENCH_PAGES sets how many pages, FARLEAF_BENCH_LEVELS=1 compares
//! zlib levels, and FARLEAF_BENCH_PNG=out.png saves page 2 as rendered.

use crate::encode;
use crate::layout::{Fit, Geom, Layout};
use crate::mupdf;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let v = f();
    (v, t.elapsed())
}

pub fn run(path: &Path) -> Result<(), String> {
    // Piped into `head`, stop quietly when it has read enough.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    mupdf::init()?;
    let geom = std::env::var("FARLEAF_BENCH_GEOM").unwrap_or_else(|_| "131x48x19x42".into());
    let g: Vec<u32> = geom.split('x').filter_map(|n| n.parse().ok()).collect();
    let [cols, rows, cw, ch] = g[..] else {
        return Err(format!("FARLEAF_BENCH_GEOM={geom}: want COLSxROWSxCWxCH"));
    };
    let pages: usize = std::env::var("FARLEAF_BENCH_PAGES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(10);
    let threads = std::env::var("FARLEAF_BENCH_THREADS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .saturating_sub(2)
                .clamp(1, 8)
        });

    let (bytes, read) = timed(|| std::fs::read(path));
    let bytes = bytes.map_err(|e| e.to_string())?;
    let (doc, open) = timed(|| mupdf::Doc::open(bytes));
    let doc = doc?;
    let (sizes, sizes_t) = timed(|| doc.page_sizes());
    let (outline, outline_t) = timed(|| doc.outline(&sizes));
    println!(
        "{}: {} pages; read {:.1} ms, open {:.1} ms, page sizes {:.1} ms, outline ({} entries) {:.1} ms",
        path.display(),
        sizes.len(),
        ms(read),
        ms(open),
        ms(sizes_t),
        outline.len(),
        ms(outline_t)
    );
    if std::env::var_os("FARLEAF_BENCH_OUTLINE").is_some() {
        for e in &outline {
            let at = e
                .page
                .map_or("-".into(), |p| format!("page {} at {:.3}", p + 1, e.frac));
            println!("  {}{}  ({at})", "  ".repeat(e.depth as usize), e.title);
        }
    }
    let geom = Geom {
        cols,
        rows,
        cw,
        ch,
        scale: 1.0,
    };
    let layout = Layout::new(&sizes, geom, Fit::Width, 1.0);
    println!(
        "window {cols}x{rows} cells of {cw}x{ch} px, fit width: pages {} px wide, strips {} rows; {threads} render threads",
        layout.pages[0].px_w, layout.pages[0].strip_rows
    );
    println!(
        "{:>4} {:>6} {:>7} {:>7} {:>8} {:>7} {:>7} {:>6} {:>7} {:>7} {:>6}",
        "page", "load", "strips", "render", "parallel", "whole", "png", "b64", "zlib", "png KB", "gray"
    );
    let mut sum = [Duration::ZERO; 7];
    let (mut png_bytes, mut zlib_bytes, mut raw_bytes, mut n) = (0, 0, 0, 0);
    for p in 0..sizes.len().min(pages) {
        let (page, load) = timed(|| doc.page(p));
        let page = Arc::new(page.ok_or("page failed to load")?);
        let b = layout.pages[p];
        let pieces: Vec<(u32, u32, i32, i32)> = (0..layout.strips(p))
            .flat_map(|k| (0..layout.tiles(p)).map(move |t| (k, t)))
            .map(|(k, t)| layout.piece(p, k, t))
            .collect();
        // Each way timed cold: nothing decoded or cached from the one before.
        mupdf::empty_cache();
        let mut strips = Vec::new();
        let mut render = Duration::ZERO;
        for &(bw, bh, dx, dy) in &pieces {
            let (rgb, d) = timed(|| page.render(bw, bh, dx, dy, b.px_w, b.px_h));
            render += d;
            strips.push((rgb, bw, bh));
        }
        // The same strips on the render threads, as farleaf does them; they
        // must come out the same.
        mupdf::empty_cache();
        let out: Vec<std::sync::Mutex<Vec<u8>>> = pieces.iter().map(|_| Default::default()).collect();
        let (_, parallel) = timed(|| {
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(|| loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(bw, bh, dx, dy)) = pieces.get(i) else { break };
                        *out[i].lock().unwrap() = page.render(bw, bh, dx, dy, b.px_w, b.px_h);
                    });
                }
            })
        });
        if out.iter().zip(&strips).any(|(o, s)| *o.lock().unwrap() != s.0) {
            println!(
                "     page {}: strips rendered in parallel differ from serial ones",
                p + 1
            );
        }
        mupdf::empty_cache();
        let (whole_rgb, whole) = timed(|| page.render(b.px_w, b.px_h, 0, 0, b.px_w, b.px_h));
        if let Some(out) = std::env::var_os("FARLEAF_BENCH_PNG").filter(|_| p == 1) {
            std::fs::write(out, encode::png(&whole_rgb, b.px_w, b.px_h)).map_err(|e| e.to_string())?;
        }
        let (mut png, mut b64, mut zlib, mut gray, mut kb) = (Duration::ZERO, Duration::ZERO, Duration::ZERO, 0, 0);
        for (rgb, w, h) in &strips {
            let (data, d) = timed(|| encode::png(rgb, *w, *h));
            png += d;
            gray += usize::from(data[25] == 0);
            kb += data.len();
            let (_, d) = timed(|| encode::base64(&mut Vec::with_capacity(data.len() * 4 / 3 + 4), &data));
            b64 += d;
            let (z, d) = timed(|| encode::zlib(rgb, 2));
            zlib += d;
            zlib_bytes += z.len();
            raw_bytes += rgb.len();
        }
        png_bytes += kb;
        n += 1;
        for (s, d) in sum.iter_mut().zip([load, render, parallel, whole, png, b64, zlib]) {
            *s += d;
        }
        println!(
            "{:>4} {:>6.1} {:>7} {:>7.1} {:>8.1} {:>7.1} {:>7.1} {:>6.1} {:>7.1} {:>7} {:>3}/{:<2}",
            p + 1,
            ms(load),
            strips.len(),
            ms(render),
            ms(parallel),
            ms(whole),
            ms(png),
            ms(b64),
            ms(zlib),
            kb / 1024,
            gray,
            strips.len()
        );
    }
    if std::env::var_os("FARLEAF_BENCH_LEVELS").is_some() {
        let n = sizes.len().min(pages);
        let mut all = Vec::new();
        for p in 0..n {
            let page = doc.page(p).ok_or("page failed to load")?;
            for k in 0..layout.strips(p) {
                let (bw, bh, dx, dy) = layout.piece(p, k, 0);
                let b = layout.pages[p];
                let rgb = page.render(bw, bh, dx, dy, b.px_w, b.px_h);
                let gray = rgb.as_chunks::<3>().0.iter().all(|&[r, g, b]| r == g && g == b);
                all.push(if gray {
                    rgb.iter().step_by(3).copied().collect()
                } else {
                    rgb
                });
            }
        }
        for level in 1..=9 {
            let (total, d) = timed(|| all.iter().map(|s| encode::zlib(s, level).len()).sum::<usize>());
            println!(
                "level {level}: {:.1} ms per page, {:.0} KB per page",
                ms(d) / n as f64,
                total as f64 / n as f64 / 1e3
            );
        }
    }
    if n == 0 {
        return Ok(());
    }
    let avg = |d: Duration| ms(d) / n as f64;
    println!(
        "mean {:>6.1} {:>7} {:>7.1} {:>8.1} {:>7.1} {:>7.1} {:>6.1} {:>7.1} {:>7}",
        avg(sum[0]),
        "",
        avg(sum[1]),
        avg(sum[2]),
        avg(sum[3]),
        avg(sum[4]),
        avg(sum[5]),
        avg(sum[6]),
        png_bytes / 1024 / n
    );
    println!(
        "per page: {:.1} MB of pixels, PNG {:.0} KB, zlib {:.0} KB",
        raw_bytes as f64 / n as f64 / 1e6,
        png_bytes as f64 / n as f64 / 1e3,
        zlib_bytes as f64 / n as f64 / 1e3
    );
    Ok(())
}
