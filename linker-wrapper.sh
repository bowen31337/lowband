#!/bin/bash
exec rust-lld -L/tmp/rust-libs "$@"
