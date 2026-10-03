# Mesurer les performances

Le projet fournit un benchmark std-only, sans framework de mesure :

```sh
cargo run --release --offline --locked --example benchmark
cargo run --release --offline --locked --example benchmark -- examples/demo.mp4 10000
```

Il renvoie un objet JSON contenant le nombre d’itérations, la durée, le temps moyen
par analyse, les analyses par seconde et les débits SHA-1/SHA-256 en Mio/s.
`std::hint::black_box` conserve le travail mesuré. Le média est analysé 32 fois
avant la mesure : les résultats médias concernent donc un **cache chaud**.
Les hachages utilisent un bloc déterministe de 1 Mio, répété 32 fois chacun.

Le profil release utilise LTO thin et une seule unité de génération. Les nombres
observés dépendent du processeur, de la charge concurrente, du stockage et du
compilateur. Ils ne décrivent pas le débit d’un torrent public, la latence Plex,
un disque froid ou toutes les tailles de métadonnées possibles.

L’analyse des conteneurs saute leurs blocs audio/vidéo avec `seek` plutôt que de
copier le média en mémoire. Un test crée un MP4 creux avec un bloc `mdat` supérieur
à 4 Gio et vérifie l’extraction des métadonnées finales. Ce test vérifie la
stratégie de lecture ; il ne constitue pas un benchmark de décodage.

Les résultats de la validation de livraison sont consignés dans
[le point de reprise](reprise.md). Relance le benchmark sur ta machine avant de
dimensionner la concurrence. Aucune qualification de « parfait » ou de performance
universelle n’est déduite d’un seul environnement de mesure.

## Mesure locale du 3 octobre 2026

Sur le binaire exemple release de Mynou 0.6.0, compilé avec Rust 1.99.0,
environnement Linux x86_64 partagé annonçant deux processeurs logiques :

| Mesure | Résultat |
| --- | --- |
| Fichier analysé | Démo MP4 incluse, 125 671 octets |
| Analyses en cache chaud | 10 000 |
| Durée des analyses | 0,08982 s |
| Temps moyen par analyse | 8,98 µs |
| Analyses par seconde | Environ 111 333 |
| SHA-256, 32 × 1 Mio | 177,14 Mio/s |
| SHA-1, 32 × 1 Mio | 250,74 Mio/s |

Cette mesure porte sur la démo et les fonctions de hachage, pas sur la
performance de bout en bout d’une bibliothèque réelle. Le petit intervalle mesuré
rend aussi les variations de charge visibles ; il ne justifie pas de comparaison
générale avec un autre logiciel.

Le JSON exact de cette mesure est inclus dans
[benchmark-results.json](benchmark-results.json).
