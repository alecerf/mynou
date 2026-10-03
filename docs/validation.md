# Validation de livraison

Les contrôles de source, de compilation et d’exécution ont réussi le 3 octobre
2026, après la correction de concurrence entre annulation d’une demande et
démarrage d’un torrent.

| Contrôle | Résultat confirmé |
| --- | --- |
| Graphe Cargo hors ligne | Un package `mynou`, zéro dépendance |
| Formatage et Clippy sur toutes les cibles, `-D warnings` | Réussis |
| Suite hors ligne | 131 tests réussis, aucun échec ni test ignoré |
| Médias | Dix tests, dont vraie démo MP4, MP4 creux >4 Gio, formats synthétiques et mutations |
| Constructions release GNU et musl | Réussies |
| Liaison musl | Aucun interpréteur ELF ni bibliothèque dynamique requise |
| Construction Docker finale | Réussie ; image `scratch`, utilisateur 1000:1000 |
| Conteneur final sans réseau externe | Magnet local, import et confirmation Plex simulée : `ready` |
| Nouvelle installation Compose | Générée depuis l’image sans Rust sur l’hôte ; santé, doctor et API vérifiés |
| Import et redémarrage Compose | Octets importés identiques ; même identifiant et état `ready` après redémarrage |

Le binaire statique Linux x86_64 inclus pèse 1 717 088 octets. L’image finale
pèse 1 898 812 octets et porte l’identifiant :

```text
sha256:b71a5d2f06c0cf2dfcfc13dceb86a73626a103cf8129616b0b94374da2c05146
```

L’inspection des couches confirme exactement deux fichiers réguliers embarqués :
`/mynou` et `/etc/ssl/certs/ca-certificates.crt`. Aucun shell, programme auxiliaire
ou bibliothèque partagée n’est présent. La démo finale a fonctionné en lecture
seule, sans capacités Linux et avec `--network none`. Les conteneurs de validation
ont été retirés après les contrôles.

Les tests couvrent des transferts réels entre pairs locaux, les magnets v1/v2,
les torrents hybrides, les preuves Merkle, la reprise, les erreurs de chemins,
les cycles de trackers, les métadonnées média et l’orchestration sur services
locaux. Ces contrôles ne constituent pas un déploiement sur le serveur personnel
de l’utilisateur ni une mesure de débit sur des torrents publics.

Pour reproduire les contrôles de source :

```sh
cargo metadata --offline --locked --format-version 1
cargo tree --all-features --target all --edges all --offline --locked
cargo fmt --all --check
cargo clippy --all-targets --offline --locked -- -D warnings
cargo test --all-targets --offline --locked
cargo build --release --offline --locked
cargo run --release --offline --locked --example benchmark -- examples/demo.mp4 10000
```

## Vérifier l’archive reçue

La livraison fournit un fichier `.sha256` pour le ZIP et un manifeste
`SHA256SUMS` pour son contenu. Vérifie le fichier de somme accompagnant le ZIP
avec `sha256sum -c fichier.sha256`, puis, depuis le dossier racine décompressé :

```sh
sha256sum -c SHA256SUMS
```

L’archive a été contrôlée pour son intégrité ZIP et son contenu comparé aux
fichiers de livraison par SHA-256. Elle exclut les données personnelles, les
secrets de configuration, les caches de compilation et les anciennes sources Go.

Les [mesures de performance](performance.md) et leurs
[résultats bruts](benchmark-results.json) consignent le benchmark local confirmé.
Le [point de reprise](reprise.md) décrit l’état de la livraison et les accès
nécessaires pour la relier à une installation personnelle.
