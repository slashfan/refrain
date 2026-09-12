# AGENTS.md

This file provides guidance to coding agents — Claude Code (claude.ai/code) and any
other tool that reads `AGENTS.md` — when working with code in this repository.
`CLAUDE.md` is a symlink to this file: one source, no drift.

Ce fichier est en français, comme tout ce que lit un contributeur ici.

## Deux documents font autorité

- **[CONTRIBUTING.md](CONTRIBUTING.md)** — la procédure : ticket, branche,
  pull request, fusion en *rebase*. **Rien n'arrive sur `main` autrement que par
  une pull request dont la CI est verte**, y compris une correction d'une ligne.
  Le hook `.githooks/pre-push` le rappelle ; il s'installe une fois par clone :
  `git config core.hooksPath .githooks`.
- **[README.md](README.md)** — ce que fait l'outil, ses options, ses limites
  connues, et le tableau « How the code is laid out » qui donne le rôle de
  chaque fichier. `README.fr.md` en est la traduction française.

Ce fichier-ci ne répète ni l'un ni l'autre : il dit ce qu'il faut avoir en tête
avant d'écrire la première ligne.

## La langue n'est pas un détail

- **Ce que voit un utilisateur est en anglais** : l'aide de `clap`, les onglets,
  les résumés, le JSON, les messages d'erreur.
- **Ce que lit un contributeur est en français** : les commentaires, les messages
  de commit, les noms de tests, ce fichier.

Toute chaîne affichée qui change doit être répercutée dans **les deux README**,
et le GIF de tête se refait (`./docs/demo.sh`) si l'interface a bougé.

## Commandes

```bash
cargo test                       # 68 tests : 58 unitaires + 10 de bout en bout
cargo test le_plafond_des_routes # un seul test, par son nom (en français)
cargo test --test cli            # seulement les tests de bout en bout
cargo test --lib stats::         # seulement les tests d'un module
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check

cargo run -- var/log/prod.log                # le tableau de bord
cargo run --bin genlogs -- --rate 0 --count 5000 essai.log   # des logs de test
cargo run --release --bin bench              # le banc de débit
cargo run --release --bin bench -- --requests 20000 --min 100000   # ce que lance la CI
```

Rust 1.88 minimum (ratatui 0.30 l'exige), édition 2024. La CI compile avec
`--locked` : monter une version de `Cargo.toml` impose `cargo update --workspace`
dans le même commit.

Les tests de bout en bout lancent les vrais binaires via `CARGO_BIN_EXE_*` :
`cargo test` suffit, Cargo les compile pour eux.

## Quatre invariants d'architecture

1. **Une bibliothèque, puis des binaires** — et non un binaire unique. C'est ce
   qui permet à `src/bin/bench.rs` d'appeler `parse_line` et `Stats::ingest`
   directement pour dire lequel des deux coûte quoi.
2. **Un seul thread touche à l'état.** Le suivi de fichiers, le clavier et
   l'horloge *poussent* leurs événements dans le canal `mpsc` de `src/event.rs` ;
   la boucle principale les lit. Il n'y a aucun verrou dans ce projet — ne pas en
   introduire.
3. **`app.rs` décide, `ui.rs` dessine.** Les tableaux triés sont recalculés une
   fois par battement d'horloge, jamais à chaque image : c'est ce qui garde le
   rendu quasi gratuit à 100 000 lignes par seconde.
4. **La mémoire est bornée.** Toute table indexée par une clé venue des logs a un
   plafond (`src/stats.rs` : `MAX_ROUTES`, `MAX_ERRORS`, `MAX_CHANNELS`,
   `MAX_SQL_SHAPES`, `MAX_OPEN_REQUESTS`, `MAX_NPLUS1`). Un plafond atteint cesse
   de **détailler**, jamais de **compter** — et chacun a son test. C'est ce qui
   permet d'avaler 40 Go sans que la consommation bouge.

## Trois modes, une seule chaîne de lecture

`src/main.rs` câble trois chemins au-dessus du même `tail` → `parser` → `stats` :
`run_tui` (le défaut), `run_report` (`--summary`, ou `--json` sans `--every`) et
`run_json_stream` (`--json --every`, du NDJSON). Un comportement de lecture qui
change les concerne tous les trois.

## Les codes de sortie sont un contrat

| Code | Cause |
| --- | --- |
| 0 | tout va bien |
| 1 | une source n'a pas pu être lue |
| 2 | la ligne de commande est fautive (rendu par `clap`) |
| 3 | un seuil `--fail-if` est franchi |

Une source illisible **prime** sur un seuil franchi : sans tout lire, les chiffres
ne veulent rien dire, et un job doit distinguer « l'application va mal » de
« refrain n'a rien pu lire ». Le 3 existe parce que `clap` occupe déjà le 2. Ces
codes sont vérifiés par `tests/cli.rs` : les changer, c'est casser des crons.

## Ce que la CI ne rattrape pas tout de suite

Le dépôt est privé, où une minute macOS est facturée dix fois le tarif Linux :
sur une pull request, **seul Linux tourne**. macOS s'exécute à la fusion sur
`main`, sur les tags et en lancement manuel. Une régression propre à macOS se
voit donc après coup.

## Écrire du code qu'on relira

- **Les commentaires disent le pourquoi**, pas le quoi : pourquoi cette borne,
  pourquoi ce plafond, pourquoi ce compromis. Le projet est écrit ainsi de bout
  en bout, y compris dans les en-têtes de module (`//!`) qui expliquent chaque
  fichier avant qu'on le lise.
- **Un comportement corrigé vient avec son test**, et les tests portent des noms
  français qui énoncent la règle vérifiée.
- **Le parseur ne doit jamais paniquer** : il est éprouvé sur dix-sept mille
  lignes tordues — troncatures à chaque position, puis mutations à graine fixe.
