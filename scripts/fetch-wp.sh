#!/usr/bin/env bash
# Vendor a pinned WordPress release into vendor/wordpress/ — the corpus for the
# second oracle (examples/wpscan.rs). Same pattern as fetch-corpus.sh: a plain
# download, never committed (vendor/ is gitignored).
set -euo pipefail
cd "$(dirname "$0")/.."

WP_VERSION="${WP_VERSION-6.7.1}"
DEST="vendor/wordpress"

fetch_plugin() {
  PLUG="$DEST/wp-content/plugins/sqlite-database-integration"
  if [ ! -d "$PLUG" ]; then
    echo "Fetching sqlite-database-integration plugin ..."
    curl -fsSL "https://downloads.wordpress.org/plugin/sqlite-database-integration.latest-stable.zip" -o vendor/sqlite-plugin.zip
    unzip -q vendor/sqlite-plugin.zip -d "$DEST/wp-content/plugins/"
    rm vendor/sqlite-plugin.zip
    echo "Plugin files: $(find "$PLUG" -name '*.php' | wc -l)"
  fi
}

if [ -f "$DEST/wp-settings.php" ]; then
  echo "WordPress already present in $DEST ($(grep -m1 "wp_version = " "$DEST/wp-includes/version.php" 2>/dev/null || echo unknown))"
  fetch_plugin
  exit 0
fi

echo "Fetching WordPress $WP_VERSION ..."
mkdir -p vendor
curl -fsSL "https://wordpress.org/wordpress-$WP_VERSION.tar.gz" -o vendor/wp.tar.gz
tar -xzf vendor/wp.tar.gz -C vendor
rm vendor/wp.tar.gz
mv vendor/wordpress "$DEST" 2>/dev/null || true
echo "Done. Files:"
find "$DEST" -name "*.php" | wc -l

fetch_plugin
