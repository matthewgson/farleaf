//! farleaf's FARLEAF_LOG, read as it grows: each image's checksum and each
//! frame's placements, which say what the screen should show.

use crate::screen::Shown;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// A frame: its number (-1 after farleaf quit and cleared its images),
/// whether images were still missing, and its placements as (image, row,
/// col, source x, y, w, h, cols, rows).
pub type Frame = (i64, bool, Vec<[i64; 9]>);

pub struct FarleafLog {
    path: PathBuf,
    pos: u64,
    crc: HashMap<i64, i64>,
    pub frame: Option<Frame>,
    pub lines: Vec<String>,
}

impl FarleafLog {
    pub fn new(path: PathBuf) -> FarleafLog {
        FarleafLog {
            path,
            pos: 0,
            crc: HashMap::new(),
            frame: None,
            lines: Vec::new(),
        }
    }

    pub fn poll(&mut self) {
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return;
        };
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return;
        }
        let mut data = Vec::new();
        if f.read_to_end(&mut data).is_err() {
            return;
        }
        let Some(end) = data.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else {
            return;
        };
        self.pos += end as u64;
        for line in String::from_utf8_lossy(&data[..end]).lines() {
            self.line(line);
            self.lines.push(line.to_string());
        }
    }

    fn line(&mut self, line: &str) {
        // "  1234.5 what happened"
        let Some((_, what)) = line.trim_start().split_once(' ') else {
            return;
        };
        if let Some(rest) = what.strip_prefix("strip id=") {
            let id = rest.split(' ').next().and_then(|v| v.parse().ok());
            let crc = rest
                .rsplit_once("crc ")
                .and_then(|(_, c)| i64::from_str_radix(c.trim(), 16).ok());
            if let (Some(id), Some(crc)) = (id, crc) {
                self.crc.insert(id, crc);
            }
        } else if let Some(rest) = what.strip_prefix("frame ") {
            let mut parts = rest.split(' ');
            let n = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let missing = parts.next() == Some("missing=1");
            let places = parts
                .filter(|p| !p.is_empty())
                .filter_map(|p| {
                    let v: Vec<i64> = p.split(':').filter_map(|x| x.parse().ok()).collect();
                    v.try_into().ok()
                })
                .collect();
            self.frame = Some((n, missing, places));
        } else if what.starts_with("exit: ") {
            self.frame = Some((-1, false, Vec::new())); // it quit and cleared its images
        }
    }

    /// farleaf's last frame as shown placements, and whether it was complete.
    pub fn expected(&self) -> (Option<Vec<Shown>>, bool) {
        let Some((_, missing, places)) = &self.frame else {
            return (None, false);
        };
        let out = places
            .iter()
            .map(|&[i, row, col, sx, sy, sw, sh, c, r]| {
                [self.crc.get(&i).copied().unwrap_or(-1), row, col, c, r, sx, sy, sw, sh]
            })
            .collect();
        (Some(out), !missing)
    }
}

/// Is `shown` exactly `expected`, give or take one offset for all (herdr's
/// pane origin)?
pub fn compare(expected: Option<&[Shown]>, shown: &[Shown]) -> (bool, String) {
    let Some(expected) = expected else {
        return (false, "farleaf drew nothing".into());
    };
    if expected.len() != shown.len() {
        return (
            false,
            format!("{} images on screen, farleaf drew {}", shown.len(), expected.len()),
        );
    }
    let Some(first) = expected.first() else {
        return (true, "empty".into());
    };
    let body = |p: &Shown| -> [i64; 7] { [p[0], p[3], p[4], p[5], p[6], p[7], p[8]] };
    let mut have: HashMap<([i64; 7], i64, i64), usize> = HashMap::new();
    for p in shown {
        *have.entry((body(p), p[1], p[2])).or_default() += 1;
    }
    // The offset is that of some image with the first expected one's content.
    let mut worst = None;
    for s in shown.iter().filter(|s| body(s) == body(first)) {
        let (dr, dc) = (s[1] - first[1], s[2] - first[2]);
        let mut left = have.clone();
        let bad = expected
            .iter()
            .find(|e| match left.get_mut(&(body(e), e[1] + dr, e[2] + dc)) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    false
                }
                _ => true,
            });
        match bad {
            None => return (true, format!("{} images match", expected.len())),
            Some(b) => worst = worst.or(Some(*b)),
        }
    }
    (false, format!("missing or different: {:?}", worst.unwrap_or(*first)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offset_for_all_is_allowed() {
        let e = vec![[1, 0, 0, 2, 2, 0, 0, 4, 4], [2, 2, 0, 2, 2, 0, 0, 4, 4]];
        let s: Vec<Shown> = e
            .iter()
            .map(|p| [p[0], p[1] + 1, p[2] + 3, p[3], p[4], p[5], p[6], p[7], p[8]])
            .collect();
        assert!(compare(Some(&e), &s).0);
        let mut moved = s.clone();
        moved[1][1] += 1;
        assert!(!compare(Some(&e), &moved).0);
    }

    #[test]
    fn reads_strips_and_frames() {
        let mut l = FarleafLog::new(PathBuf::new());
        l.line("   10.0 strip id=16 p1 k0 t0 10x10: render 1.0 ms, encode 1.0 ms, 3 KB, crc 0000002a");
        l.line("   11.0 frame 3 missing=0 16:0:5:0:0:10:10:1:1");
        let (exp, complete) = l.expected();
        assert!(complete);
        assert_eq!(exp.unwrap(), vec![[42, 0, 5, 1, 1, 0, 0, 10, 10]]);
    }
}
