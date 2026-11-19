//! Embeds the vendored file-icon theme (`assets/file-icons/icons/*.svg`) as a
//! sorted `(name, bytes)` table so lookups can binary-search it.

use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("assets/file-icons/icons");
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut icons: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|entry| {
                let path = entry.ok()?.path();
                let name = path.file_stem()?.to_str()?.to_string();
                (path.extension()? == "svg").then_some((name, path))
            })
            .collect()
        })
        .unwrap_or_default();
    icons.sort();

    let mut out = String::from("pub static FILE_ICONS: &[(&str, &[u8])] = &[\n");
    for (name, path) in &icons {
        writeln!(out, "    ({name:?}, include_bytes!({:?})),", path.display().to_string()).unwrap();
    }
    out.push_str("];\n");

    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("file_icons.rs");
    std::fs::write(dest, out).unwrap();
}
