# refrain

[![CI](https://github.com/slashfan/refrain/actions/workflows/ci.yml/badge.svg)](https://github.com/slashfan/refrain/actions/workflows/ci.yml)

*[English version](README.md) — cette page est la version française.*

Analyseur de logs **Symfony / Monolog** en temps réel, dans le terminal.

Il suit un ou plusieurs fichiers de log à la manière de `tail -f`, les analyse au
vol et affiche un tableau de bord : erreurs regroupées par type, endpoints les
plus lents, pics de trafic.

Vos logs ont un refrain : la même erreur, la même requête SQL, encore et encore.
C'est ce qu'il cherche.

> **L'interface est en anglais** — onglets, résumés, messages — parce que
> l'écosystème Symfony l'est. Cette documentation existe dans les deux langues,
> et les commentaires du code sont en français.

![refrain : le tableau de bord, le suivi d'un endpoint, ses motifs N+1 et la recherche dans le flux](docs/demo.gif)

Sur une machine de développement : **≈ 1,7 million de lignes/s** — 237 Mo
analysés en 0,69 s — pour quelques dizaines de mégaoctets de mémoire. Celle-ci est
**plafonnée par construction** — histogramme à erreur bornée pour les quantiles,
tampon circulaire pour l'axe du temps, et un plafond sur chaque table (routes,
signatures d'erreur, formes SQL, requêtes en cours). Elle varie donc avec ce que
contiennent les logs, jamais avec la taille du fichier : 10 Mo ou 40 Go, c'est le
même ordre de grandeur. **Chacun de ces plafonds est couvert par un test** : la
table cesse de s'étendre, sans jamais cesser de compter ce qu'elle connaît déjà.

Ce chiffre se rejoue plutôt qu'il ne se croit :

```bash
cargo run --release --bin bench
```

```
corpus    : 1 208 100 lignes, 237.6 Mo — /tmp/refrain-bench-100000-g1.log
parseur   :    2 545 641 lignes/s   (475 ms)
+ agrégat :    1 666 726 lignes/s   (725 ms)
```

Le banc engendre son corpus avec `genlogs` à graine fixe, puis mesure deux
choses : l'analyse d'une ligne seule, puis l'analyse **et** l'agrégation. La
lecture du fichier est hors chronomètre — c'est le processeur qu'on mesure, pas
le disque. Le chiffre de tête, lui, est celui du binaire réel, lecture comprise :
il dépasse la mesure mono-thread parce que l'analyse tourne dans le thread de
lecture pendant que l'agrégation tourne dans le thread principal.

Mesuré sur Apple M5 Pro, rustc 1.98.1, profil `release`. Sur une autre machine
les chiffres changeront ; la méthode, non.

La démo ci-dessus se refait de la même façon — `./docs/demo.sh` — à partir d'un
scénario versionné. Elle n'est donc pas condamnée à se périmer à la première
évolution de l'interface.

## Installation

Des binaires sont publiés à chaque version :
[Releases](https://github.com/slashfan/refrain/releases).

```bash
# Linux x86_64 — statique (musl), aucune dépendance système : il démarre aussi
# sur un serveur à la glibc ancienne, là où un binaire classique refuserait.
curl -sSL https://github.com/slashfan/refrain/releases/latest/download/refrain-linux-x86_64.tar.gz | tar xz
```

```bash
# macOS Apple Silicon (refrain-macos-x86_64.tar.gz pour les Mac Intel)
curl -sSL https://github.com/slashfan/refrain/releases/latest/download/refrain-macos-arm64.tar.gz | tar xz
```

Les binaires macOS ne sont pas signés. Récupérés par `curl` ils s'exécutent sans
histoire ; téléchargés depuis un navigateur, il faut lever la mise en quarantaine
avec `xattr -d com.apple.quarantine refrain`.

Chaque release porte un fichier `SHA256SUMS`, vérifiable par `shasum -c`.

## Compiler soi-même

Rust 1.88 ou plus récent.

```bash
cargo build --release
```

Sans logs sous la main, le binaire `genlogs` en fabrique de réalistes :

```bash
cargo run --release --bin genlogs -- --rate 300 var/log/prod.log
```

`--spread 200` date les requêtes sur les deux cents dernières secondes au lieu
de toutes les écrire à l'instant : de quoi remplir les graphes, et de quoi
essayer `--since`.

Et dans un autre terminal :

```bash
cargo run --release -- var/log/prod.log
```

Un résumé texte, sans interface, pour un cron ou une CI :

```bash
cargo run --release -- --summary var/log/prod.log
```

`-n` le restreint à la fin du fichier, sans relire les quarante gigaoctets qui
précèdent :

```bash
cargo run --release -- --summary -n 100000 var/log/prod.log
```

## Les cinq onglets

| Onglet | Ce qu'on y voit |
| --- | --- |
| **Vue d'ensemble** | volume et erreurs par seconde (sparklines), répartition par niveau, canaux les plus bavards, top erreurs |
| **Erreurs** | erreurs regroupées par signature, avec le détail du dernier exemplaire (exception, endpoint, contexte JSON) |
| **Endpoints** | requêtes, p50, p95, max, requêtes SQL par requête et taux d'erreur par route |
| **SQL** | motifs N+1 : la même requête SQL répétée au sein d'une seule requête HTTP |
| **Flux** | les dernières entrées, filtrables par niveau, par motif et par endpoint |

### Raccourcis

| Touche | Effet |
| --- | --- |
| `q` | quitter |
| `Échap` | lever le filtre en cours ; sinon quitter |
| `Tab`, `←` `→`, `1`–`5` | changer d'onglet |
| `↑` `↓`, `j` `k` | naviguer · `Page↑` `Page↓` par 10 · `g` / `G` début / fin |
| `Entrée` | suivre l'endpoint sélectionné (onglets Endpoints et SQL) |
| `/` | chercher dans le flux · `Entrée` valide · `Échap` efface |
| `espace` | figer ou reprendre le flux |
| `s` | changer le tri des endpoints (p95 → max → requêtes → erreurs) |
| `+` / `-` | relever / abaisser le niveau minimum du flux |
| `w` / `y` | extraire la sélection : fichier / presse-papier |
| `r` | remettre les compteurs à zéro |
| `?` | aide |

`/` cherche dans le **message**, le **canal** et la **route** à la fois, sans
tenir compte de la casse : `doctrine` isole les requêtes SQL, `app_login` tout ce
qui touche à cet endpoint, `Connection refused` l'incident lui-même. Le motif
s'affiche dans le bandeau de l'onglet tant qu'il est actif, pour qu'un filtre
oublié ne laisse jamais croire que les logs se sont taris.

### Suivre un endpoint

`Entrée` sur une ligne de l'onglet **Endpoints** — ou d'un motif N+1 dans
l'onglet **SQL** — met cet endpoint sous surveillance : les onglets Erreurs, SQL
et Flux ne montrent plus que ce qui le concerne. Le tableau des endpoints, lui,
garde tout le monde, puisque c'est là qu'on choisit ; celui qu'on suit y est
marqué d'un `▸`, et rappelé dans le bandeau du haut depuis n'importe quel onglet.

C'est le trajet habituel d'un diagnostic : un p95 qui dérape dans Endpoints, ses
N+1 dans SQL, ses erreurs dans Erreurs, ses lignes brutes dans Flux — sans jamais
retaper de filtre.

Le rattachement va plus loin que le texte des lignes. Une requête SQL de Doctrine
ne nomme aucune route, une exception non capturée non plus ; c'est leur token
partagé avec la ligne `Matched route` qui les relie, et le flux s'en souvient.
Suivre `app_orders` fait donc remonter ses requêtes SQL, que rien dans leur texte
ne rattachait à lui. Sans token de corrélation, seules les lignes portant
elles-mêmes une route sont retenues.

`Entrée` à nouveau sur le même endpoint relâche le suivi, `Échap` aussi. Échap
défait d'ailleurs les filtres l'un après l'autre — le motif de recherche d'abord,
puis l'endpoint suivi — et ne quitte que lorsqu'il ne reste rien à défaire.

### Extraire ce qu'on a trouvé

Une fois l'erreur tenue, on veut la coller dans un ticket. `w` écrit la sélection
dans un fichier du répertoire courant, `y` la met dans le presse-papier :

```
refrain-erreur-ProductNotFound-20260909-231205.txt
refrain-endpoint-api_orders_list-20260909-231240.txt
refrain-nplus1-api_orders_list-20260909-231302.txt
```

Le rapport se suffit à lui-même : ce qu'on regardait, quand, depuis quels
fichiers, puis le détail. Pour une erreur, c'est **la trace d'exécution
entière** — l'écran n'en montre que les trois premières lignes — avec son
contexte JSON. Pour un endpoint, ses quantiles et les motifs N+1 qui expliquent
le plus souvent son p95. Depuis la vue d'ensemble ou le flux, où il n'y a rien de
sélectionné, c'est le résumé complet.

`y` passe par la séquence OSC 52 : c'est le **terminal** qu'on charge de la
copie, donc le presse-papier de la machine devant laquelle on est assis, pas
celui du serveur où tourne refrain. C'est le seul moyen qui traverse un `ssh`, et il
n'ajoute aucune dépendance. Tous les terminaux ne l'honorent pas — Terminal.app
l'ignore, tmux le veut avec `set -g set-clipboard on` — d'où `w`, qui ne dépend
de personne.

## Formats reconnus

La détection se fait **ligne par ligne**, sans option à passer :

- le format ligne de Symfony (`LineFormatter`) :
  `[2026-09-09T10:23:45.123456+02:00] request.CRITICAL: … {"exception":"…"} []`
- le format JSON (`JsonFormatter`), un objet par ligne.

Les lignes qui ne commencent ni par `[` ni par `{` — typiquement une stack trace
sur plusieurs lignes — sont rattachées à l'entrée précédente au lieu d'être
comptées comme du bruit.

Un log n'est pas toujours de l'UTF-8 valide : un octet latin-1 venu d'une
bibliothèque ancienne, un blob binaire dans un message d'exception, un caractère
coupé en deux par une rotation. Les lignes sont lues en octets et converties sans
jamais échouer — un octet fautif ne coûte que le caractère qu'il occupe, jamais
le reste du fichier.

Les erreurs sont regroupées par **signature** : la classe d'exception suivie du
message normalisé (chiffres remplacés par `#`, chaînes entre guillemets par
`"…"`). « Product 42 not found » et « Product 1337 not found » comptent donc pour
une seule et même erreur.

## Mesurer les durées

Monolog n'écrit **aucune durée** par défaut. refrain sait s'en procurer de deux
façons ; l'onglet Endpoints affiche toujours laquelle est en usage.

### 1. Un champ de durée dans le contexte (recommandé)

Le plus fiable. Un abonné sur `kernel.terminate` suffit :

```php
// src/EventSubscriber/RequestDurationSubscriber.php
namespace App\EventSubscriber;

use Psr\Log\LoggerInterface;
use Symfony\Component\EventDispatcher\EventSubscriberInterface;
use Symfony\Component\HttpKernel\Event\RequestEvent;
use Symfony\Component\HttpKernel\Event\TerminateEvent;
use Symfony\Component\HttpKernel\KernelEvents;

final class RequestDurationSubscriber implements EventSubscriberInterface
{
    private float $start = 0.0;

    public function __construct(private readonly LoggerInterface $logger) {}

    public static function getSubscribedEvents(): array
    {
        return [
            KernelEvents::REQUEST => ['onRequest', 4096],
            KernelEvents::TERMINATE => 'onTerminate',
        ];
    }

    public function onRequest(RequestEvent $event): void
    {
        if ($event->isMainRequest()) {
            $this->start = microtime(true);
        }
    }

    public function onTerminate(TerminateEvent $event): void
    {
        $this->logger->info('Request finished', [
            'route' => $event->getRequest()->attributes->get('_route'),
            'method' => $event->getRequest()->getMethod(),
            'status' => $event->getResponse()->getStatusCode(),
            'duration_ms' => round((microtime(true) - $this->start) * 1000, 1),
        ]);
    }
}
```

refrain repère seul les clés usuelles : `duration_ms`, `duration`, `elapsed_ms`,
`elapsed`, `response_time`, `execution_time`, `exec_time`, `runtime`,
`request_time`. Pour une clé maison : `--duration-key temps_total`.

L'unité est déduite du suffixe (`_ms`, `_s`, `_us`), puis de l'ordre de grandeur
— un flottant sous 30 est interprété comme des secondes, parce que `microtime()`
en donne. En cas de doute : `--duration-unit ms`.

### 2. Par corrélation de token (sans rien changer au code)

Si chaque ligne porte un identifiant de requête, refrain mesure l'écart entre la
première et la dernière ligne d'une même requête. Le `UidProcessor` de Monolog
suffit à l'activer :

```yaml
# config/services.yaml
services:
    Monolog\Processor\UidProcessor:
        tags: [monolog.processor]
```

Les clés reconnues d'office : `token`, `uid`, `request_id`, `x-request-id`,
`trace_id`. Sinon : `--correlate-key ma_cle`.

> **À savoir.** Cette méthode mesure du premier au dernier *log*, pas du début à
> la fin de la *requête* : rien n'étant journalisé après la dernière ligne, elle
> **sous-estime** la durée réelle. Le classement des endpoints reste juste, les
> valeurs absolues sont à prendre comme un plancher. Pour du chiffre exact,
> passez par la méthode 1.

Sans l'une ni l'autre, l'onglet Endpoints reste utilisable : les requêtes sont
comptées grâce à la ligne `Matched route` du canal `request`, et le taux
d'erreur par endpoint reste exact.

## Les journaux tournés

Dès qu'on remonte à hier — le cas même du post-mortem — le fichier s'appelle
`prod.log.1.gz`. refrain les lit tels quels, décompressés au vol, sans fichier
temporaire :

```bash
refrain --summary var/log/prod.log.2.gz var/log/prod.log.1.gz var/log/prod.log
```

La détection se fait sur **l'entête, pas sur l'extension** : un `.log` gzippé est
reconnu, un `.gz` qui n'en est pas un est lu en clair. Les archives en plusieurs
membres — ce que produit un `cat a.gz b.gz` — sont lues jusqu'au bout.

Un fichier compressé est clos par nature : il n'y a rien à suivre, ni de rotation
à guetter. Il est lu en entier puis la source se termine, pendant que les autres
continuent d'être suivies. `-n` reste honoré : la décompression complète est
inévitable, mais seules les N dernières lignes sont retenues, et la mémoire reste
bornée.

## Borner l'analyse dans le temps

En post-mortem, la question n'est jamais « les cent mille dernières lignes »,
c'est « depuis 14h30 ». `--since` et `--until` bornent ce qui est **compté** :

```bash
refrain --summary --since 15m                  var/log/prod.log
refrain --summary --since 14:30 --until 15:00  var/log/prod.log
refrain --json    --since 2026-09-09T14:30:00  var/log/prod.log
```

Les deux acceptent une durée comptée depuis le lancement (`30s`, `15m`, `2h`,
`3d`) ou une date : `2026-09-09T14:30:00+02:00` avec son fuseau,
`2026-09-09 14:30` ou `2026-09-09` sans — c'est alors celui de la machine — et
`14:30` pour aujourd'hui, ce qu'on tape en plein incident.

Une ligne hors fenêtre ne pèse **nulle part** : ni dans les totaux, ni sur l'axe
du temps, ni dans les quantiles. Sans quoi `--since 15m` rendrait un p95 calculé
sur la journée entière. Elle n'est pas comptée comme « ignorée » non plus —
`skipped` sert à repérer un problème de format, pas un filtre qui fait son
travail. Les rapports disent combien de lignes ont été écartées, ce qui évite de
prendre une fenêtre trop étroite pour une application au repos :

```
0 entrées analysées (0 ignorées), 0 erreurs
fenêtre : 23 116 lignes écartées hors bornes
```

`--since` implique de lire le fichier depuis le début — suivre depuis la fin ne
montrerait rien tant qu'une nouvelle ligne n'arrive pas. Sur un `prod.log` de
quarante gigaoctets, `-n` reste le garde-fou de coût : `--since 15m -n 100000`
ne relit que la fin du fichier, puis n'en garde que le quart d'heure demandé.

## Faire échouer un job sur un seuil

Un rapport en cron ou en CI ne sert à rien s'il faut le lire pour savoir que ça
va mal. `--fail-if` rend **3** dès qu'un seuil est franchi :

```bash
refrain --summary \
  --fail-if 'error-rate>2%' \
  --fail-if 'p95>1s' \
  --fail-if 'p95:api_orders_list>800ms' \
  var/log/prod.log
```

```
refrain: seuil franchi — error-rate = 8.60 % > 2.00 %
refrain: seuil franchi — p95 (api_orders_list) = 3.35 s > 1.00 s
```

Les seuils franchis partent sur la sortie d'erreur, un par ligne : le rapport
lui-même reste exploitable par un tube.

La grammaire est volontairement étroite — `métrique comparateur valeur` :

| | |
| --- | --- |
| **Métriques** | `error-rate`, `request-error-rate`, `errors`, `entries`, `p50`, `p95`, `p99`, `max` |
| **Comparateurs** | `>`, `>=`, `<`, `<=` |
| **Unités** | `%` pour un taux, `ms` ou `s` pour une durée ; sans unité, une durée est en millisecondes et un taux en fraction (`0.02` = `2%`) |

### Quel taux d'erreur

`error-rate` est la part des **lignes** qui sont des erreurs. Il dépend donc de
ce qu'on donne à lire à refrain — et cette page pousse à lui en donner plus :
ajoutez `doctrine.log` pour que les N+1 se détectent, et des dizaines de lignes
DEBUG par requête HTTP rejoignent le dénominateur. Sur les mêmes 400 requêtes :

```
error-rate         = 0,56 %      ← un seuil à 2 % reste silencieux
request-error-rate = 6,75 %      ← les mêmes erreurs, rapportées aux requêtes
```

`request-error-rate` rapporte ces mêmes erreurs aux requêtes HTTP — la
définition qu'emploie déjà la colonne `Err.` — et ne bouge donc pas quand on
ajoute un fichier. C'est celui qu'on garde en CI ; `error-rate` répond à une
autre question, « ce journal est-il bavard en erreurs », et reste ce qu'il a
toujours été.

Tous deux comptent des **lignes** en erreur : une requête qui en journalise
trois en pèse trois. Et quand aucune requête n'a été vue — ni « Matched route »,
ni champ de durée —, `request-error-rate` ne se prononce pas plutôt que de
rendre un zéro rassurant.

Un quantile sans endpoint porte sur **le pire de tous** : « aucune route ne doit
dépasser une seconde au p95 » est ce qu'on veut dire en CI, et le message nomme
la coupable. `p95:api_orders_list` vise une route précise ; si elle n'apparaît
pas dans les logs, le seuil ne se prononce pas plutôt que d'inventer un zéro qui
le ferait passer pour respecté.

Un seuil mal écrit est refusé **au lancement**, pas après avoir lu quarante
gigaoctets — et avec le code 2, ce qui le distingue d'un seuil réellement
franchi. `--fail-if` n'a de sens que sur un rapport qui se termine : il est
refusé avec `--every` comme dans le tableau de bord.

## Sortie JSON (monitoring)

`--json` remplace le tableau de bord par un objet JSON, pour brancher refrain
sur une chaîne de métriques.

Un relevé ponctuel, typiquement en cron ou en CI :

```bash
refrain --json var/log/prod.log > /var/lib/metrics/refrain.json
```

Un flux continu, un objet par ligne (NDJSON), sans jamais relire le fichier
depuis le début :

```bash
refrain --json --every 30 var/log/prod.log
```

Les compteurs de `totals` et `levels` sont **cumulés** depuis le lancement, à la
manière d'un compteur Prometheus : c'est au collecteur de faire les différences
d'un relevé à l'autre. `throughput` fournit en plus des débits sur fenêtre
glissante, exploitables sans garder d'état.

`--top N` limite les listes `errors` et `endpoints` — 25 par défaut, `0` pour
tout sortir.

Les codes de sortie distinguent les causes, pour qu'un job sache à quoi il a
affaire :

| Code | Cause |
| --- | --- |
| **0** | tout va bien |
| **1** | une source n'a pas pu être lue |
| **2** | la ligne de commande est fautive |
| **3** | un seuil `--fail-if` est franchi (voir plus bas) |

Le **1** évite qu'un cron laisse passer un instantané à zéro pour « tout va
bien ». Et si une source manque, c'est elle qui prime sur les seuils : des
chiffres incomplets ne permettent de rien affirmer.

Un lecteur qui s'en va n'est rien de tout cela : `| head -1`, un collecteur qui
redémarre. refrain cesse d'écrire, ne dit rien, et sort avec **0** — les
journaux ont bien été lus, il n'y a simplement plus personne à qui le dire.

<details>
<summary>Structure d'un instantané</summary>

```json
{
  "generated_at": "2026-09-09T00:52:11.482913+02:00",
  "window": { "first_seen": "…", "last_seen": "…", "span_seconds": 12.418 },
  "totals": {
    "entries": 4600, "skipped": 0, "errors": 58, "error_rate": 0.0126,
    "requests": 400, "request_error_rate": 0.145, "out_of_window": 0
  },
  "levels": { "debug": 2826, "info": 1600, "warning": 46, "critical": 58, "…": 0 },
  "throughput": {
    "peak_per_second": 907,
    "peak_at": "2026-09-09T00:52:05+02:00",
    "last_5s_per_second": 517.0,
    "last_60s_per_second": 76.7
  },
  "duration_source": { "kind": "field", "key": "duration_ms" },
  "open_requests": 12,
  "capped": [],
  "sql": { "shapes": 6, "nplus1_threshold": 10 },
  "channels": [{ "channel": "doctrine", "count": 2026, "errors": 0 }],
  "errors": [
    {
      "signature": "ConnectionLost: Uncaught PHP Exception …",
      "count": 13,
      "level": "critical",
      "channel": "request",
      "exception": "Doctrine\\DBAL\\Exception\\ConnectionLost",
      "endpoint": "app_login",
      "first_seen": "…",
      "last_seen": "…",
      "message": "Uncaught PHP Exception …"
    }
  ],
  "endpoints": [
    {
      "endpoint": "api_orders_list",
      "requests": 117,
      "errors": 10,
      "error_rate": 0.0855,
      "timed": 117,
      "p50_ms": 881.8,
      "p95_ms": 2908.7,
      "p99_ms": 3728.9,
      "max_ms": 11800.8,
      "avg_ms": 1291.88,
      "queries_avg": 29.1,
      "queries_max": 57
    }
  ],
  "nplus1": [
    {
      "endpoint": "api_orders_list",
      "sql": "SELECT t0.id, t0.email FROM customer t0 WHERE t0.id = ?",
      "requests_affected": 14,
      "max_per_request": 57,
      "avg_per_request": 30.7,
      "last_seen": "…"
    }
  ]
}
```

`duration_source.kind` vaut `field`, `correlation` ou `none` : le collecteur sait
ainsi si les latences sont exactes ou seulement un plancher (voir plus haut).

`capped` énumère les tables qui n'acceptent plus de nouvelle clé — `routes`,
`errors`, `channels`, `sql shapes`, `n+1 patterns`, `open requests`. Vide, tout
ce qui suit est complet ; un nom dedans, et la liste correspondante n'est qu'un
sous-ensemble — les compteurs qui la surplombent, eux, restent exacts.

`request_error_rate` vaut `null` quand aucune requête HTTP n'a été vue : il n'y
a rien par quoi diviser, et un zéro se lirait comme une bonne nouvelle (voir
plus haut).

`peak_per_second` est la seconde la plus chargée de **tout ce qui a été lu**, et
non d'une fenêtre récente : sur un fichier couvrant la journée, le pic de la
journée. Les débits glissants qui l'accompagnent — `last_5s_per_second`,
`last_60s_per_second` — sont ceux qui décrivent le présent. Dans le tableau de
bord, `r` remet le pic à zéro avec le reste des compteurs.

</details>

## Détecter les N+1

Un N+1, c'est la même requête SQL exécutée des dizaines de fois au sein d'une
seule requête HTTP — la boucle qui recharge une entité liée à chaque itération.
Le profiler Symfony le montre en développement ; en production, personne ne le
voit passer.

refrain s'appuie sur un fait commode : **Doctrine journalise des requêtes
préparées**, paramètres à part dans `params`. Deux exécutions d'un même N+1
produisent donc *exactement* la même chaîne — aucune normalisation SQL à écrire,
une égalité suffit.

Il faut deux choses côté Symfony :

**1. Que Doctrine journalise.** Les requêtes arrivent sur le canal `doctrine` en
niveau `DEBUG`, avec un champ `context.sql`. En production ce niveau est souvent
filtré — et c'est précisément là que les N+1 se cachent. Un handler dédié suffit
à les rendre visibles sans noyer `prod.log` :

```yaml
# config/packages/monolog.yaml
monolog:
    handlers:
        doctrine:
            type: stream
            path: '%kernel.logs_dir%/doctrine.log'
            level: debug
            channels: [doctrine]
```

Puis on donne les deux fichiers à refrain, qui les fusionne :

```bash
refrain var/log/prod.log var/log/doctrine.log
```

**2. Un token de corrélation**, pour savoir quelles lignes appartiennent à la
même requête HTTP — le `UidProcessor` de la section précédente.

Le seuil se règle avec `--nplus1 N` : une requête SQL répétée au moins N fois
dans une même requête HTTP est signalée. 10 par défaut, `0` désactive.

L'onglet **Endpoints** gagne au passage une colonne `SQL/req`, le nombre moyen de
requêtes par requête HTTP. C'est souvent le premier coupable d'un p95 qui dérape :

```
Endpoint            Requêtes  SQL/req  p50      p95      max      Err.
api_orders_list     73        29.1     912 ms   3.35 s   4.49 s   9.6%
app_search          95        2.0      230 ms   900 ms   1.08 s   5.3%
```

## Options

```
refrain [OPTIONS] <FICHIER>...

  <FICHIER>...              fichiers à suivre ; « .gz » lu tel quel, « - » lit
                            l'entrée standard
  -a, --from-start          analyser depuis le début plutôt que depuis la fin
  -n, --lines <N>           relire les N dernières lignes au démarrage
  -l, --min-level <NIVEAU>  niveau initial du flux [défaut : debug]
      --since <QUAND>       ne compter qu'à partir de là (15m, 14:30, une date)
      --until <QUAND>       ne compter que jusque-là
      --summary             pas d'interface : lire jusqu'au bout puis résumer
      --json                sortie JSON au lieu du tableau de bord
      --every <SEC>         avec --json : un instantané NDJSON toutes les SEC s
      --fail-if <SEUIL>     échouer (code 3) si le seuil est franchi ; répétable
      --top <N>             erreurs et endpoints détaillés en JSON [25 ; 0 = tous]
      --nplus1 <N>          seuil de détection N+1 [10 ; 0 désactive]
      --duration-key <CLÉ>  clé portant la durée
      --duration-unit <U>   auto | ms | s | us [défaut : auto]
      --correlate-key <CLÉ> clé identifiant une requête
      --no-correlate        désactiver la corrélation
      --correlate-timeout <SEC>  inactivité avant clôture d'une requête [5]
      --tick-ms <MS>        période de rafraîchissement [250]
      --scrollback <N>      entrées conservées dans le flux [2000]
```

Plusieurs fichiers à la fois, chacun sur son thread :

```bash
refrain var/log/prod.log var/log/worker.log
```

Depuis une machine distante, sans rien installer là-bas :

```bash
ssh prod 'tail -f /srv/app/var/log/prod.log' | refrain -
```

## Organisation du code

| Fichier | Rôle |
| --- | --- |
| [`src/lib.rs`](src/lib.rs) | la bibliothèque : tout sauf le câblage |
| [`src/main.rs`](src/main.rs) | boucle principale, câblage des threads |
| [`src/cli.rs`](src/cli.rs) | options de ligne de commande (clap) |
| [`src/event.rs`](src/event.rs) | canal unique d'événements, threads clavier et horloge |
| [`src/tail.rs`](src/tail.rs) | suivi de fichiers : rotation, troncature, ligne incomplète, gzip |
| [`src/parser.rs`](src/parser.rs) | une ligne brute → `LogEntry` |
| [`src/stats.rs`](src/stats.rs) | agrégation : axe du temps, quantiles, corrélation |
| [`src/app.rs`](src/app.rs) | état applicatif et réaction aux touches |
| [`src/ui.rs`](src/ui.rs) | rendu ratatui |
| [`src/threshold.rs`](src/threshold.rs) | seuils `--fail-if` : grammaire et verdict |
| [`src/export.rs`](src/export.rs) | extraction de la sélection : rapport, fichier, OSC 52 |
| [`src/bin/genlogs.rs`](src/bin/genlogs.rs) | générateur de faux logs Symfony |
| [`src/bin/bench.rs`](src/bin/bench.rs) | banc de mesure du débit |

Le schéma d'ensemble :

```
   thread(s) tail ─┐
   thread clavier ─┼──► canal mpsc ──► boucle principale ──► ratatui
   thread horloge ─┘                    (app: décide)        (ui: dessine)
```

Un seul thread touche à l'état : aucun verrou, toute la concurrence passe par le
canal. La lecture et l'analyse tournent en parallèle du rendu.

```bash
cargo test      # 77 tests
cargo clippy --all-targets
cargo run --release --bin bench -- --min 100000   # le garde-fou de la CI
```

66 tests unitaires couvrent le parseur, le suivi de fichier (rotation,
troncature, ligne incomplète, journal gzippé y compris en plusieurs membres,
octet UTF-8 invalide),
l'agrégation — dont chacun des plafonds mémoire et la synchronisation entre
plusieurs fichiers lus en parallèle —, la détection de N+1 et le rendu, celui-ci
via le backend de test de ratatui, y compris sur un terminal minuscule, sous la
frappe d'une recherche et sous le suivi d'un endpoint — et l'extraction, jusqu'à
l'encodage base64 de la séquence OSC 52. Le parseur est en outre éprouvé sur
dix-sept mille lignes tordues — toutes les troncatures possibles, puis des
mutations à graine fixe — dont il doit sortir sans paniquer. Onze tests
de bout en bout ([`tests/cli.rs`](tests/cli.rs)) lancent les vrais binaires et
les branchent l'un sur l'autre : génération, analyse, tube sur l'entrée standard,
tube refermé en aval, lecture des dernières lignes, fenêtre temporelle et seuils
sur des fichiers aux valeurs connues, lecture d'un journal compressé par le
`gzip` du système, étalement des logs engendrés, validité du JSON et codes de
sortie.

Toute modification passe par une pull request à la CI verte : la marche à suivre
est dans [CONTRIBUTING.md](CONTRIBUTING.md).

La CI rejoue tout ça sur **Linux et macOS** à chaque poussée, et vérifie en plus
le formatage, clippy sans avertissement, et que le binaire release démarre.

### Publier une version

La version de `Cargo.toml` commande : la monter dans une pull request suffit, la
fusion publie. Le workflow pose le tag lui-même, compile les trois cibles avec le
profil `dist` (dépouillé, LTO), et publie la release avec ses archives et leurs
empreintes.

Aucun tag à pousser à la main, donc aucune dérive possible entre ce que le
binaire annonce et ce qui est publié. Une fusion qui ne touche pas à la version
ne compile rien ; une version qui reculerait sous la dernière release fait
échouer la CI de la pull request ; et `cargo build --locked` refuse un
`Cargo.lock` resté en arrière. Voir [CONTRIBUTING.md](CONTRIBUTING.md).

## Limites connues

- **Unix uniquement**, et c'est un choix : la détection de rotation s'appuie
  sur l'inode. Sous Windows, on travaille de toute façon dans WSL — donc sous
  Linux, où tout fonctionne.
- Un fichier compressé n'est pas suivi : il est lu une fois, en entier. C'est ce
  qu'il est — un journal clos.
- Les quantiles sortent d'un histogramme et non d'un échantillon trié : ils
  portent sur **tout ce qui a été lu** — ou sur tout depuis `r` dans le tableau
  de bord — à **±1,6 %** près. Chaque octave est découpée en 32 tranches, si
  bien que cette borne vaut à 1 ms comme à 10 s ; 672 compteurs par endpoint
  couvrent 0,06 ms à 131 s pour 2,6 Ko. Le `max`, lui, n'est pas une estimation :
  il est suivi exactement.
- Au-delà de 4096 routes ou signatures d'erreur distinctes, les nouvelles clés
  ne sont plus enregistrées (les compteurs déjà connus continuent). Même principe
  pour les motifs N+1 (1024) et les formes de requêtes SQL retenues (2048). Une
  erreur dont la signature n'entre plus reste comptée dans le total : on cesse de
  détailler, jamais de compter. Et ça se dit : `capped: routes` dans le bandeau,
  une ligne `capped` dans le résumé, une liste `capped` dans le JSON. Une table
  qui a cessé de détailler rend sa propre liste partielle, et une route qui en
  serait absente se lirait sinon comme une route sans trafic.
- Les requêtes SQL sont identifiées par une empreinte 64 bits plutôt que par leur
  texte, pour ne pas dupliquer celui-ci dans chaque requête en cours. Une
  collision reste théoriquement possible, mais négligeable à cette échelle.
- Une ligne datée dans le futur est ramenée à l'heure courante pour l'axe du
  temps, afin qu'une horloge décalée ne vide pas les graphes.
- Deux lectures des mêmes fichiers rendent le même rapport, jusqu'à l'ordre des
  lignes : les égalités sont départagées par le nom, jamais laissées à la table
  de hachage.

## Licence

[MIT](LICENSE) — © 2026 Nicolas Cabot.

L'avis de copyright accompagne les binaires publiés : chaque archive de release
contient le fichier `LICENSE`, comme la licence l'exige.
