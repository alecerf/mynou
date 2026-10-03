# Mynou 0.6 — Rust, bibliothèque standard seule

Mynou automatise une bibliothèque multimédia : demande Plex ou soumission locale,
recherche, téléchargement torrent, vérification, import puis rafraîchissement Plex.
Cette version réécrit les composants en Rust sûr. Elle ne contient aucune dépendance
Cargo, y compris pour les tests et la construction, aucun code tiers embarqué, aucun
appel FFI, aucun bloc `unsafe` et aucun programme externe appelé à l’exécution.

Le client BitTorrent, les parseurs de médias, HTTP/TLS, les formats JSON/bencode et
le journal durable appartiennent au projet. SQLite, ffprobe, Go, qBittorrent,
Radarr et Sonarr ne sont plus nécessaires. Plex, TMDB et les sources que tu choisis
sont des intégrations réseau configurables.

## Essayer immédiatement

L’archive inclut un binaire statique **Linux x86_64** :

```sh
./bin/mynou analyze examples/demo.mp4 --json
./bin/mynou demo --dir /tmp/mynou-demo
```

Pour compiler les sources avec Rust **1.99.0** :

```sh
cargo build --release --offline --locked
./target/release/mynou analyze examples/demo.mp4 --json
./target/release/mynou demo --dir /tmp/mynou-demo
```

La démo démarre un pair torrent et des réponses Plex/indexeur locaux, télécharge
le média synthétique inclus, l’analyse, l’importe et confirme sa disponibilité.
Elle n’utilise ni compte Plex, ni source publique, ni secret personnel. Le dossier
de démo doit être nouveau.

## Installer avec Docker

L’image finale `scratch` contient le binaire statique et les données des autorités
TLS. Elle s’exécute avec l’utilisateur 1000 ; elle n’embarque aucun shell ni
bibliothèque partagée.

```sh
docker build -t mynou:0.6.0 .
./bin/mynou setup-docker --dir ./mynou-docker
cd mynou-docker
docker compose up -d
docker compose exec mynou /mynou doctor --config /config/mynou.json
```

Les fichiers de configuration, le jeton API, le dossier de données et la
bibliothèque sont générés. Sur une machine dont l’utilisateur ne porte pas
l’identifiant 1000, adapte `MYNOU_UID` et `MYNOU_GID` dans `.env` avant de lancer
Compose. [Le guide Docker](docs/deployment.md) détaille les droits, les chemins Plex
et l’installation sans Rust sur l’hôte.

## Ce qui est implémenté

- Analyse native MP4/MOV, Matroska/WebM, AVI, WAV/RF64, FLAC et MP3 : conteneur,
  titre, date, taille, durée quand elle est décrite, pistes vidéo et audio.
- Torrents v1, v2 et hybrides, magnets `btih`/`btmh`, métadonnées échangées avec les
  pairs, vérification SHA-1/Merkle SHA-256, reprise et partage des fichiers vérifiés.
- Découverte via trackers HTTP/HTTPS/UDP, DHT et PEX ; règles de confidentialité
  pour les torrents privés. Le transport des données des pairs utilise TCP.
- Demandes persistantes, déduplication, baux des workers, reprises après erreur,
  annulation, import sans écrasement et conservation des fichiers sources.
- Watchlist Plex, enrichissement TMDB, sources RSS/JSON/Torznab, rafraîchissement et
  confirmation Plex, API HTTP locale authentifiée et CLI de gestion.

Les formats et protocoles ont des limites explicites. L’analyse ne décode pas les
images ou le son et ne remplace pas les fonctions de transcodage de Plex. Les
anciens fichiers SQLite et états Go restent séparés : la nouvelle version ne les
migre pas implicitement. [Les limites](docs/limits.md) précisent les variantes
prises en charge et les conditions de reprise.

## Configuration et commandes

```sh
./target/release/mynou init --config ./mynou.json
./target/release/mynou serve --config ./mynou.json
```

`init` crée une configuration locale et un `.env` privé avec un jeton API aléatoire.
Plex et TMDB sont désactivés au départ. Configure leurs adresses, active les
intégrations voulues et fournis les secrets dans l’environnement du service.
Le jeton API peut aussi être lu dans le `.env` voisin du fichier de configuration.

```sh
mynou submit --title "Film local" --path ./film.mp4 --config ./mynou.json
mynou submit --title "Film" --year 2026 --url 'magnet:?xt=urn:btih:...' --config ./mynou.json
mynou sync --config ./mynou.json
mynou jobs --config ./mynou.json
mynou show ID --config ./mynou.json
mynou events ID --config ./mynou.json
mynou retry ID --config ./mynou.json
mynou cancel ID --config ./mynou.json
mynou status --config ./mynou.json
```

Les commandes de gestion passent par l’API si le service tourne ; sinon elles
ouvrent son journal local. Elles ne démarrent pas un second service de téléchargement.
Utilise un chemin accessible depuis le conteneur pour une demande locale en Docker.

## Vérifier et mesurer

```sh
cargo metadata --offline --locked --format-version 1
cargo fmt --all --check
cargo clippy --all-targets --offline --locked -- -D warnings
cargo test --all-targets --offline --locked
cargo build --release --offline --locked
cargo run --release --offline --locked --example benchmark
```

Le graphe Cargo doit contenir exactement un package, `mynou`, et zéro dépendance.
Les tests réseau utilisent des services locaux. Le benchmark mesure l’analyse
en cache chaud et les condensats ; [sa méthode](docs/performance.md) permet de
reproduire les valeurs sur ta machine. L’absence de dépendances et les résultats
de tests ne constituent pas une preuve de perfection du code.

[Architecture](docs/architecture.md) · [Installation Docker](docs/deployment.md) ·
[Dépendances](docs/dependencies.md) · [Formats et limites](docs/limits.md) ·
[Performances](docs/performance.md) · [Validation](docs/validation.md)
