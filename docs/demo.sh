#!/usr/bin/env bash
#
# Fabrique la démo animée du README : docs/demo.gif
#
#   brew install asciinema agg     # expect est déjà là sur macOS
#   cargo build --release
#   ./docs/demo.sh
#
# Le corpus est engendré à graine fixe et le scénario est un fichier versionné
# (docs/demo.exp) : deux prises donnent les mêmes chiffres à l'écran, et le GIF
# se refait à l'identique après une évolution de l'interface.

set -euo pipefail
cd "$(dirname "$0")/.."
racine="$PWD"

if [ ! -x target/release/refrain ] || [ ! -x target/release/genlogs ]; then
    echo "Compilez d'abord : cargo build --release" >&2
    exit 1
fi

# Une taille fixe, sinon le GIF change de dimensions d'une machine à l'autre.
COLONNES=132
# Vingt-huit lignes plutôt que trente-quatre : au-delà, le bas de l'écran est
# vide et le GIF paie des pixels pour rien.
LIGNES=28

atelier=$(mktemp -d)
trap 'rm -rf "$atelier"' EXIT
mkdir -p "$atelier/var/log"

echo "→ corpus"
# Un fond déjà écrit, étalé sur les trois dernières minutes : le tableau de
# bord a de quoi montrer dès la première image, sparklines comprises. Sans
# `--spread`, les neuf cents requêtes porteraient le même horodatage et les
# graphes se réduiraient à une barre unique.
(cd "$atelier" && "$racine/target/release/genlogs" \
    --rate 0 --count 900 --spread 200 --seed 7 var/log/prod.log)

echo "→ enregistrement"
# Un flux vivant par-dessus : c'est un outil temps réel, ça doit bouger. Le
# débit est calé sur celui du fond — plus rapide, la dernière seconde formerait
# un mur qui écraserait tout l'historique du graphe.
(cd "$atelier" && "$racine/target/release/genlogs" \
    --rate 20 --seed 7 var/log/prod.log &
 echo $! > "$atelier/genlogs.pid")
trap 'kill "$(cat "$atelier/genlogs.pid" 2>/dev/null)" 2>/dev/null || true; rm -rf "$atelier"' EXIT

cd "$atelier"
PATH="$racine/target/release:$PATH" asciinema rec \
    --headless \
    --window-size "${COLONNES}x${LIGNES}" \
    --overwrite \
    --command "expect -f $racine/docs/demo.exp" \
    "$atelier/demo.cast"

echo "→ rendu"
# `--idle-time-limit` resserre les temps morts, `--fps-cap` tient le poids du
# GIF : il vit dans l'historique git pour toujours.
# gifsicle rogne encore le tiers du poids : le GIF vit dans l'historique git
# pour toujours, autant qu'il y entre léger.
agg --font-size 12 \
    --theme asciinema \
    --idle-time-limit 1.5 \
    --fps-cap 8 \
    --last-frame-duration 2 \
    "$atelier/demo.cast" "$racine/docs/demo.gif"

echo "→ optimisation"
gifsicle -O3 --lossy=80 --batch "$racine/docs/demo.gif"

cd "$racine"
poids=$(du -h docs/demo.gif | cut -f1)
echo "→ docs/demo.gif ($poids)"
