# Installer Mynou avec Docker

L’image finale est `scratch` : un exécutable Rust statique et un bundle PEM
d’autorités TLS. Le conteneur n’appelle aucun programme externe. Rust 1.99.0 et
son environnement Alpine interviennent uniquement pendant la construction.
La cible par défaut de l’image est `x86_64-unknown-linux-musl`.

## Installation sans Rust sur l’hôte

Depuis les sources décompressées, construis l’image puis utilise son binaire pour
préparer une installation dans un dossier nouveau :

```sh
docker build -t mynou:0.6.0 .
docker run --rm --network none \
  --user "$(id -u):$(id -g)" \
  --mount "type=bind,src=$PWD,dst=/work" \
  --workdir /work \
  mynou:0.6.0 setup-docker --dir mynou-docker
```

Si ton compte n’utilise pas UID/GID 1000, ajoute ses identifiants au `.env` généré :

```sh
printf '\nMYNOU_UID=%s\nMYNOU_GID=%s\n' "$(id -u)" "$(id -g)" >> mynou-docker/.env
```

Utilise un compte normal pour cette procédure. Si les fichiers ont été créés par
root, attribue le dossier de l’installation à l’utilisateur choisi pour le
conteneur ; son `mynou.json` privé doit aussi être lisible par cet utilisateur.

```sh
cd mynou-docker
docker compose config --quiet
docker compose up -d
docker compose logs -f mynou
docker compose exec mynou /mynou doctor --config /config/mynou.json
```

`setup-docker` refuse d’écraser un dossier existant. Il génère :

```text
mynou-docker/
├── compose.yaml
├── mynou.json
├── .env
├── data/
└── library/
    ├── movies/
    └── series/
```

Le dossier `data` contient les demandes, les torrents et les téléchargements.
La bibliothèque est montée séparément. Le système de fichiers de l’image reste
en lecture seule, les capacités Linux sont retirées et le conteneur utilise
un utilisateur sans privilèges par défaut.

## Relier ton Plex existant

Modifie `mynou.json` pour activer `plex.enabled`, définir `plex.url`, les identifiants
de sections films/séries et, si nécessaire, `plex.watchlist_url`. L’adresse
`http://host.docker.internal:32400` désigne l’hôte Docker ; si Plex est dans un
autre conteneur, tu peux plutôt utiliser un réseau Docker commun et son nom DNS.

Renseigne `MYNOU_PLEX_TOKEN` dans `.env`. Pour TMDB, active `catalog.enabled` et
fournis `MYNOU_TMDB_TOKEN` ou `MYNOU_TMDB_API_KEY`. Chaque entrée `indexers` décrit
une source `rss`, `json` ou `torznab`, son URL et le nom d’une variable contenant
sa clé API éventuelle. Mynou effectue les requêtes ; aucun gestionnaire de médias
supplémentaire n’est lancé dans cette installation.

Monte les mêmes dossiers dans Plex et Mynou. Un exemple simple : le dossier hôte
`./library` devient `/library` dans chacun des deux conteneurs ; les films se
trouvent alors dans `/library/movies` et les séries dans `/library/series`.
Les chemins de la configuration Mynou sont des chemins vus depuis son conteneur.

Après une modification des secrets ou de la configuration :

```sh
docker compose up -d --force-recreate
docker compose exec mynou /mynou sync --config /config/mynou.json
docker compose exec mynou /mynou jobs --config /config/mynou.json
```

## Ports et contrôle

L’API est publiée uniquement sur `127.0.0.1:8787`. Les endpoints `/healthz` et
`/readyz` décrivent l’état du service ; les opérations de gestion exigent le jeton
Bearer `MYNOU_API_TOKEN`. Le contrôle de santé utilise le binaire Mynou lui-même.

Le port 6881/TCP sert aux pairs BitTorrent. DHT et trackers UDP utilisent des
requêtes sortantes depuis des sockets éphémères ; aucun port UDP entrant n’est
publié. Le client DHT n’est pas un serveur DHT complet et le moteur n’implémente
pas uTP. L’accès entrant TCP dépend de ton pare-feu et de ton routeur.

Tu peux gérer les demandes avec la CLI du conteneur :

```sh
docker compose exec mynou /mynou submit --title "Film" --year 2026 \
  --url 'magnet:?xt=urn:btih:...' --config /config/mynou.json
docker compose exec mynou /mynou status --config /config/mynou.json
docker compose exec mynou /mynou events ID --config /config/mynou.json
docker compose exec mynou /mynou retry ID --config /config/mynou.json
docker compose exec mynou /mynou cancel ID --config /config/mynou.json
```

Pour une source locale, monte son dossier et utilise `--path` avec le chemin du
conteneur. Une demande annulée ne supprime pas les fichiers ni les autres demandes
qui pourraient partager le même torrent.

## Sauvegarder et mettre à jour

Conserve `mynou.json`, `.env`, `data` et les chemins de bibliothèque. Pour une
sauvegarde cohérente, arrête le service avant de copier ses données. Le journal
synchronise les transactions confirmées ; après une interruption brutale, une
écriture finale incomplète est récupérée au prochain démarrage.

L’arrêt de gestion `POST /api/shutdown`, authentifié, permet au service de terminer
ses workers. La bibliothèque standard ne fournit pas de gestionnaire Unix SIGTERM
portable dans ce projet ; un arrêt forcé du conteneur s’appuie donc sur la reprise
durable, et n’est pas présenté comme un arrêt applicatif gracieux.

Reconstruis `mynou:0.6.0` depuis les sources voulues puis recrée le conteneur.
Garde les anciennes données Go/SQLite dans un autre dossier : elles ne constituent
pas le format de persistance Rust et ne sont pas automatiquement importées.

## Autorités TLS et proxy

Les connexions HTTPS utilisent le client TLS natif. Le bundle de confiance de
l’image peut être remplacé par un fichier PEM monté en lecture seule et la variable
`MYNOU_CA_FILE`. Une autorité privée doit être ajoutée à ce bundle pour un service
interne ; la vérification des certificats ne dispose pas d’un mode de désactivation.

Les variables `HTTP_PROXY`, `HTTPS_PROXY` et `NO_PROXY` peuvent être fournies au
service lorsque ton réseau l’exige. Les commandes de contrôle local contournent
le proxy. [Les limites des protocoles](limits.md) listent les variantes acceptées.
