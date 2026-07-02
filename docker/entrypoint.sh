#!/bin/sh
# Entrypoint común de las imágenes de módulo. Cada imagen fija ROLE
# (qkc|orr|dkms|sdn). Convierte el /config/node.yml simple en la config real y
# arranca el binario. Escape hatch: si montas un TOML crudo se usa tal cual.
set -e
: "${ROLE:?ROLE no definido en la imagen}"
SRC="${CONFIG_SRC:-/config}"
RENDER="${RENDER_DIR:-/run/cfg}"
mkdir -p "$RENDER"

if [ "$ROLE" = "qkc" ] && [ -f "$SRC/qkc.toml" ]; then
  cp "$SRC/qkc.toml" "$RENDER/qkc.toml"                       # TOML crudo (qkc)
elif [ "$ROLE" != "qkc" ] && [ -f "$SRC/default.toml" ]; then
  cp "$SRC/default.toml" "$RENDER/default.toml"               # TOML crudo (otros)
  [ -d "$SRC/topology" ] && cp -r "$SRC/topology" "$RENDER/topology"
elif [ -f "$SRC/node.yml" ]; then
  python3 /opt/render_config.py "$ROLE" "$SRC/node.yml" "$RENDER"   # node.yml -> TOML
else
  echo "entrypoint: falta $SRC/node.yml (o un TOML crudo $SRC/{qkc.toml|default.toml})" >&2
  exit 1
fi

BIN="/usr/local/bin/$ROLE"
if [ "$ROLE" = "qkc" ]; then
  exec "$BIN" --config "$RENDER/qkc.toml"
else
  export CONFIG_DIR="$RENDER"
  exec "$BIN"
fi
