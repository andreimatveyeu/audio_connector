# Makefile for audio-connector
# Convenience wrapper that sets up the JACK pkg-config / library paths,
# transparently handling NixOS where they are not on the default search path.

# Detect if running on NixOS.
IS_NIXOS := $(shell test -d /nix/store && echo yes)

ifeq ($(IS_NIXOS),yes)
    # Nix store paths are shallow (/nix/store/<hash>-pkg/lib/...), so -maxdepth
    # keeps these scans fast; 2>/dev/null swallows the SIGPIPE from `head`.
    pc_dir = $(shell find /nix/store -maxdepth 4 -name jack.pc -path "*$(1)*" 2>/dev/null | head -1 | xargs -r dirname 2>/dev/null)
    lib_dir = $(shell find /nix/store -maxdepth 4 -name libjack.so.0 -path "*$(1)*" 2>/dev/null | head -1 | xargs -r dirname 2>/dev/null)

    # jack.pc for the build-time pkg-config probe (prefer libjack2, fall back to jack2).
    JACK_PC_PATH := $(or $(call pc_dir,libjack2),$(call pc_dir,jack2))
    export PKG_CONFIG_PATH := $(JACK_PC_PATH)

    # libjack.so.0 for the runtime dlopen (prefer PipeWire's JACK implementation).
    JACK_LIB_PATH := $(or $(call lib_dir,pipewire),$(call lib_dir,libjack2))
    export LD_LIBRARY_PATH := $(JACK_LIB_PATH)
endif

.PHONY: all build release run run-release test check clippy fmt clean help

all: build

build:
	cargo build

release:
	cargo build --release

# Run against the bundled example config.
run: build
	./target/debug/audio-connector examples/connections.toml

run-release: release
	./target/release/audio-connector examples/connections.toml

test:
	cargo test

check:
	cargo check

clippy:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt --all

clean:
	cargo clean

help:
	@echo "audio-connector Makefile targets:"
	@echo "  build       - Debug build"
	@echo "  release     - Release build"
	@echo "  run         - Build and run with examples/connections.toml"
	@echo "  run-release - Release build and run with examples/connections.toml"
	@echo "  test        - Run tests"
	@echo "  check       - cargo check"
	@echo "  clippy      - Lint (warnings as errors)"
	@echo "  fmt         - Format the code"
	@echo "  clean       - Remove build artifacts"
ifeq ($(IS_NIXOS),yes)
	@echo ""
	@echo "NixOS detected"
	@echo "  PKG_CONFIG_PATH = $(PKG_CONFIG_PATH)"
	@echo "  LD_LIBRARY_PATH = $(LD_LIBRARY_PATH)"
endif
