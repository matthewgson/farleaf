//! A small sample PDF for tests: text and a color bar chart on every page,
//! the word "farleaf" on every page (for search), and an outline with a
//! section every four pages and two subsections each, some pointing at
//! explicit destinations and some through GoTo actions, as LaTeX writes them.

const WORDS: &str = "the reader renders each page in strips and sends them to the terminal as images \
                     while the next page is already being prepared so that turning a page with farleaf \
                     only moves pictures that are there";

fn page_stream(n: usize, pages: usize) -> String {
    let words: Vec<&str> = WORDS.split(' ').collect();
    let mut out = vec![format!("BT /F1 20 Tf 72 720 Td (Sample page {n} of {pages}) Tj ET")];
    let mut y = 690;
    for line in 0..30 {
        if (8..18).contains(&line) {
            continue; // room for the figure
        }
        let text: Vec<&str> = (0..13).map(|i| words[(n * 7 + line * 5 + i) % words.len()]).collect();
        out.push(format!("BT /F1 11 Tf 72 {y} Td ({}) Tj ET", text.join(" ")));
        y -= if line != 7 { 16 } else { 16 * 11 };
    }
    // A bar chart, so pages are not all gray.
    for i in 0..6 {
        let (r, g, b) = (0.2 + 0.1 * i as f32, 0.4, 0.8 - 0.1 * i as f32);
        let h = 20 + ((n + i * 3) % 7) * 18;
        out.push(format!("{r:.2} {g:.2} {b:.2} rg {} 420 50 {h} re f", 100 + i * 70));
    }
    out.push("0 0 0 RG 1 w 90 420 m 530 420 l S".into());
    out.join("\n")
}

pub fn write(path: &std::path::Path, pages: usize) -> std::io::Result<()> {
    let pages = pages.max(1);
    // Objects 1 and 2 (catalog, page tree) are filled in last.
    let mut objs: Vec<String> = vec![String::new(), String::new()];
    objs.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    let mut kids = Vec::new();
    for n in 1..=pages {
        let content = page_stream(n, pages);
        objs.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
        let contents = objs.len();
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {contents} 0 R >>"
        ));
        kids.push(objs.len());
    }
    let root = outline(&mut objs, &kids);
    let kid_refs: Vec<String> = kids.iter().map(|k| format!("{k} 0 R")).collect();
    objs[1] = format!("<< /Type /Pages /Kids [{}] /Count {pages} >>", kid_refs.join(" "));
    objs[0] = format!("<< /Type /Catalog /Pages 2 0 R /Outlines {root} 0 R /PageMode /UseOutlines >>");
    let mut data = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(data.len());
        data.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = data.len();
    data.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        data.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    data.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    std::fs::write(path, data)
}

/// Adds the outline's objects; returns the outline root's object number.
fn outline(objs: &mut Vec<String>, kids: &[usize]) -> usize {
    let dest = |page: usize, y: u32| format!("[{} 0 R /XYZ 0 {y} null]", kids[page]);
    let pages = kids.len();
    // Numbers first, so entries can point at each other.
    let root = objs.len() + 1;
    let mut next = root + 1;
    // (number, title, target), and a section's children
    type Entry = (usize, String, String);
    let mut sections: Vec<(usize, String, String, Vec<Entry>)> = Vec::new();
    for (i, page) in (0..pages).step_by(4).enumerate() {
        let sec = next;
        next += 1;
        let mut subs = Vec::new();
        for (j, (p, y)) in [(page, 600), ((page + 2).min(pages - 1), 400)].into_iter().enumerate() {
            // The first subsection by a GoTo action, the second by /Dest.
            let target = if j == 0 {
                format!("/A << /S /GoTo /D {} >>", dest(p, y))
            } else {
                format!("/Dest {}", dest(p, y))
            };
            subs.push((next, format!("({}.{} Part {})", i + 1, j + 1, ["a", "b"][j]), target));
            next += 1;
        }
        let title = format!("({} Section on page {})", i + 1, page + 1);
        sections.push((sec, title, format!("/Dest {}", dest(page, 740)), subs));
    }
    objs.resize(next - 1, String::new());
    let links = |list: &[usize], n: usize| {
        let mut s = String::new();
        if n > 0 {
            s += &format!(" /Prev {} 0 R", list[n - 1]);
        }
        if n + 1 < list.len() {
            s += &format!(" /Next {} 0 R", list[n + 1]);
        }
        s
    };
    let sec_nums: Vec<usize> = sections.iter().map(|s| s.0).collect();
    for (n, (num, title, target, subs)) in sections.iter().enumerate() {
        let sub_nums: Vec<usize> = subs.iter().map(|s| s.0).collect();
        for (m, (snum, stitle, starget)) in subs.iter().enumerate() {
            objs[snum - 1] = format!(
                "<< /Title {stitle} /Parent {num} 0 R {starget}{} >>",
                links(&sub_nums, m)
            );
        }
        objs[num - 1] = format!(
            "<< /Title {title} /Parent {root} 0 R {target}{} /First {} 0 R /Last {} 0 R /Count {} >>",
            links(&sec_nums, n),
            sub_nums[0],
            sub_nums[sub_nums.len() - 1],
            sub_nums.len()
        );
    }
    objs[root - 1] = format!(
        "<< /Type /Outlines /First {} 0 R /Last {} 0 R /Count {} >>",
        sec_nums[0],
        sec_nums[sec_nums.len() - 1],
        sec_nums.len()
    );
    root
}
