#!/bin/sh
set -eu

cargo build --release
sudo install -o root -g root -m 0755 target/release/RustyPac /usr/local/bin/RustyPac
