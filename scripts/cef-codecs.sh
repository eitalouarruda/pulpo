#!/usr/bin/env bash
# Arma el CEF con códecs propietarios (H.264, AAC) que usa la AppImage.
#
# El CEF oficial (cef-builds.spotifycdn.com) viene sin ellos: Discord manda los
# GIF como MP4/H.264 y quedan quietos, y no se ven los videos MP4 ni se oyen
# los audios AAC. Compilar Chromium lleva horas, así que se reutiliza el
# paquete cef-vaapi de Arch (https://github.com/bnema/cef-vaapi), que es el
# CEF oficial de Arch compilado con proprietary_codecs y VAAPI: de él salen
# libcef.so y los recursos; headers, libcef_dll, CMake y archive.json salen
# de la distribución oficial de la misma versión, con la estructura que
# espera cef-dll-sys (la misma que deja en su caché).
#
# Se sube como asset de la release "cef-<versión>-codecs" y el CI la baja.
# Hay que correrlo cada vez que tauri-runtime-cef cambie de versión de CEF
# (CEF_VERSION en .github/workflows/build.yml), si cef-vaapi ya la tiene.
#
#   scripts/cef-codecs.sh 152.0.6          # arma el .tar.zst aquí
#   scripts/cef-codecs.sh 152.0.6 --upload # y lo sube a GitHub
#
# Necesita curl, jq, bsdtar, zstd y (para subir) gh.
set -euo pipefail

version=${1:?"uso: $0 <versión de CEF, p. ej. 152.0.6> [--upload]"}
upload=${2:-}
pkgrel=${PKGREL:-1}
out="$PWD/cef-$version-codecs-linux-x86_64.tar.zst"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# 1. Distribución oficial "minimal" de esa versión.
read -r name sha1 < <(curl -fsSL https://cef-builds.spotifycdn.com/index.json | jq -r --arg v "$version" '
  .linux64.versions[] | select(.cef_version | startswith($v + "+"))
  | .files[] | select(.type == "minimal") | "\(.name) \(.sha1)"' | head -n 1)
[ -n "${name:-}" ] || { echo "CEF $version no está en cef-builds.spotifycdn.com" >&2; exit 1; }
echo "Bajando $name"
curl -fL --progress-bar -o "$work/official.tar.bz2" "https://cef-builds.spotifycdn.com/${name//+/%2B}"
echo "$sha1  $work/official.tar.bz2" | sha1sum -c --quiet
mkdir "$work/official"
bsdtar -xf "$work/official.tar.bz2" -C "$work/official" --strip-components 1

cef="$work/cef_linux_x86_64"
mkdir "$cef"
cp -a "$work/official/include" "$work/official/libcef_dll" "$work/official/cmake" \
  "$work/official/CMakeLists.txt" "$work/official/CREDITS.html" "$work/official/LICENSE.txt" "$cef/"
cp -a "$work/official/Release/." "$work/official/Resources/." "$cef/"
printf '{\n  "type": "minimal",\n  "name": "%s",\n  "sha1": "%s"\n}\n' "$name" "$sha1" > "$cef/archive.json"

# 2. Los binarios con códecs, del paquete cef-vaapi de la misma versión.
pkg="cef-vaapi-$version-$pkgrel-x86_64.pkg.tar.zst"
echo "Bajando $pkg"
curl -fL --progress-bar -o "$work/$pkg" \
  "https://github.com/bnema/cef-vaapi/releases/download/cef-vaapi-v$version-$pkgrel/$pkg" ||
  { echo "cef-vaapi no tiene la versión $version (todavía)" >&2; exit 1; }
mkdir "$work/pkg"
bsdtar -xf "$work/$pkg" -C "$work/pkg" usr
grep -q "define CEF_VERSION \"$version+" "$work/pkg/usr/include/cef/include/cef_version.h" ||
  { echo "El paquete no es CEF $version" >&2; exit 1; }
# La API tiene que ser exactamente la de los headers oficiales.
cmp -s "$cef/include/cef_api_hash.h" "$work/pkg/usr/include/cef/include/cef_api_hash.h" ||
  { echo "El hash de la API de CEF no coincide con el oficial" >&2; exit 1; }
cp -a "$work/pkg/usr/lib/cef/." "$cef/"

tar -C "$work" -c cef_linux_x86_64 | zstd -19 -T0 -q -o "$out" -f
echo "Listo: $out"
sha256sum "$out"

if [ "$upload" = "--upload" ]; then
  tag="cef-$version-codecs"
  gh release view "$tag" >/dev/null 2>&1 ||
    gh release create "$tag" --title "CEF $version con códecs (para el build)" --prerelease --latest=false \
      --notes "CEF $version con códecs propietarios (H.264, AAC) que usa el build de la AppImage. Generado con scripts/cef-codecs.sh; no es una versión de Pulpo."
  gh release upload "$tag" "$out" --clobber
fi
