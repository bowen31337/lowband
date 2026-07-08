#!/bin/bash
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Override the linker for the host target (x86_64-unknown-linux-gnu).
# This env var is recognised by cargo and applied to build scripts too.
# Our wrapper prepends -L/tmp/rust-libs before any -l flags so rust-lld
# (which processes -L/-l in order) can find the system shared libraries.
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/bin/rust-lld"
/home/ubuntu/.cargo/bin/cargo test -p lowband-xfer "$@" &&
exec /home/ubuntu/.cargo/bin/cargo test -p lowband-messaging "$@"
