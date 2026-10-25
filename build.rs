//! Builds MuPDF (from vendor/, see scripts/fetch-mupdf.sh) into static
//! libraries with its own makefile, and the C shim that farleaf calls
//! (src/mupdf.c). Only what a PDF viewer needs is compiled: no other document
//! formats, no JavaScript, no fonts beyond the base 14 and one CJK fallback.

use std::path::{Path, PathBuf};
use std::process::Command;

const MUPDF_VERSION: &str = "1.28.5";

/// Compiled out: other formats and outputs, scripting, hyphenation, and the
/// big Noto font collection (PDFs embed their fonts; the base 14 and a CJK
/// fallback stay).
const FEATURES: &[&str] = &[
    "-DFZ_ENABLE_XPS=0",
    "-DFZ_ENABLE_SVG=0",
    "-DFZ_ENABLE_CBZ=0",
    "-DFZ_ENABLE_IMG=0",
    "-DFZ_ENABLE_HTML=0",
    "-DFZ_ENABLE_MD=0",
    "-DFZ_ENABLE_FB2=0",
    "-DFZ_ENABLE_MOBI=0",
    "-DFZ_ENABLE_EPUB=0",
    "-DFZ_ENABLE_OFFICE=0",
    "-DFZ_ENABLE_TXT=0",
    "-DFZ_ENABLE_HTML_ENGINE=0",
    "-DFZ_ENABLE_OCR_OUTPUT=0",
    "-DFZ_ENABLE_DOCX_OUTPUT=0",
    "-DFZ_ENABLE_ODT_OUTPUT=0",
    "-DFZ_ENABLE_JS=0",
    "-DFZ_ENABLE_BARCODE=0",
    "-DFZ_ENABLE_HYPHEN=0",
    "-DTOFU",
    "-DTOFU_CJK_EXT",
];

fn main() {
    let src = std::env::var_os("FARLEAF_MUPDF_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"))
                .join(format!("vendor/mupdf-{MUPDF_VERSION}-source"))
        });
    println!("cargo:rerun-if-changed=src/mupdf.c");
    println!("cargo:rerun-if-env-changed=FARLEAF_MUPDF_SRC");
    println!("cargo:rerun-if-changed={}", src.join("Makefile").display());
    if !src.join("Makefile").exists() {
        panic!(
            "\n\nMuPDF's source is not at {}.\nRun scripts/fetch-mupdf.sh first (or set FARLEAF_MUPDF_SRC).\n\n",
            src.display()
        );
    }
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("set by cargo")).join("mupdf");
    std::fs::create_dir_all(&out_dir).expect("creating the MuPDF build directory");
    // make cannot handle spaces in paths, which a checkout's path may have:
    // build through a link that has none.
    let out = if out_dir.to_string_lossy().contains(' ') {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        out_dir.hash(&mut h);
        let link = std::env::temp_dir().join(format!("farleaf-mupdf-{:016x}", h.finish()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&out_dir, &link).expect("linking the MuPDF build directory");
        link
    } else {
        out_dir.clone()
    };
    let target = std::env::var("TARGET").expect("set by cargo");

    let cc = cc::Build::new().get_compiler();
    let cxx = cc::Build::new().cpp(true).get_compiler();
    let host = std::env::var("HOST").expect("set by cargo");
    // Linux from a Mac (cargo zigbuild): macOS's ar and ranlib write archive
    // indexes Linux linkers cannot read; zig's can.
    let (ar, ranlib) =
        if host.contains("apple") && target.contains("linux") && command_output("zig", &["version"]).is_some() {
            ("zig ar".to_string(), "zig ranlib".to_string())
        } else {
            let ar = cc::Build::new()
                .get_archiver()
                .get_program()
                .to_string_lossy()
                .into_owned();
            let ranlib = if host.contains("apple") {
                "xcrun ranlib"
            } else {
                "ranlib"
            };
            (ar, ranlib.to_string())
        };
    let mut cflags: Vec<String> = vec!["-O2".into(), "-fPIC".into(), "-w".into()];
    cflags.extend(FEATURES.iter().map(|f| f.to_string()));
    let mut cxxflags = Vec::new();
    if target.contains("apple") {
        // Some command line tools installs leave the C++ headers out of the
        // default search path: name the SDK and its headers outright.
        if let Some(sdk) = command_output("xcrun", &["--show-sdk-path"]) {
            cflags.push(format!("-isysroot{sdk}"));
            let cpp = Path::new(&sdk).join("usr/include/c++/v1");
            if cpp.exists() {
                cxxflags.push(format!("-I{}", cpp.display()));
            }
        }
    }
    let jobs = std::env::var("NUM_JOBS").unwrap_or_else(|_| "4".into());
    let status = Command::new("make")
        .current_dir(&src)
        .arg(format!("-j{jobs}"))
        .arg(format!("OUT={}", out.display()))
        .args([
            "build=release",
            "libs",
            "HAVE_X11=no",
            "HAVE_GLUT=no",
            "HAVE_WAYLAND=no",
            "HAVE_CURL=no",
            "HAVE_TESSERACT=no",
            "HAVE_LEPTONICA=no",
            "HAVE_OBJCOPY=no",
            "verbose=no",
        ])
        .arg(format!("CC={}", command_line(&cc)))
        .arg(format!("CXX={}", command_line(&cxx)))
        .arg(format!("AR={ar}"))
        .arg(format!("RANLIB={ranlib}"))
        .arg(format!("XCFLAGS={}", cflags.join(" ")))
        .arg(format!("XCXXFLAGS={}", cxxflags.join(" ")))
        // The makefile picks its own -arch from this on macOS.
        .env_remove("ARCHFLAGS")
        .status()
        .expect("running make (MuPDF's build needs make and a C/C++ compiler)");
    assert!(status.success(), "building MuPDF failed");

    let mut shim = cc::Build::new();
    shim.file("src/mupdf.c")
        .include(src.join("include"))
        .flag_if_supported("-w");
    for f in FEATURES {
        let (k, v) = f
            .trim_start_matches("-D")
            .split_once('=')
            .unwrap_or((f.trim_start_matches("-D"), "1"));
        shim.define(k, v);
    }
    shim.compile("farleaf_mupdf");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=mupdf");
    println!("cargo:rustc-link-lib=static=mupdf-third");
    // HarfBuzz, inside MuPDF, is C++.
    if target.contains("apple") {
        println!("cargo:rustc-link-lib=c++");
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
    if target.contains("linux") {
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=pthread");
    }
}

/// A compiler as one command line: program and its target flags.
fn command_line(tool: &cc::Tool) -> String {
    let mut s = tool.path().to_string_lossy().into_owned();
    for a in tool.args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}

fn command_output(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
