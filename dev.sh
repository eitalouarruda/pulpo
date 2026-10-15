#!/usr/bin/env bash
# Pulpo — script de desarrollo local (Linux)
#
# Uso:
#   ./dev.sh              # arranca la app en modo dev (tauri dev)
#   ./dev.sh --update-ubo # regenera los scriptlets de uBlock Origin y arranca
#   ./dev.sh --bench      # arranca con el benchmark de navegación de YouTube Music
set -euo pipefail

cd "$(dirname "$0")"

for arg in "$@"; do
  case "$arg" in
    --update-ubo) node scripts/update-ubo-resources.mjs ;;
    --bench) export PULPO_BENCH=1 ;;
    *) echo "flag desconocido: $arg"; exit 1 ;;
  esac
done

if [ ! -d node_modules ]; then
  echo "==> Instalando dependencias del frontend..."
  pnpm install
fi

# Vite usa strictPort: si quedó un servidor colgado en 1440, tauri dev falla.
if ss -ltn 2>/dev/null | grep -q ':1440 '; then
  echo "El puerto 1440 ya está en uso (¿otro ./dev.sh abierto?). Ciérralo antes de arrancar."
  exit 1
fi

exec pnpm tauri dev
