#!/bin/bash
CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu cargo test -p lowband-messaging "$@"
