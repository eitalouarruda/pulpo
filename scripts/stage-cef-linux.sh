#!/usr/bin/env bash
# cef-dll-sys assumes the old Cargo OUT_DIR layout. With Cargo's nested build
# directories it stages CEF under debug/build instead of next to the executable.
set -euo pipefail
profile=$(realpath "${1:?usage: stage-cef-linux.sh <target/profile>}")
if [ ! -f "$profile/libcef.so" ]; then
  runtime=$(find "$profile/build" -name libcef.so -type f -print -quit)
  if [ -z "$runtime" ]; then
    echo '::error::CEF runtime missing from the Cargo build directory'
    exit 1
  fi
  runtime=$(dirname "$runtime")
  echo "Staging CEF from $runtime into $profile"
  # Same files as cef-dll-sys: top-level runtime files plus locales.
  find "$runtime" -maxdepth 1 -type f -exec cp -t "$profile" -- {} +
  cp -a "$runtime/locales" "$profile/"
fi
for file in libcef.so libEGL.so libGLESv2.so icudtl.dat resources.pak chrome-sandbox; do
  test -f "$profile/$file" || { echo "::error::Missing CEF runtime file: $file"; exit 1; }
done
# Fail here with the missing library names, before starting the UI test.
ldd "$profile/pulpo" | tee "$profile/cef-linkage.log"
if grep -q 'not found' "$profile/cef-linkage.log"; then
  echo '::error::Linux application has unresolved shared libraries'
  exit 1
fi
