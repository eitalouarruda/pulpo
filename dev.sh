#!/usr/bin/env bash
# Pulpo — script de desarrollo local (Linux)
#
# Siempre arranca una instancia de pruebas: puerto 1450 y datos en .dev-data/
# (apps, sesiones, ajustes, caché y tokens de CI aparte), para tenerla abierta
# junto al Pulpo de uso real sin tocar sus datos.
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

port=1450
# Solo la app usa esta carpeta: XDG_DATA_HOME no se toca aquí porque ahí viven
# también herramientas del sistema (mise, pnpm…).
export PULPO_DATA_HOME="$PWD/.dev-data"
export PULPO_DEV_PORT=$port
echo "==> Instancia de pruebas: datos en $PULPO_DATA_HOME, puerto $port"

if [ ! -d node_modules ]; then
  echo "==> Instalando dependencias del frontend..."
  pnpm install
fi

# Vite usa strictPort: si quedó un servidor colgado en el puerto, tauri dev falla.
if ss -ltn 2>/dev/null | grep -q ":$port "; then
  echo "El puerto $port ya está en uso (¿otro ./dev.sh abierto?). Ciérralo antes de arrancar."
  exit 1
fi

exec pnpm tauri dev --config "{\"build\":{\"devUrl\":\"http://localhost:$port\"}}"
