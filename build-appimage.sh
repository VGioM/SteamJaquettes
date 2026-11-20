#!/bin/bash
# Construit dist/SteamJaquettes-<version>-linux-x86_64.AppImage (à lancer dans la
# distrobox « rustbuild », Fedora 44, pour rester compatible avec les systèmes
# plus anciens). Besoin : Rust, squashfs-tools ; runtime AppImage dans
# APPIMAGE_RUNTIME (par défaut celui du cache electron-builder).
set -euo pipefail
cd "$(dirname "$0")"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
OUT="dist/SteamJaquettes-$VERSION-linux-x86_64.AppImage"
RUNTIME="${APPIMAGE_RUNTIME:-$(ls "$HOME"/.cache/electron-builder/appimage-*/appimage-*/runtime-x64 2>/dev/null | head -1)}"
[ -f "$RUNTIME" ] || { echo "✗ Runtime AppImage introuvable : donnez son chemin dans APPIMAGE_RUNTIME."; exit 1; }
command -v mksquashfs >/dev/null || { echo "✗ mksquashfs manquant (dnf install squashfs-tools)."; exit 1; }
export PATH="$HOME/.cargo/bin:$PATH"
echo "▸ Tests et compilation…"
cargo test --release --quiet
cargo build --release --quiet
echo "▸ AppImage…"
APPDIR="$(mktemp -d)"; trap 'rm -rf "$APPDIR"' EXIT
mkdir -p "$APPDIR/usr/bin" dist
cp target/release/steamjaquettes "$APPDIR/usr/bin/"
cp resources/steamjaquettes.desktop "$APPDIR/"
cp resources/icon.png "$APPDIR/steamjaquettes.png"
ln -s steamjaquettes.png "$APPDIR/.DirIcon"
printf '#!/bin/sh\nHERE="$(dirname "$(readlink -f "$0")")"\nexec "$HERE/usr/bin/steamjaquettes" "$@"\n' > "$APPDIR/AppRun"
chmod +x "$APPDIR/AppRun"
rm -f "$OUT" "$OUT.squashfs"
mksquashfs "$APPDIR" "$OUT.squashfs" -root-owned -noappend -comp gzip -quiet >/dev/null
cat "$RUNTIME" "$OUT.squashfs" > "$OUT"; rm -f "$OUT.squashfs"; chmod +x "$OUT"
ls -la "$OUT"
