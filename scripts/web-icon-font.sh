#!/usr/bin/env bash
# Rebuild src/web/agents.woff2, the web console's icon font: a subset of
# Nerd Fonts' "Symbols Nerd Font" with every glyph the current file has
# (agent icons) plus the worker glyphs below. Keep WORKER_GLYPHS in sync
# with `workerGlyphs` in src/web/app.js and src/pool_icon.rs.
#
#   nix shell --impure --expr 'let pkgs = (builtins.getFlake "nixpkgs").legacyPackages.${builtins.currentSystem}; in pkgs.symlinkJoin { name = "rworker-icon-tools"; paths = [ pkgs.nerd-fonts.symbols-only (pkgs.python3.withPackages (p: [ p.fonttools p.brotli ])) ]; }' \
#     -c bash scripts/web-icon-font.sh
set -euo pipefail

# nf-md-tools, nf-md-robot, nf-md-server, nf-md-tent, nf-md-web, nf-fa-github
WORKER_GLYPHS=(U+F1064 U+F06A9 U+F048B U+F0508 U+F059F U+F09B)

usage() {
  echo "usage: $(basename "$0") [SYMBOLS_FONT]" >&2
  echo "SYMBOLS_FONT defaults to SymbolsNerdFont-Regular.ttf found via fc-match or the nix profile." >&2
}

find_symbols_font() {
  local font
  font="$(fc-match -f '%{file}' 'Symbols Nerd Font' 2>/dev/null || true)"
  if [[ -n "$font" && "$font" == *Symbols* ]]
  then
    printf '%s\n' "$font"
    return 0
  fi
  local dir
  for dir in ${PATH//:/ }
  do
    font="$(find "${dir%/bin}/share/fonts" -name 'SymbolsNerdFont-Regular.ttf' 2>/dev/null | head -n 1 || true)"
    if [[ -n "$font" ]]
    then
      printf '%s\n' "$font"
      return 0
    fi
  done
  return 1
}

main() {
  if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]
  then
    usage
    return 0
  fi

  local root out source
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
  out="$root/src/web/agents.woff2"
  source="${1:-}"
  if [[ -z "$source" ]]
  then
    source="$(find_symbols_font)" || {
      echo "Symbols Nerd Font not found; pass its path" >&2
      return 1
    }
  fi

  local existing
  existing="$(python3 - "$out" <<'EOF'
import sys
from fontTools.ttLib import TTFont
print(",".join(f"U+{cp:04X}" for cp in sorted(TTFont(sys.argv[1]).getBestCmap())))
EOF
)"

  local unicodes
  unicodes="$(IFS=,; echo "${existing:+$existing,}${WORKER_GLYPHS[*]}")"
  pyftsubset "$source" \
    --unicodes="$unicodes" \
    --flavor=woff2 \
    --layout-features='' \
    --no-hinting \
    --desubroutinize \
    --output-file="$out.new"
  mv "$out.new" "$out"
  echo "wrote $out ($(wc -c < "$out") bytes) with $unicodes"
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]
then
  main "$@"
fi

# vim: set ft=sh et ts=2 sw=2 :
