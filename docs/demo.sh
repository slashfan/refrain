#!/usr/bin/env bash
#
# Builds the README's animated demo: docs/demo.gif
#
#   brew install asciinema agg     # expect ships with macOS
#   cargo build --release
#   ./docs/demo.sh
#
# The corpus is generated from a fixed seed and the scenario is a versioned
# file (docs/demo.exp): two takes give the same figures on screen, and the GIF
# can be remade identically after an interface change.

set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"

if [ ! -x target/release/refrain ] || [ ! -x target/release/genlogs ]; then
    echo "Build first: cargo build --release" >&2
    exit 1
fi

# A fixed size, otherwise the GIF changes dimensions from one machine to another.
WIDTH=132
# Twenty-eight lines rather than thirty-four: beyond that the bottom of the
# screen is empty and the GIF pays pixels for nothing.
HEIGHT=28

workshop=$(mktemp -d)
trap 'rm -rf "$workshop"' EXIT
mkdir -p "$workshop/var/log"

echo "→ corpus"
# A backdrop already written, spread over the last three minutes: the dashboard
# has something to show from the very first frame, sparklines included. Without
# `--spread`, the nine hundred requests would carry the same timestamp and the
# graphs would collapse into a single bar.
(cd "$workshop" && "$root/target/release/genlogs" \
    --rate 0 --count 900 --spread 200 --seed 7 var/log/prod.log)

echo "→ recording"
# A live stream on top: this is a real-time tool, it has to move. The rate is
# paced on the backdrop's — any faster and the last second would form a wall
# crushing the whole history of the graph.
(cd "$workshop" && "$root/target/release/genlogs" \
    --rate 20 --seed 7 var/log/prod.log &
 echo $! > "$workshop/genlogs.pid")
trap 'kill "$(cat "$workshop/genlogs.pid" 2>/dev/null)" 2>/dev/null || true; rm -rf "$workshop"' EXIT

cd "$workshop"
PATH="$root/target/release:$PATH" asciinema rec \
    --headless \
    --window-size "${WIDTH}x${HEIGHT}" \
    --overwrite \
    --command "expect -f $root/docs/demo.exp" \
    "$workshop/demo.cast"

echo "→ rendering"
# `--idle-time-limit` tightens the dead time, `--fps-cap` holds the weight of
# the GIF: it lives in git history forever.
# gifsicle shaves off another third: since the GIF lives in git history
# forever, it may as well enter it light.
agg --font-size 12 \
    --theme asciinema \
    --idle-time-limit 1.5 \
    --fps-cap 8 \
    --last-frame-duration 2 \
    "$workshop/demo.cast" "$root/docs/demo.gif"

echo "→ optimising"
# `--colors 255` is gifsicle's own suggestion, and the one that pays: without
# it each frame carries a local colour table, which on a forty-second capture
# is most of the file. The GIF lives in git history forever.
gifsicle -O3 --colors 255 --lossy=80 --batch "$root/docs/demo.gif"

cd "$root"
weight=$(du -h docs/demo.gif | cut -f1)
echo "→ docs/demo.gif ($weight)"
