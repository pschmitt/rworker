#!/usr/bin/env bash
# Render the Home Assistant brand images in custom_components/rworker/brand/
# from the logo SVGs in assets/:
#
#   icon.png, icon@2x.png            assets/rworker-icon.svg
#   dark_icon.png, dark_icon@2x.png  assets/rworker-icon-dark.svg
#   logo.png, logo@2x.png            assets/rworker.svg and the name in Inter
#   dark_logo.png, dark_logo@2x.png  assets/rworker-dark.svg, light lettering
#
#   nix shell nixpkgs#resvg -c bash scripts/brand-icons.sh
set -euo pipefail

usage() {
  echo "usage: $(basename "$0") [INTER_FONT]" >&2
  echo "INTER_FONT defaults to Inter found via fc-match, else nixpkgs#inter." >&2
}

find_inter() {
  local font
  font=$(fc-match -f '%{file}' 'Inter:bold' 2>/dev/null || true)
  if [[ $font == *Inter* && -f $font ]]
  then
    echo "$font"
    return
  fi
  font="$(nix build --no-link --print-out-paths nixpkgs#inter)/share/fonts/truetype/Inter.ttc"
  [[ -f $font ]] && echo "$font"
}

# Body of an SVG file: everything between its <title> and </svg>.
svg_body() {
  sed -e '1,/<title>/d' -e '/<\/svg>/d' "$1"
}

# Square icon, cropped to the drawing (HA wants little padding).
icon_svg() {
  echo '<svg xmlns="http://www.w3.org/2000/svg" viewBox="4 4 120 120">'
  svg_body "$1"
  echo '</svg>'
}

# The logo with the name beside it.
logo_svg() {
  local src="$1" ink="$2"
  echo '<svg xmlns="http://www.w3.org/2000/svg" viewBox="4 0 420 130">'
  echo '<svg x="0" y="0" width="128" height="128" viewBox="0 0 128 128">'
  svg_body "$src"
  echo '</svg>'
  echo "<text x=\"136\" y=\"88\" font-family=\"Inter\" font-weight=\"700\" font-size=\"68\" letter-spacing=\"-2\" fill=\"$ink\">rworker</text>"
  echo '</svg>'
}

main() {
  case "${1:-}" in
    -h|--help)
      usage
      return 0
      ;;
  esac

  local root font out
  root=$(cd "$(dirname "$0")/.." && pwd)
  font=${1:-$(find_inter)} || {
    echo "Inter not found; pass the path to Inter.ttc" >&2
    return 1
  }
  out="$root/custom_components/rworker/brand"
  TMP_DIR=$(mktemp -d)
  trap 'rm -rf "$TMP_DIR"' EXIT
  local tmp=$TMP_DIR

  icon_svg "$root/assets/rworker-icon.svg" > "$tmp/icon.svg"
  icon_svg "$root/assets/rworker-icon-dark.svg" > "$tmp/dark_icon.svg"
  logo_svg "$root/assets/rworker.svg" '#0b1d36' > "$tmp/logo.svg"
  logo_svg "$root/assets/rworker-dark.svg" '#f2f5fa' > "$tmp/dark_logo.svg"

  local name
  for name in icon dark_icon
  do
    resvg -w 256 -h 256 "$tmp/$name.svg" "$out/$name.png"
    resvg -w 512 -h 512 "$tmp/$name.svg" "$out/$name@2x.png"
  done
  for name in logo dark_logo
  do
    resvg --skip-system-fonts --use-font-file "$font" -h 128 \
      "$tmp/$name.svg" "$out/$name.png"
    resvg --skip-system-fonts --use-font-file "$font" -h 256 \
      "$tmp/$name.svg" "$out/$name@2x.png"
  done
  ls -l "$out"
}

main "$@"
