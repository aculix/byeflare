#!/usr/bin/env bash
set -euo pipefail
rev=6ec4412f1933c4b4d0600fd7db873fc4130b3133
dest=vendor/cfsolve
if [ ! -d "$dest/.git" ]; then
  mkdir -p vendor
  git clone --filter=blob:none https://github.com/chris124567/cfsolve "$dest"
fi
git -C "$dest" fetch --quiet origin "$rev"
git -C "$dest" checkout --quiet "$rev"
