//! Embeds the Studio's web UI (studio/dist, from `npm run build` in studio/) in the binary: writes
//! OUT_DIR/studio_assets.rs, a table of (path, content type, bytes) that src/studio.rs serves.
//! Without studio/dist, a page saying how to build it stands in, so `cargo build` never needs Node.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const FALLBACK: &str = "<!doctype html><meta charset=\"utf-8\"><title>Chronos Studio</title>\
<p>Studio isn't built into this binary: run <code>npm ci &amp;&amp; npm run build</code> in studio/, then rebuild chronos.</p>";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let dist = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("studio/dist");
    // a path that doesn't exist would rerun this (and rebuild the crate) every time: watch studio/
    // (sources only, without Node) until dist/ appears
    println!("cargo:rerun-if-changed={}", if dist.is_dir() { "studio/dist" } else { "studio" });
    let mut files = vec![];
    if dist.join("index.html").is_file() {
        walk(&dist, &mut files);
    }
    files.sort();
    let mut out = String::from("pub static ASSETS: &[(&str, &str, &[u8])] = &[\n");
    if files.is_empty() {
        writeln!(out, "    (\"index.html\", \"text/html; charset=utf-8\", {FALLBACK:?}.as_bytes()),").unwrap();
    }
    for f in &files {
        let rel = f.strip_prefix(&dist).unwrap().to_string_lossy().replace('\\', "/");
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
        let kind = match ext {
            "html" => "text/html; charset=utf-8",
            "js" | "mjs" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "svg" => "image/svg+xml",
            "json" => "application/json",
            "png" => "image/png",
            "ico" => "image/x-icon",
            "woff2" => "font/woff2",
            "txt" => "text/plain; charset=utf-8",
            _ => continue, // source maps and anything unexpected stay out
        };
        writeln!(out, "    ({rel:?}, {kind:?}, include_bytes!({:?})),", f.to_string_lossy()).unwrap();
    }
    out.push_str("];\n");
    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("studio_assets.rs");
    std::fs::write(dest, out).unwrap();
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}
