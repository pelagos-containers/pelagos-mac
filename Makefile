.PHONY: all build build-guest sign image test test-e2e

# Default: build, sign, and build the VM image.
all: build sign image

# Build the macOS CLI binary.
build:
	cargo build --release -p pelagos-mac

# Cross-compile the guest daemon for aarch64-unknown-linux-gnu.
# Must use the rustup-managed cargo so the Linux sysroot is available.
build-guest:
	PATH="$$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:$$PATH" \
	    $$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo zigbuild \
	        -p pelagos-guest \
	        --target aarch64-unknown-linux-gnu \
	        --release

# Ad-hoc code-sign the binary with the AVF entitlement.
sign: build
	bash scripts/sign.sh

# Build the VM image (kernel + custom initramfs + placeholder disk).
image:
	bash scripts/build-vm-image.sh

# Run unit tests (host-side; guest tests require Linux target to run).
test:
	cargo test

# Run the end-to-end integration test (requires signed binary + VM image).
test-e2e: sign image
	bash scripts/test-e2e.sh
