# ruru

[![CI](https://github.com/slashfan/ruru/actions/workflows/ci.yml/badge.svg)](https://github.com/slashfan/ruru/actions/workflows/ci.yml)

Analyseur de logs **Symfony / Monolog** en temps réel, dans le terminal.

Il suit un ou plusieurs fichiers de log à la manière de `tail -f`, les analyse au
vol et affiche un tableau de bord : erreurs regroupées par type, endpoints les
plus lents, pics de trafic.

Sur une machine de développement : **≈ 700 000 lignes/s** (133 Mo analysés en
0,97 s), pour quelques dizaines de mégaoctets de mémoire. Celle-ci est
**plafonnée par construction** — échantillon glissant pour les quantiles, tampon
circulaire pour l'axe du temps, et un plafond sur chaque table (routes,
signatures d'erreur, formes SQL, requêtes en cours). Elle varie donc avec ce que
contiennent les logs, jamais avec la taille du fichier : 10 Mo ou 40 Go, c'est le
même ordre de grandeur.

## Installation

Des binaires sont publiés à chaque version :
[Releases](https://github.com/slashfan/ruru/releases).

```bash
# Linux x86_64 — statique (musl), aucune dépendance système : il démarre aussi
# sur un serveur à la glibc ancienne, là où un binaire classique refuserait.
curl -sSL https://github.com/slashfan/ruru/releases/latest/download/ruru-linux-x86_64.tar.gz | tar xz
```

```bash
# macOS Apple Silicon (ruru-macos-x86_64.tar.gz pour les Mac Intel)
curl -sSL https://github.com/slashfan/ruru/releases/latest/download/ruru-macos-arm64.tar.gz | tar xz
```

Les binaires macOS ne sont pas signés. Récupérés par `curl` ils s'exécutent sans
histoire ; téléchargés depuis un navigateur, il faut lever la mise en quarantaine
avec `xattr -d com.apple.quarantine ruru`.

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

## Formats reconnus

La détection se fait **ligne par ligne**, sans option à passer :

- le format ligne de Symfony (`LineFormatter`) :
  `[2026-09-09T10:23:45.123456+02:00] request.CRITICAL: … {"exception":"…"} []`
- le format JSON (`JsonFormatter`), un objet par ligne.

Les lignes qui ne commencent ni par `[` ni par `{` — typiquement une stack trace
sur plusieurs lignes — sont rattachées à l'entrée précédente au lieu d'être
comptées comme du bruit.

Les erreurs sont regroupées par **signature** : la classe d'exception suivie du
message normalisé (chiffres remplacés par `#`, chaînes entre guillemets par
`"…"`). « Product 42 not found » et « Product 1337 not found » comptent donc pour
une seule et même erreur.

## Mesurer les durées

Monolog n'écrit **aucune durée** par défaut. ruru sait s'en procurer de deux
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

ruru repère seul les clés usuelles : `duration_ms`, `duration`, `elapsed_ms`,
`elapsed`, `response_time`, `execution_time`, `exec_time`, `runtime`,
`request_time`. Pour une clé maison : `--duration-key temps_total`.

L'unité est déduite du suffixe (`_ms`, `_s`, `_us`), puis de l'ordre de grandeur
— un flottant sous 30 est interprété comme des secondes, parce que `microtime()`
en donne. En cas de doute : `--duration-unit ms`.

### 2. Par corrélation de token (sans rien changer au code)

Si chaque ligne porte un identifiant de requête, ruru mesure l'écart entre la
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

## Sortie JSON (monitoring)

`--json` remplace le tableau de bord par un objet JSON, pour brancher ruru sur
une chaîne de métriques.

Un relevé ponctuel, typiquement en cron ou en CI :

```bash
ruru --json var/log/prod.log > /var/lib/metrics/ruru.json
```

Un flux continu, un objet par ligne (NDJSON), sans jamais relire le fichier
depuis le début :

```bash
ruru --json --every 30 var/log/prod.log
```

Les compteurs de `totals` et `levels` sont **cumulés** depuis le lancement, à la
manière d'un compteur Prometheus : c'est au collecteur de faire les différences
d'un relevé à l'autre. `throughput` fournit en plus des débits sur fenêtre
glissante, exploitables sans garder d'état.

`--top N` limite les listes `errors` et `endpoints` — 25 par défaut, `0` pour
tout sortir.

Le code de sortie vaut **1** si une source n'a pas pu être lue : un cron ou un
job de CI échoue franchement au lieu de laisser passer un instantané à zéro.

<details>
<summary>Structure d'un instantané</summary>

```json
{
  "generated_at": "2026-09-09T00:52:11.482913+02:00",
  "window": { "first_seen": "…", "last_seen": "…", "span_seconds": 12.418 },
  "totals": { "entries": 4600, "skipped": 0, "errors": 58, "error_rate": 0.0126 },
  "levels": { "debug": 2826, "info": 1600, "warning": 46, "critical": 58, "…": 0 },
  "throughput": {
    "peak_per_second": 907,
    "peak_at": "2026-09-09T00:52:05+02:00",
    "last_5s_per_second": 517.0,
    "last_60s_per_second": 76.7
  },
  "duration_source": { "kind": "field", "key": "duration_ms" },
  "open_requests": 12,
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

</details>

## Détecter les N+1

Un N+1, c'est la même requête SQL exécutée des dizaines de fois au sein d'une
seule requête HTTP — la boucle qui recharge une entité liée à chaque itération.
Le profiler Symfony le montre en développement ; en production, personne ne le
voit passer.

ruru s'appuie sur un fait commode : **Doctrine journalise des requêtes
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

Puis on donne les deux fichiers à ruru, qui les fusionne :

```bash
ruru var/log/prod.log var/log/doctrine.log
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
ruru [OPTIONS] <FICHIER>...

  <FICHIER>...              fichiers à suivre ; « - » lit l'entrée standard
  -a, --from-start          analyser depuis le début plutôt que depuis la fin
  -n, --lines <N>           relire les N dernières lignes au démarrage
  -l, --min-level <NIVEAU>  niveau initial du flux [défaut : debug]
      --summary             pas d'interface : lire jusqu'au bout puis résumer
      --json                sortie JSON au lieu du tableau de bord
      --every <SEC>         avec --json : un instantané NDJSON toutes les SEC s
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
ruru var/log/prod.log var/log/worker.log
```

Depuis une machine distante, sans rien installer là-bas :

```bash
ssh prod 'tail -f /srv/app/var/log/prod.log' | ruru -
```

## Organisation du code

| Fichier | Rôle |
| --- | --- |
| [`src/main.rs`](src/main.rs) | boucle principale, câblage des threads |
| [`src/cli.rs`](src/cli.rs) | options de ligne de commande (clap) |
| [`src/event.rs`](src/event.rs) | canal unique d'événements, threads clavier et horloge |
| [`src/tail.rs`](src/tail.rs) | suivi de fichiers : rotation, troncature, ligne incomplète |
| [`src/parser.rs`](src/parser.rs) | une ligne brute → `LogEntry` |
| [`src/stats.rs`](src/stats.rs) | agrégation : axe du temps, quantiles, corrélation |
| [`src/app.rs`](src/app.rs) | état applicatif et réaction aux touches |
| [`src/ui.rs`](src/ui.rs) | rendu ratatui |
| [`src/bin/genlogs.rs`](src/bin/genlogs.rs) | générateur de faux logs Symfony |

Le schéma d'ensemble :

```
   thread(s) tail ─┐
   thread clavier ─┼──► canal mpsc ──► boucle principale ──► ratatui
   thread horloge ─┘                    (app: décide)        (ui: dessine)
```

Un seul thread touche à l'état : aucun verrou, toute la concurrence passe par le
canal. La lecture et l'analyse tournent en parallèle du rendu.

```bash
cargo test      # 35 tests
cargo clippy --all-targets
```

29 tests unitaires couvrent le parseur, le suivi de fichier (rotation,
troncature, ligne incomplète), l'agrégation — dont la synchronisation entre
plusieurs fichiers lus en parallèle —, la détection de N+1 et le rendu, celui-ci
via le backend de test de ratatui, y compris sur un terminal minuscule, sous la
frappe d'une recherche et sous le suivi d'un endpoint. Six tests
de bout en bout ([`tests/cli.rs`](tests/cli.rs)) lancent les vrais binaires et
les branchent l'un sur l'autre : génération, analyse, tube sur l'entrée standard,
lecture des dernières lignes, validité du JSON et codes de sortie.

Toute modification passe par une pull request à la CI verte : la marche à suivre
est dans [CONTRIBUTING.md](CONTRIBUTING.md).

La CI rejoue tout ça sur **Linux et macOS** à chaque poussée, et vérifie en plus
le formatage, clippy sans avertissement, et que le binaire release démarre.

### Publier une version

Mettre à jour `version` dans `Cargo.toml`, puis poser le tag correspondant :

```bash
git tag v0.2.0 && git push origin v0.2.0
```

Le workflow compile les trois cibles avec le profil `dist` (dépouillé, LTO),
publie la release avec ses archives et leurs empreintes. Si le tag ne correspond
pas à la version de `Cargo.toml`, il échoue en vingt secondes — avant la moindre
compilation.

## Limites connues

- La détection de rotation s'appuie sur l'inode : Unix uniquement.
- Les quantiles portent sur les **1024 dernières** requêtes de chaque endpoint —
  c'est voulu, pour rester utile sur un flux vivant et borner la mémoire.
- Au-delà de 4096 routes ou signatures d'erreur distinctes, les nouvelles clés
  ne sont plus enregistrées (les compteurs déjà connus continuent). Même principe
  pour les motifs N+1 (1024) et les formes de requêtes SQL retenues (2048).
- Les requêtes SQL sont identifiées par une empreinte 64 bits plutôt que par leur
  texte, pour ne pas dupliquer celui-ci dans chaque requête en cours. Une
  collision reste théoriquement possible, mais négligeable à cette échelle.
- Une ligne datée dans le futur est ramenée à l'heure courante pour l'axe du
  temps, afin qu'une horloge décalée ne vide pas les graphes.

## Licence

[MIT](LICENSE) — © 2026 Nicolas Cabot.

L'avis de copyright accompagne les binaires publiés : chaque archive de release
contient le fichier `LICENSE`, comme la licence l'exige.
