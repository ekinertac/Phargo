#!/usr/bin/env sh
# Vendor the canonical PHP test corpus (the .phpt oracle) into vendor/php-src.
# Shallow clone — we only ever read the tests, never the history.
set -e
cd "$(dirname "$0")/.."
if [ -d vendor/php-src ]; then
  echo "vendor/php-src already exists — pulling latest…"
  git -C vendor/php-src pull --depth 1 --ff-only
else
  echo "Cloning php/php-src (shallow)…"
  git clone --depth 1 https://github.com/php/php-src vendor/php-src
fi
echo "Done. Test files:"
find vendor/php-src -name '*.phpt' | wc -l
