# Contribuer à refrain

Ce document est en français, comme les commentaires du code. L'interface de
l'outil, elle, est en anglais — voir [README.md](README.md), dont
[README.fr.md](README.fr.md) est la traduction française.

## La règle

**Rien n'arrive sur `main` autrement que par une pull request dont la CI est
verte.** Pas de commit direct, pas de poussée directe — y compris pour une
correction d'une ligne.

Ce n'est pas de la cérémonie : la CI compile en debug (où Rust vérifie les
dépassements d'entiers), rejoue les 72 tests, passe clippy sans indulgence et
vérifie que le binaire release démarre. C'est ce filet-là qu'un commit direct
contourne.

## La boucle

1. **Un ticket d'abord.** Il porte le pourquoi, les pistes et le critère
   « fait quand ». Si le chantier n'en a pas, l'ouvrir avant de coder.
2. **Une branche depuis `main`**, nommée d'après le chantier, en minuscules et
   tirets : `correlation-multi-sources`, `regle-des-pull-requests`. Pas de
   préfixe `feat/` ni `fix/` — le ticket dit déjà de quoi il s'agit.
3. **Une pull request** rattachée à son jalon, qui référence son ticket
   (`Closes #12`).
4. **La CI verte**, puis la fusion en *rebase* : l'historique reste linéaire, et
   chaque commit y garde son message.

```bash
git switch -c mon-chantier main
# … du code, des tests …
cargo test && cargo clippy --all-targets && cargo fmt --check
git push -u origin mon-chantier
gh pr create --milestone "v0.5.0 — Fenêtres et seuils"
gh pr checks --watch
gh pr merge --rebase --delete-branch
```

## La garde locale

