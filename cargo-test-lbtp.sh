#!/bin/bash
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/bin/rust-lld"
exec /home/ubuntu/.cargo/bin/cargo test -p lowband-lbtp "$@"
