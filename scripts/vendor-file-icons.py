#!/usr/bin/env python3
"""Vendor the Material Icon Theme (MIT) into apps/gale-desktop/assets/file-icons.

Only icons referenced by file-extension / file-name / folder-name mappings are
kept (no light, high-contrast or "open folder" variants). Each SVG gets an
explicit 64x64 size because GPUI rasterizes SVG images at their intrinsic size.

    python3 scripts/vendor-file-icons.py [version]
"""

import io
import json
import re
import shutil
import sys
import tarfile
import urllib.request
from pathlib import Path

VERSION = sys.argv[1] if len(sys.argv) > 1 else "5.40.0"
URL = f"https://registry.npmjs.org/material-icon-theme/-/material-icon-theme-{VERSION}.tgz"
OUT = Path(__file__).resolve().parent.parent / "apps/gale-desktop/assets/file-icons"
SIZE = 64


def main() -> None:
    with urllib.request.urlopen(URL) as resp:
        tar = tarfile.open(fileobj=io.BytesIO(resp.read()), mode="r:gz")
    files = {m.name: m for m in tar.getmembers() if m.isfile()}
    read = lambda name: tar.extractfile(files[name]).read()

    theme = json.loads(read("package/dist/material-icons.json"))
    defs = theme["iconDefinitions"]

    def mapping(key: str) -> dict:
        return {k.lower(): v for k, v in theme[key].items() if v in defs}

    manifest = {
        "version": VERSION,
        "file": theme["file"],
        "folder": theme["folder"],
        "fileExtensions": mapping("fileExtensions"),
        "fileNames": mapping("fileNames"),
        "folderNames": mapping("folderNames"),
    }
    used = {manifest["file"], manifest["folder"]}
    for key in ("fileExtensions", "fileNames", "folderNames"):
        used.update(manifest[key].values())

    if OUT.exists():
        shutil.rmtree(OUT)
    (OUT / "icons").mkdir(parents=True)

    svg_tag = re.compile(r"<svg\b[^>]*>")
    for name in sorted(used):
        path = "package/" + defs[name]["iconPath"].replace("./../", "")
        svg = read(path).decode("utf-8")

        def sized(m: re.Match) -> str:
            tag = re.sub(r'\s(width|height)="[^"]*"', "", m.group(0))
            return tag.replace("<svg", f'<svg width="{SIZE}" height="{SIZE}"', 1)

        svg = svg_tag.sub(sized, svg, count=1)
        (OUT / "icons" / f"{name}.svg").write_text(svg, encoding="utf-8")

    (OUT / "manifest.json").write_text(
        json.dumps(manifest, separators=(",", ":"), sort_keys=True), encoding="utf-8"
    )
    (OUT / "LICENSE").write_bytes(read("package/LICENSE"))
    print(f"material-icon-theme {VERSION}: {len(used)} icons -> {OUT}")


if __name__ == "__main__":
    main()
