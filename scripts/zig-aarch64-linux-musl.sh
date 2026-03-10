#!/bin/sh
# Rust's aarch64-unknown-linux-musl target is self-contained: it supplies its
# own crt1.o / crti.o / crtn.o.  Tell zig cc not to add its own copies.
exec /opt/homebrew/bin/zig cc -target aarch64-linux-musl -nostartfiles "$@"
