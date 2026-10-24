//! Records the demo GIF: farleaf in the fake terminal, one frame per step.
//!
//!   cargo dev demo [--pdf docs/demo/demo.pdf] [--out docs/demo.gif] [--farleaf BIN]
//!
//! Each frame is what farleaf sent, as the fake terminal draws it: the
//! images, and the text in its colors. A badge shows the key just pressed.
//! Every screen is checked against what farleaf drew first.

use crate::draw::{rounded_box, Text};
use crate::session::{self, keybytes, Opts, Step};
use std::path::PathBuf;

/// (keys, badge, seconds on screen)
const SCRIPT: &[(Option<&str>, &str, f64)] = &[
    (None, "farleaf demo.pdf", 2.2),
    (Some("j"), "j  scroll", 0.6),
    (Some("j"), "j  scroll", 0.6),
    (Some("l"), "l  next page", 1.4),
    (Some("t"), "t  page thumbnails", 1.6),
    (Some("j"), "j", 0.9),
    (Some("j"), "j", 0.9),
    (Some("j"), "j", 1.0),
    (Some("\\r"), "Enter  go there", 1.5),
    (Some("o"), "o  outline", 1.6),
    (Some("k"), "k", 0.8),
    (Some("k"), "k", 0.8),
    (Some("k"), "k", 0.8),
    (Some("k"), "k", 1.0),
    (Some("\\r"), "Enter  go there", 1.5),
    (Some("o"), "o", 0.6),
    (Some("o"), "o  hide", 1.3),
    (Some("/pieces"), "/  search", 1.0),
    (Some("\\r"), "Enter", 1.6),
    (Some("n"), "n  next match", 1.4),
    (Some("z"), "z  fit page", 1.5),
    (Some("Z"), "Z  fit width", 1.2),
    (Some("?"), "?  every key", 2.6),
    (Some("?"), "", 1.2),
];

pub fn main(args: &[String]) -> Result<(), String> {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut pdf = repo.join("docs/demo/demo.pdf");
    let mut out = repo.join("docs/demo.gif");
    let mut farleaf = repo.join("target/release/farleaf");
    for pair in args.chunks(2) {
        let v = pair
            .get(1)
            .map(PathBuf::from)
            .ok_or(format!("{} needs a value", pair[0]))?;
        match pair[0].as_str() {
            "--pdf" => pdf = v,
            "--out" => out = v,
            "--farleaf" => farleaf = v,
            a => return Err(format!("unknown option {a}")),
        }
    }
    let (cols, rows, cell) = (128, 36, (10, 22));
    // The frames and the log stay in dev/out/demo, for a look afterwards.
    let work = repo.join("dev/out/demo");
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let doc = work.join("demo.pdf");
    std::fs::copy(&pdf, &doc).map_err(|e| format!("{}: {e}", pdf.display()))?;
    let flog = work.join("farleaf.log");
    let _ = std::fs::write(&flog, "");
    let mut steps = vec![Step::Wait("open".into(), 1.0)];
    for (i, (keys, _, _)) in SCRIPT.iter().enumerate() {
        if let Some(k) = keys {
            steps.push(Step::Keys(keybytes(k)));
        }
        steps.push(Step::Snap(format!("{i:02}")));
    }
    let o = Opts {
        cols,
        rows,
        cell,
        shm: true,
        timeout: 120.0,
        out: work.clone(),
        flog: Some(flog.clone()),
        verbose: false,
        unset: [
            "HERDR_ENV",
            "HERDR_PANE_ID",
            "HERDR_TAB_ID",
            "HERDR_WORKSPACE_ID",
            "HERDR_SOCKET_PATH",
        ]
        .map(String::from)
        .to_vec(),
        env: vec![
            ("TERM".into(), "xterm-ghostty".into()),
            ("TERM_PROGRAM".into(), "ghostty".into()),
            ("FARLEAF_LOG".into(), flog.to_string_lossy().into_owned()),
        ],
        ..Opts::default()
    };
    let cmd = [
        farleaf.to_string_lossy().into_owned(),
        doc.to_string_lossy().into_owned(),
    ];
    let res = session::run(&cmd, &o, Vec::new(), Some(steps))?;
    if let Some(bad) = res.checks.iter().find(|c| !c.ok) {
        return Err(format!(
            "the screen did not match what farleaf drew at {:?}: {}",
            bad.name, bad.why
        ));
    }
    let mut text = Text::new(cell.0, cell.1).ok_or("no font found (set FARLEAF_DEV_FONT)")?;
    let file = std::fs::File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let (w, h) = ((cols * cell.0) as u16, (rows * cell.1) as u16);
    let mut gif = gif::Encoder::new(file, w, h, &[]).map_err(|e| e.to_string())?;
    gif.set_repeat(gif::Repeat::Infinite).map_err(|e| e.to_string())?;
    let mut prev: Option<Vec<u8>> = None;
    let mut total = 0.0;
    for (i, (_, label, secs)) in SCRIPT.iter().enumerate() {
        let data = std::fs::read(work.join(format!("{i:02}.png"))).map_err(|e| e.to_string())?;
        let (_, _, mut rgb) = crate::png::decode(&data)?;
        badge(&mut rgb, w as usize, &mut text, label);
        // Only the part that changed goes into the frame; the rest stays.
        let (x0, y0, x1, y1) = match &prev {
            Some(p) => changed(p, &rgb, w as usize, h as usize).unwrap_or((0, 0, 1, 1)),
            None => (0, 0, w as usize, h as usize),
        };
        let mut part = Vec::with_capacity((x1 - x0) * (y1 - y0) * 3);
        for y in y0..y1 {
            part.extend_from_slice(&rgb[(y * w as usize + x0) * 3..(y * w as usize + x1) * 3]);
        }
        let mut frame = gif::Frame::from_rgb_speed((x1 - x0) as u16, (y1 - y0) as u16, &part, 10);
        frame.left = x0 as u16;
        frame.top = y0 as u16;
        frame.delay = (secs * 100.0).round() as u16;
        gif.write_frame(&frame).map_err(|e| e.to_string())?;
        prev = Some(rgb);
        total += secs;
    }
    drop(gif);
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!(
        "wrote {}: {} frames, {:.1} MB, {total:.0} s; {} screens checked",
        out.display(),
        SCRIPT.len(),
        size as f64 / 1e6,
        res.checks.len()
    );
    Ok(())
}

/// The box around the pixels that differ, if any: x0, y0, x1, y1.
fn changed(a: &[u8], b: &[u8], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        let row = y * w * 3;
        for x in 0..w {
            let i = row + x * 3;
            if a[i..i + 3] != b[i..i + 3] {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
    }
    (x1 > x0).then_some((x0, y0, x1, y1))
}

fn badge(rgb: &mut [u8], w: usize, text: &mut Text, label: &str) {
    if label.is_empty() {
        return;
    }
    let h = rgb.len() / 3 / w;
    let px = 20.0;
    let pad = 14.0;
    let tw = text.width(label, px);
    let (x1, y1) = (w as f32 - 24.0, h as f32 - 56.0);
    let rect = [
        (x1 - tw - 2.0 * pad) as i64,
        (y1 - px - 2.0 * pad) as i64,
        x1 as i64,
        y1 as i64,
    ];
    rounded_box(rgb, w, rect, 10, 2, [20, 24, 30], [135, 175, 215]);
    let baseline = rect[1] as f32 + pad + px * 0.8;
    text.string(rgb, w, rect[0] as f32 + pad, baseline, label, px, [235, 235, 235]);
}
