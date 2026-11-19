//! File / folder icons from the vendored Material Icon Theme (VS Code's
//! `material-icon-theme`, MIT). Matching follows VS Code icon themes: exact
//! file name, then the longest dotted extension, then the default icon.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

use gpui_kit::*;
use serde::Deserialize;

include!(concat!(env!("OUT_DIR"), "/file_icons.rs"));

const PREFIX: &str = "file-icons/";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    file: String,
    folder: String,
    file_extensions: HashMap<String, String>,
    file_names: HashMap<String, String>,
    folder_names: HashMap<String, String>,
}

fn manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/file-icons/manifest.json"))
            .expect("bundled file-icon manifest")
    })
}

fn icon_name(name: &str, is_dir: bool) -> &'static str {
    let m = manifest();
    let lower = name.to_lowercase();
    if is_dir {
        return m.folder_names.get(&lower).unwrap_or(&m.folder);
    }
    if let Some(icon) = m.file_names.get(&lower) {
        return icon;
    }
    // `a.spec.ts` tries `spec.ts`, then `ts`; a leading dot is part of the name.
    let mut rest = lower.get(1..).unwrap_or_default();
    while let Some(dot) = rest.find('.') {
        rest = &rest[dot + 1..];
        if let Some(icon) = m.file_extensions.get(rest) {
            return icon;
        }
    }
    &m.file
}

/// Asset path for `img()`; served by [`GaleAssets`].
pub fn icon_path(name: &str, is_dir: bool) -> SharedString {
    SharedString::from(format!("{PREFIX}{}.svg", icon_name(name, is_dir)))
}

fn bundled(path: &str) -> Option<&'static [u8]> {
    let name = path.strip_prefix(PREFIX)?.strip_suffix(".svg")?;
    FILE_ICONS
        .binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|ix| FILE_ICONS[ix].1)
}

// Lucide icons beyond gpui-kit's default component set.
assets::icon_assets!(ExtraIcons, [
    ClipboardCopy,
    Download,
    FilePlus,
    FolderPlus,
    KeyRound,
    ListChecks,
    Pencil,
    SquareCheck,
    Upload,
]);

/// gpui-kit's component icons, the extra Lucide icons and the file icons.
pub struct GaleAssets;

impl AssetSource for GaleAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = bundled(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut list = assets::Assets.list(path)?;
        list.extend(ExtraIcons.list(path)?);
        if PREFIX.starts_with(path) || path.starts_with(PREFIX) {
            list.extend(
                FILE_ICONS
                    .iter()
                    .map(|(name, _)| format!("{PREFIX}{name}.svg"))
                    .filter(|p| p.starts_with(path))
                    .map(SharedString::from),
            );
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::{bundled, icon_name, icon_path};

    #[test]
    fn names_extensions_and_folders_resolve() {
        assert_eq!(icon_name("main.rs", false), "rust");
        assert_eq!(icon_name("Dockerfile", false), "docker");
        assert_eq!(icon_name("package.json", false), "nodejs");
        assert_eq!(icon_name("REPORT.PDF", false), "pdf");
        assert_eq!(icon_name("archive.tar.gz", false), "zip");
        assert_eq!(icon_name(".gitignore", false), "git");
        assert_eq!(icon_name("no_extension", false), "file");
        assert_eq!(icon_name("src", true), "folder-src");
        assert_eq!(icon_name("某个目录", true), "folder");
    }

    #[test]
    fn every_mapped_icon_is_bundled() {
        let m = super::manifest();
        for icon in m
            .file_extensions
            .values()
            .chain(m.file_names.values())
            .chain(m.folder_names.values())
            .chain([&m.file, &m.folder])
        {
            assert!(bundled(&format!("file-icons/{icon}.svg")).is_some(), "{icon}");
        }
        assert!(bundled(&icon_path("x.rs", false)).is_some());
    }
}
