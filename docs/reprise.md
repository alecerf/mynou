# Point de reprise — Mynou 0.6.0

L’instruction active impose Rust et sa bibliothèque standard seule : zéro crate,
aucun code tiers embarqué, FFI, `unsafe`, programme externe ou retour vers Go.
La réécriture remplace SQLite, ffprobe et le moteur torrent Go. Les données des
versions précédentes restent séparées et ne sont pas migrées implicitement.

Au 3 octobre 2026, les parseurs médias, le moteur torrent, la persistance,
l’orchestration, les intégrations, la CLI/API et les fichiers Docker sont écrits.
Le graphe Cargo vérifié hors ligne contient un package, `mynou`, avec
`dependencies: []`. Rust 1.99.0 et `actions/checkout` v7.0.1 ont été vérifiés sur
leurs sources officielles.

La validation finale a passé **131 tests**, sans échec ni test ignoré, le
formatage et Clippy sur toutes les cibles sans avertissement, ainsi que les
constructions release GNU et musl. Elle inclut la régression de concurrence
entre annulation et démarrage d’un torrent. Le binaire musl inclus, de 1 717 088
octets, a été vérifié sans interpréteur ELF ni bibliothèque dynamique requise.

L’image `scratch` finale, de 1 898 812 octets, utilise UID/GID 1000:1000 et porte
l’identifiant `sha256:b71a5d2f06c0cf2dfcfc13dceb86a73626a103cf8129616b0b94374da2c05146`.
L’inspection des couches confirme seulement le binaire et le bundle CA comme
fichiers réguliers embarqués. La démo finale a confirmé un magnet local jusqu’à
l’import et à l’état `ready`, avec Plex simulé, `--network none`, racine en
lecture seule et capacités Linux retirées.

Une nouvelle installation générée depuis l’image, sans Rust sur l’hôte, a été
vérifiée avec Compose : santé, doctor, API, import aux octets identiques, puis
même identifiant de demande et état `ready` après redémarrage. Les conteneurs
de test ont été retirés ; aucun service Docker de validation n’est laissé actif.

Le benchmark release confirmé utilise 10 000 analyses de la démo MP4 en cache
chaud : 8,98 µs/analyse, environ 111 333 analyses/s, SHA-256 177,14 Mio/s et SHA-1
250,74 Mio/s sur un environnement partagé avec deux processeurs logiques. Les
conditions de mesure sont dans [performance.md](performance.md).

## Livraison terminée

L’archive `mynou-v0.6.0-rust-std.zip` contient les sources, les tests, les guides,
les fichiers Docker, le média de démo et le binaire statique Linux x86_64.
Son contenu a été comparé aux fichiers de livraison et ses sommes SHA-256
vérifiées. Un fichier `.sha256` accompagne le ZIP ; le manifeste `SHA256SUMS`
permet de contrôler chaque fichier après extraction.
[validation.md](validation.md) décrit la procédure de vérification après réception.

Les logs de session sont conservés sous `/workspace/scratch/mynou-rust-*`.
Les preuves finales de session incluent `mynou-rust-final-tests.log`,
`mynou-rust-final-docker-runtime.json` et `mynou-rust-final-benchmark.json`.
Les résultats du benchmark sont aussi inclus dans `docs/benchmark-results.json`.

Le branchement sur le Plex personnel reste à configurer : adresses, sections,
chemins partagés, jeton Plex et accès TMDB/sources. Les tests sur services locaux
ne sont pas annoncés comme un déploiement sur cette installation.

Aucun outil disponible ne donne accès à la consommation du quota ChatGPT du
compte. Ce fichier permet une reprise explicite ; aucune surveillance automatique
du quota ni relance après sa réinitialisation n’est annoncée.
