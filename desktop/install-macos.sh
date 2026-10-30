#!/bin/sh
# Run from the extracted Mac bundle. No Rust toolchain is needed on the Mac.
set -eu

if [ "$(uname -s)" != Darwin ]; then
  printf '%s\n' 'Run this installer on the Mac that runs Herdr desktop.' >&2
  exit 1
fi

case "$(uname -m)" in
  arm64) arch=arm64 ;;
  x86_64) arch=x86_64 ;;
  *) printf '%s\n' 'This bundle supports Apple Silicon and Intel Macs.' >&2; exit 1 ;;
esac

bundle_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
"$bundle_dir/space-usage-$arch" --install-desktop
herdr-app do mod.open '{"name":"space-usage"}'
