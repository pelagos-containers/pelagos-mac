#!/bin/sh
# Cross-linker: zig cc targeting aarch64-linux-musl.
#
# zig 0.15+ always supplies its own startup files (crt1.o, crti.o, etc.)
# even when -nostartfiles is passed, while Rust's self-contained musl target
# also passes its own copies.  Drop Rust's copies so zig wins and there is
# no duplicate _start symbol.
args=""
for arg in "$@"; do
  case "$arg" in
    */crt1.o|*/crti.o|*/crtbegin.o|*/crtend.o|*/crtn.o|-nostartfiles) ;;
    *) args="$args $arg" ;;
  esac
done
exec /opt/homebrew/bin/zig cc -target aarch64-linux-musl $args
