#!/usr/bin/env bash
set -euo pipefail
rev=ee4031fc9eb65702718f7ffbfedb451ad6bba541
dest=vendor/cfsolve
if [ ! -d "$dest/.git" ]; then
  mkdir -p vendor
  git clone --filter=blob:none https://github.com/chris124567/cfsolve "$dest"
fi
git -C "$dest" fetch --quiet origin "$rev"
git -C "$dest" checkout --quiet "$rev"
