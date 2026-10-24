//! `farleaf --bench FILE`: where the time goes. Opens the file, then renders
//! its first pages strip by strip, as farleaf would for a window of
//! FARLEAF_BENCH_GEOM (columns x rows x cell width x cell height; by default
//! 131x48x19x42, half of a 5K display), and times each stage.

use crate::encode;
use crate::layout::{Fit, Geom, Layout};
use crate::pdfium;
use std::path::Path;
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
    pdfium::init()?;
    let geom = std::env::var("FARLEAF_BENCH_GEOM").unwrap_or_else(|_| "131x48x19x42".into());
    let g: Vec<u32> = geom.split('x').filter_map(|n| n.parse().ok()).collect();
    let [cols, rows, cw, ch] = g[..] else {
        return Err(format!("FARLEAF_BENCH_GEOM={geom}: want COLSxROWSxCWxCH"));
    };
    let pages: usize = std::env::var("FARLEAF_BENCH_PAGES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(10);

    let (bytes, read) = timed(|| std::fs::read(path));
    let bytes = bytes.map_err(|e| e.to_string())?;
    let (doc, open) = timed(|| pdfium::Doc::open(bytes));
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
    let geom = Geom {
        cols,
        rows,
        cw,
        ch,
        scale: 1.0,
    };
    let layout = Layout::new(&sizes, geom, Fit::Width, 1.0);
    println!(
        "window {cols}x{rows} cells of {cw}x{ch} px, fit width: pages {} px wide, strips {} rows",
        layout.pages[0].px_w, layout.pages[0].strip_rows
    );
    println!(
        "{:>4} {:>6} {:>7} {:>7} {:>7} {:>7} {:>6} {:>7} {:>7} {:>6}",
        "page", "load", "strips", "render", "whole", "png", "b64", "zlib", "png KB", "gray"
    );
    let mut sum = [Duration::ZERO; 6];
    let (mut png_bytes, mut zlib_bytes, mut raw_bytes, mut n) = (0, 0, 0, 0);
    for p in 0..sizes.len().min(pages) {
        let (page, load) = timed(|| doc.page(p));
        let page = page.ok_or("page failed to load")?;
        let mut strips = Vec::new();
        let mut render = Duration::ZERO;
        for k in 0..layout.strips(p) {
            for t in 0..layout.tiles(p) {
                let (bw, bh, dx, dy) = layout.piece(p, k, t);
                let b = layout.pages[p];
                let (rgb, d) = timed(|| page.render(bw, bh, dx, dy, b.px_w, b.px_h));
                render += d;
                strips.push((rgb, bw, bh));
            }
        }
        let b = layout.pages[p];
        let (direct, whole) = timed(|| page.render(b.px_w, b.px_h, 0, 0, b.px_w, b.px_h));
        if std::env::var_os("FARLEAF_BENCH_CHECK").is_some() {
            let (old, old_t) = timed(|| page.render_swapped(b.px_w, b.px_h, 0, 0, b.px_w, b.px_h));
            let differ = direct.iter().zip(&old).filter(|(a, b)| a != b).count();
            println!(
                "     direct RGB {:.1} ms, BGR + reorder {:.1} ms, {differ} bytes differ",
                ms(whole),
                ms(old_t)
            );
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
        for (s, d) in sum.iter_mut().zip([load, render, whole, png, b64, zlib]) {
            *s += d;
        }
        println!(
            "{:>4} {:>6.1} {:>7} {:>7.1} {:>7.1} {:>7.1} {:>6.1} {:>7.1} {:>7} {:>3}/{:<2}",
            p + 1,
            ms(load),
            strips.len(),
            ms(render),
            ms(whole),
            ms(png),
            ms(b64),
            ms(zlib),
            kb / 1024,
            gray,
            strips.len()
        );
    }
    if std::env::var_os("FARLEAF_BENCH_BANDS").is_some() {
        // Several strips rendered in one call, then cut apart.
        let n = sizes.len().min(pages);
        for per in [1u32, 2, 4, 8, 16, 1000] {
            let mut total = Duration::ZERO;
            for p in 0..n {
                let page = doc.page(p).ok_or("page failed to load")?;
                let b = layout.pages[p];
                let strips = layout.strips(p);
                let mut k = 0;
                while k < strips {
                    let last = (k + per).min(strips) - 1;
                    let (bw, _, dx, dy) = layout.piece(p, k, 0);
                    let (_, bh_last, _, dy_last) = layout.piece(p, last, 0);
                    let bh = (dy - dy_last) as u32 + bh_last;
                    let (_, d) = timed(|| page.render(bw, bh, dx, dy, b.px_w, b.px_h));
                    total += d;
                    k = last + 1;
                }
            }
            println!("{per:>4} strips per call: {:.1} ms per page", ms(total) / n as f64);
        }
    }
    if std::env::var_os("FARLEAF_BENCH_PALETTE").is_some() {
        let n = sizes.len().min(pages);
        let (mut color, mut few, mut rgb_kb, mut idx_kb, mut idx_t) = (0, 0, 0, 0, Duration::ZERO);
        let mut filt_kb = [0usize; 5];
        for p in 0..n {
            let page = doc.page(p).ok_or("page failed to load")?;
            for k in 0..layout.strips(p) {
                let (bw, bh, dx, dy) = layout.piece(p, k, 0);
                let b = layout.pages[p];
                let rgb = page.render(bw, bh, dx, dy, b.px_w, b.px_h);
                if rgb.as_chunks::<3>().0.iter().all(|&[r, g, b]| r == g && g == b) {
                    continue;
                }
                color += 1;
                let t = Instant::now();
                let mut index = std::collections::HashMap::new();
                let mut data = Vec::with_capacity(rgb.len() / 3);
                let fits = rgb.as_chunks::<3>().0.iter().all(|px| {
                    let n = index.len();
                    let i = *index.entry(*px).or_insert(n);
                    data.push(i as u8);
                    index.len() <= 256
                });
                idx_t += t.elapsed();
                // PNG row filters: none, sub, up, paeth, each row choosing.
                let w3 = bw as usize * 3;
                for (fi, f) in ["none", "sub", "up", "paeth", "adaptive"].iter().enumerate() {
                    let mut raw = Vec::with_capacity(rgb.len() + bh as usize);
                    let mut prev = vec![0u8; w3];
                    for row in rgb.chunks_exact(w3) {
                        let filt = |t: usize| -> Vec<u8> {
                            (0..w3)
                                .map(|x| {
                                    let a = if x >= 3 { row[x - 3] } else { 0 };
                                    let b = prev[x];
                                    let c = if x >= 3 { prev[x - 3] } else { 0 };
                                    let pred = match t {
                                        0 => 0,
                                        1 => a,
                                        2 => b,
                                        _ => {
                                            let p = a as i16 + b as i16 - c as i16;
                                            let (pa, pb, pc) =
                                                ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
                                            if pa <= pb && pa <= pc {
                                                a
                                            } else if pb <= pc {
                                                b
                                            } else {
                                                c
                                            }
                                        }
                                    };
                                    row[x].wrapping_sub(pred)
                                })
                                .collect()
                        };
                        let (t, data) = match fi {
                            4 => (0..4usize)
                                .map(|t| (t, filt(t)))
                                .min_by_key(|(_, d)| d.iter().map(|&v| (v as i8).unsigned_abs() as u32).sum::<u32>())
                                .unwrap(),
                            t => (t.min(3), filt(t.min(3))),
                        };
                        raw.push([0u8, 1, 2, 4][t]);
                        raw.extend_from_slice(&data);
                        prev.copy_from_slice(row);
                    }
                    filt_kb[fi] += encode::zlib(&raw, 3).len();
                    let _ = f;
                }
                let z = encode::zlib(&rgb, 3).len();
                rgb_kb += z;
                if fits {
                    few += 1;
                    idx_kb += encode::zlib(&data, 3).len();
                } else {
                    idx_kb += z;
                }
            }
        }
        println!(
            "{color} color strips, {few} with <= 256 colors; RGB {} KB, indexed where possible {} KB; indexing {:.1} ms per page",
            rgb_kb / 1024,
            idx_kb / 1024,
            ms(idx_t) / n as f64
        );
        println!(
            "color strips by PNG filter, KB: none {}, sub {}, up {}, paeth {}, adaptive {}",
            filt_kb[0] / 1024,
            filt_kb[1] / 1024,
            filt_kb[2] / 1024,
            filt_kb[3] / 1024,
            filt_kb[4] / 1024
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
                let data: Vec<u8> = if gray {
                    rgb.iter().step_by(3).copied().collect()
                } else {
                    rgb
                };
                all.push(data);
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
        "mean {:>6.1} {:>7} {:>7.1} {:>7.1} {:>7.1} {:>6.1} {:>7.1} {:>7}",
        avg(sum[0]),
        "",
        avg(sum[1]),
        avg(sum[2]),
        avg(sum[3]),
        avg(sum[4]),
        avg(sum[5]),
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