GitHub ne sait pas protéger une branche sur un dépôt privé d'un compte gratuit
(l'API répond `403` sur la protection comme sur les rulesets). En attendant que
la question du dépôt public soit tranchée, un hook versionné refuse la poussée
vers `main`. À installer une fois par clone :

```bash
git config core.hooksPath .githooks
```

Elle attrape le geste distrait, rien de plus : `--no-verify` la contourne, et
elle ne protège que les machines où elle est installée. La vraie barrière, c'est
la discipline — le hook ne fait que la rappeler.

## Traduire

Deux règles simples :

- **Ce que voit un utilisateur est en anglais** : l'aide de la ligne de commande,
  les onglets, les résumés, le JSON, les messages d'erreur. L'écosystème Symfony
  est anglophone.
- **Ce que lit un contributeur est en français** : les commentaires, les messages
  de commit, ce fichier, et les noms des tests.

Toute chaîne affichée qui part en anglais doit avoir sa contrepartie dans les
deux README, et le GIF de la démo se refait — l'interface qu'il montre a changé.

## Écrire un commit

Les messages sont **en français**, comme le reste du projet. Le titre dit ce qui
change, à l'infinitif ou en nom :

```
Corrélation : caler le balayage sur la source la plus en retard
```

Le corps dit **pourquoi**, avec des chiffres quand il y en a — c'est ce qui rend
l'historique lisible dans six mois :

> Sur les mêmes 23 299 lignes, selon qu'elles sont dans un fichier ou deux :
> SQL/req 28,8 contre 7,3.

Un commit par idée. Deux corrections sans rapport font deux commits, et souvent
deux pull requests.

## Ce que la CI vérifie

| Job | Contenu |
| --- | --- |
| `Tests · ubuntu-latest` | `cargo build --all-targets`, `cargo test`, compilation release, `refrain --version` |
| `Format et clippy` | `cargo fmt --check`, `cargo clippy -- -D warnings` |

Sur une pull request, seul Linux tourne : une minute macOS est facturée dix fois
le tarif Linux sur un dépôt privé. macOS s'exécute à la fusion sur `main`, sur
les tags et en lancement manuel — une régression qui lui serait propre est donc
rattrapée après coup, pas avant.

Tout ce que la CI vérifie se lance en local, et c'est plus rapide que d'attendre
un runner :

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Et si le changement touche le chemin chaud — le parseur, l'agrégation — le banc
dit ce qu'il en coûte :

```bash
cargo run --release --bin bench
```

La CI en exécute une version réduite comme garde-fou. Son plancher est
volontairement très bas : les runners GitHub sont trop variables pour un seuil
serré, et l'accident qu'on veut attraper est un facteur dix, pas dix pour cent.

## Du code qu'on relira

- **Les commentaires expliquent le pourquoi**, pas le quoi. Le code dit déjà ce
  qu'il fait ; ce qu'il ne dit pas, c'est pourquoi cette borne, pourquoi ce
  plafond, pourquoi ce compromis. Le projet est écrit ainsi de bout en bout.
- **Un comportement corrigé vient avec son test.** Sans lui, rien n'empêche la
  régression de revenir.
- **La mémoire reste bornée.** Toute table indexée par une clé venue des logs a
  un plafond (`src/stats.rs`) : c'est ce qui permet d'avaler 40 Go sans bouger.
- **Un seul thread touche à l'état.** La concurrence passe par le canal `mpsc`,
  jamais par un verrou.

## Refaire la démo du README

Le GIF de tête vieillit à chaque évolution de l'interface. Il se refait en une
commande, à partir d'un scénario versionné :

```bash
brew install asciinema agg gifsicle   # expect est déjà là sur macOS
cargo build --release
./docs/demo.sh                        # écrit docs/demo.gif
```

`docs/demo.exp` décrit la séquence de touches, `docs/demo.sh` prépare les logs
et fabrique le GIF. Le corpus est engendré à graine fixe : deux prises donnent
les mêmes chiffres à l'écran, et un diff ne reflète que ce qui a vraiment changé.

Le ticket d'origine prévoyait `vhs`, plus courant pour cet usage. Il a été
écarté après essai : vhs capture ses images depuis les couches canvas de
xterm.js, servi par ttyd et piloté par un Chrome headless, et avec les versions
actuelles de ces trois-là il ne capture plus rien — dossier d'images vide, GIF
absent, sans un message d'erreur. La chaîne retenue n'a besoin d'aucun
navigateur.

Le GIF pèse quelques centaines de kilooctets et vit dans l'historique git pour
toujours : si le scénario s'allonge, vérifier son poids avant de committer.

## Publier une version

**La version de `Cargo.toml` commande.** Publier, c'est la monter dans une pull
request comme n'importe quel autre changement ; la fusion fait le reste — tag,
compilation des trois cibles, release, empreintes.

```bash
git switch -c version-0.4.0 main
# monter `version` dans Cargo.toml, puis répercuter dans Cargo.lock
cargo update --workspace
git commit -am "Version 0.4.0"
```

Il n'y a **pas de tag à poser** : `gh release create` le crée lui-même sur le
commit de fusion. Le geste manuel qui pouvait être oublié a disparu, et avec lui
la dérive entre ce que le binaire annonce et ce qui est publié.

Trois garde-fous, chacun sur un mode de défaillance réel :

| Ce qui pourrait arriver | Ce qui l'attrape |
| --- | --- |
| Monter la version sans mettre à jour `Cargo.lock` | `cargo build --locked` en CI |
| Faire reculer la version sous la dernière release | le job **Version** de la CI, sur la pull request |
| Poser un tag qui ne correspond pas à `Cargo.toml` | le job **Version à publier**, avant toute compilation |

Une fusion qui ne touche pas à la version ne publie rien : le workflow constate
que le tag existe déjà et s'arrête sans compiler. Un tag posé à la main reste
accepté — pour republier — mais il doit correspondre à `Cargo.toml`.

Le job de release est aussi lançable à la main (`workflow_dispatch`) : il compile
les trois cibles et dépose les binaires en artefacts, sans rien publier. De quoi
éprouver la matrice sans engager une version.
