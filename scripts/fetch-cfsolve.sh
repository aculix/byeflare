#!/usr/bin/env bash
set -euo pipefail
rev=19efa31656328528457376ae0895a61164b22da0
dest=vendor/cfsolve
if [ ! -d "$dest/.git" ]; then
  mkdir -p vendor
  git clone --filter=blob:none https://github.com/chris124567/cfsolve "$dest"
fi
git -C "$dest" fetch --quiet origin "$rev"
git -C "$dest" checkout --quiet "$rev"
# `stage2/benchmark.rs` calls the `internals`-gated re-export `crate::deobfuscate(...)`, which
# breaks the no-default-features build this crate is used with. The module itself is ungated.
perl -pi -e 's/crate::deobfuscate\(/crate::deobfuscate::deobfuscate(/g' \
  "$dest/src/stage2/benchmark.rs"
