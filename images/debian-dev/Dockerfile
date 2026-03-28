# debian-dev — Debian bookworm-slim base image with Rust stable toolchain
# and pelagos build dependencies (glibc, libseccomp, libcap, pkg-config).
#
# Use inside the default pelagos Alpine VM for building Rust projects that
# have C library dependencies that require glibc.
#
# Build:
#   pelagos build -t debian-dev:latest images/debian-dev/
#
# Run (mounting host source tree via virtiofs):
#   pelagos run --rm -it \
#     -v $HOME/Projects/pelagos:/workspace \
#     -w /workspace \
#     debian-dev:latest \
#     bash

FROM debian:bookworm-slim

# Suppress debconf interactive prompts (no controlling tty in a build container).
ENV DEBIAN_FRONTEND=noninteractive

# Install system build dependencies.
# libseccomp-dev + libcap-dev are required by the pelagos container runtime.
# pkg-config is required by many *-sys crates.
# curl + ca-certificates are required by rustup.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        libseccomp-dev \
        libcap-dev \
        curl \
        ca-certificates \
        git \
    && rm -rf /var/lib/apt/lists/*

# Install Rust stable via rustup.
# CARGO_HOME / RUSTUP_HOME are set explicitly so the toolchain is in a
# predictable location regardless of the user the container runs as.
ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
      sh -s -- -y --default-toolchain stable --no-modify-path \
    && RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
       /usr/local/cargo/bin/rustc --version \
    && RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
       /usr/local/cargo/bin/cargo --version

WORKDIR /workspace
