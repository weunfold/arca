# The entrypoint of the edge service (templates/arca-edge.json reads it in, put on one line by service::entrypoint, as
# `sh -ec <it> arca-edge`). It runs in the caddybuilds/caddy-cloudflare image (Alpine, busybox), as root, and execs
# Caddy on the Caddyfile Arca writes.
#
# Arca writes the whole Caddyfile to /etc/caddy/Caddyfile, on the container's root, which a stopped edge takes too.
# While the edge runs it also keeps a copy on the data volume, /data/Caddyfile. An update rebuilds the root from the
# new image, and a re-created instance starts from the image too: either way the root holds the image's own
# Caddyfile again, and the routes would be gone until the next route change. So before Caddy starts: when the file on
# the root is not one Arca wrote (its first line is Arca's) and the volume keeps one that is, the kept copy goes back
# in place. A root file of Arca's always wins, since it is the newer when a route changed while the edge was stopped.
set -eu
# The two ARCA_EDGE_* names are for the tests; the image sets neither.
LIVE=${ARCA_EDGE_CONFIG:-/etc/caddy/Caddyfile}
KEPT=${ARCA_EDGE_KEPT:-/data/Caddyfile}
CADDY=${ARCA_EDGE_CADDY:-/usr/bin/caddy}
MARK='# Written by Arca from this node'"'"'s routes'
arcas() {
  [ -s "$1" ] && head -n 1 "$1" | grep -qF "$MARK"
}
if ! arcas "$LIVE" && arcas "$KEPT"; then
  cp "$KEPT" "$LIVE.arca-new"
  chmod 600 "$LIVE.arca-new"
  mv -f "$LIVE.arca-new" "$LIVE"
fi
exec "$CADDY" run --config "$LIVE" --adapter caddyfile
