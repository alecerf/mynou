# Bibliothèque standard seule

Le manifeste ne déclare aucune section de dépendances, de développement ou de
construction. Le fichier `Cargo.lock` ne contient que `mynou`. Il n’existe ni
vendoring, ni sous-module contenant une bibliothèque tierce, ni liaison FFI,
ni génération qui appelle l’ancienne implémentation Go.

| Ancienne brique | Remplacement Rust |
| --- | --- |
| Moteur BitTorrent Go | `src/torrent.rs` et `src/torrent/` |
| SQLite | Journal transactionnel et snapshots `src/store.rs` |
| ffprobe | Parseurs de conteneurs `src/media.rs` et `src/media/` |
| Bibliothèques HTTP/TLS | `src/net.rs`, `src/tls.rs`, `src/pki/` |
| JSON et bencode | Parseurs bornés `src/json.rs` et `src/bencode.rs` |
| Bibliothèques de hachage | Algorithmes natifs `src/crypto/` |

Rust et Cargo sont des outils de construction. Docker et Compose servent au
déploiement ; le binaire peut aussi fonctionner directement. Les appels de
fichiers, réseau, temps et aléa passent par la bibliothèque standard et le système
d’exploitation. L’aléa de sécurité est fourni par `/dev/urandom` sur Linux ;
aucun générateur pseudo-aléatoire improvisé ne le remplace.

Le bundle PEM des autorités TLS est une donnée de confiance. L’image finale
contient uniquement ce bundle et le binaire statique. Aucun OpenSSL, ffprobe,
curl, shell, moteur SQL ou client torrent externe n’y est installé.

Pour contrôler le graphe local sans réseau :

```sh
cargo metadata --offline --locked --format-version 1
cargo tree --offline --locked
```

La première commande doit retourner un seul package avec `dependencies: []` ;
la seconde doit afficher seulement `mynou v0.6.0`. Les entrées réseau Plex, TMDB
et indexeurs sont des services que l’utilisateur configure, pas des dépendances
de compilation ou des commandes exécutées par Mynou.
