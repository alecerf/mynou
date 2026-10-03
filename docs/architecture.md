# Architecture Rust native

```text
CLI / API                 Watchlist Plex
    │                          │
    └──────── Demandes ────────┘
                   │
           Journal et snapshots
                   │
           Workers avec baux
                   │
       TMDB + RSS / JSON / Torznab
                   │
       BitTorrent natif et vérification
                   │
       Analyse des métadonnées média
                   │
          Import sans écrasement
                   │
       Scan et confirmation de Plex
```

Les composants sont indépendants de tout framework. `config` valide les champs
et résout les chemins relatifs au fichier de configuration. `integrations`
transforme les réponses réseau en demandes et sources. `engine` orchestre les
transitions sans garder le verrou du journal pendant un transfert ou un import.

`store` synchronise chaque transaction avant de la confirmer. Les enregistrements
sont chaînés et vérifiés par SHA-256 ; une fin incomplète après interruption est
récupérable, tandis qu’une corruption complète est signalée. Un verrou de fichier
empêche deux propriétaires du même journal. Les workers utilisent des baux et
renouvellent leur possession pendant une opération longue.

`torrent` conserve les identités et métadonnées vérifiées, relit les pièces après
reprise, contrôle les noms et les limites des fichiers puis expose un état prêt.
La présence des octets sur le disque ne suffit pas à déclarer un torrent prêt.
Un torrent hybride doit satisfaire les empreintes v1 et les racines v2 avant
publication définitive.

`media` cherche les métadonnées avec un lecteur tamponné et des déplacements dans
le fichier. Les blocs MP4 `mdat`, les clusters Matroska de taille connue et les
données WAV ne sont pas chargés en mémoire. Les lectures individuelles sont
limitées à 8 Mio, le volume total de métadonnées à 64 Mio et le nombre d’éléments à
100 000 ; les compteurs, tailles et limites de parents sont vérifiés.

`organizer` publie le média sans écraser un fichier existant. Il privilégie un
lien physique et passe par une copie synchronisée quand les systèmes de fichiers
diffèrent. Les sources sont conservées et les liens symboliques sont rejetés
sur les chemins d’import contrôlés.

`net`, `tls`, `pki` et `crypto` implémentent HTTP/1.1, le client TLS, la validation
X.509 et les primitives nécessaires. Les erreurs de protocole sont explicites ;
aucune négociation échouée ne supprime la validation des certificats. Le serveur
API utilise HTTP sur l’interface locale, avec un jeton Bearer. Les détails des
protocoles sont dans [les limites](limits.md).
