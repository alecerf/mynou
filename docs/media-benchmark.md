# Benchmark de l'analyse des médias du 30 octobre 2026

La référence est **Mynou v0.28.0, `190faea`**. Elle est comparée à **ffprobe
9.0.2** (build statique arm64). `media::analyze` (`src/media.rs`) ne décode
rien : il lit seulement les métadonnées du conteneur et saute les données. Il
s'exécute à chaque import, sur chaque fichier candidat d'un téléchargement
terminé (`src/engine.rs`), sur les releases de référence (`src/library.rs`) et
sur les imports partagés (`src/library/groups.rs`). Suivi : #387.

## Méthode

- Apple M1 Pro, 16 Go, SSD interne, macOS 27.2 arm64, Rust 1.99.1, build
  release avec LTO. Aucune dépendance Cargo ajoutée. Outils, corpus et
  compilation sous `/tmp`.
- `examples/media_bench.rs` appelle `media::analyze` sur chaque fichier :
  médiane et p95 sur 10 passes, nombre d'appels `read` envoyés au système,
  octets lus, et « sauts » (lectures qui ne commencent pas là où la précédente
  s'est arrêtée). Ces compteurs viennent d'un adaptateur placé sous le
  `BufReader` de 16 Kio d'`Input`. ffprobe (`-show_format -show_streams -of
  json`) est lancé autant de fois ; ses octets lus et ses seeks viennent de sa
  propre ligne `Statistics` en `-v debug`. Un lancement de `ffprobe -version`
  seul coûte 28 ms, donc environ la moitié de ses temps vient du démarrage du
  processus.
- **À chaud** : fichiers déjà dans le cache. **À froid** : sans `sudo purge`,
  les fichiers sont copiés dans une image disque APFS (`hdiutil`), démontée
  puis remontée avant chaque lecture, pour Mynou et pour ffprobe séparément.
  Trois essais par fichier ; l'écart entre essais reste sous 3 % pour Mynou.
- Le bench compare aussi les résultats : durée (±1 s), nombre de flux, codec,
  taille d'image, fréquence d'images (±0,5 %), fréquence d'échantillonnage,
  canaux et langue audio.

### Corpus

Le corpus synthétique est recopié en boucle, sans réencodage, à partir d'un clip
de 60 s : H.264 1080p 23,976 i/s à 4,5 Mbit/s, AAC stéreo `eng` et AC-3 5.1
`fre`, et deux sous-titres pour MKV (SRT) et MP4 (`mov_text`). Il tient dans
environ 38 Mo/min, soit 4,4 Gio pour 2 h, la taille d'un film 1080p courant.

| Fichier | Contenu |
| --- | --- |
| `clip-1m.{mkv,mp4,avi,webm}` | 1 min ; WebM en VP9 + Opus 720p |
| `episode-45m.{mkv,mp4}` | 45 min, 1,6 Gio |
| `movie-2h.mkv`, `movie-2h-cues-front.mkv` | 2 h ; index (Cues) à la fin ou au début |
| `movie-2h-faststart.mp4`, `movie-2h-moov-end.mp4` | 2 h ; `moov` au début ou à la fin |
| `movie-2h.avi` | 2 h, OpenDML (plus de 1 Gio), H.264 + AC-3 |
| `Sintel.2010.1080p.mkv` | Film Blender (CC BY), 1,1 Gio, multiplexé par mkvmerge |
| `tears_of_steel_720p.mov` | Film Blender (CC BY), 355 Mio |

## Résultats à chaud

| Fichier | Mynou médiane | Lectures | Lu | ffprobe médiane | ffprobe lu |
| --- | ---: | ---: | ---: | ---: | ---: |
| clip-1m.mkv | 0,06 ms | 27 | 417 Kio | 48,9 ms | 421 Kio |
| clip-1m.mp4 | 0,04 ms | 12 | 192 Kio | 49,7 ms | 487 Kio |
| clip-1m.webm | 0,04 ms | 16 | 240 Kio | 28,2 ms | 64 Kio |
| clip-1m.avi | 0,03 ms | 3 | 48 Kio | 50,4 ms | 2,2 Mio |
| episode-45m.mkv | 2,49 ms | 1 126 | 17,6 Mio | 49,0 ms | 421 Kio |
| episode-45m.mp4 | 0,12 ms | 39 | 592 Kio | 60,9 ms | 3,3 Mio |
| movie-2h.mkv | 7,06 ms | 2 997 | 46,8 Mio | 49,0 ms | 421 Kio |
| movie-2h-cues-front.mkv | 7,16 ms | 2 998 | 46,8 Mio | 49,9 ms | 516 Kio |
| movie-2h-faststart.mp4 | **échec** | 13 | 208 Kio | 86,3 ms | 10,5 Mio |
| movie-2h-moov-end.mp4 | **échec** | 14 | 208 Kio | 85,9 ms | 10,6 Mio |
| movie-2h.avi | 0,02 ms | 3 | 48 Kio | 170,8 ms | 59,4 Mio |
| Sintel.2010.1080p.mkv | 1,57 ms | 791 | 12,3 Mio | 25,3 ms | 32 Kio |
| tears_of_steel_720p.mov | 0,03 ms | 15 | 208 Kio | 26,7 ms | 649 Kio |

À chaud, Mynou est 7 à 8 000 fois plus rapide que ffprobe, surtout parce qu'il
ne lance pas de processus et ne lit pas de paquets pour estimer les flux. Le
coût CPU n'est pas un sujet : moins de 10 ms dans le pire cas.

## Résultats à froid

| Fichier | Mynou | ffprobe |
| --- | ---: | ---: |
| clip-1m.mkv | 6,8 ms | 76 ms |
| episode-45m.mp4 | 2,8 ms | 89 ms |
| episode-45m.mkv | **212 ms** | 82 ms |
| movie-2h.mkv | **556 ms** | 83 ms |
| Sintel.2010.1080p.mkv | **312 ms** | 54 ms |

À froid, chaque lecture dispersée coûte environ 0,19 ms sur ce SSD (0,39 ms
pour Sintel). Pour un MKV, le temps suit donc le nombre de Clusters, c'est-à-dire
la durée du film. Sur un disque dur ou un partage réseau, où un accès aléatoire
coûte de 1 à 10 ms, les 2 997 lectures d'un film de 2 h donneraient de l'ordre
de **3 à 30 s par fichier**. C'est une estimation, ce support n'a pas été mesuré.
ffprobe, lui, lit 421 Kio d'un seul tenant.

## Problèmes trouvés

1. **MKV : chaque Cluster est visité.** `ebml::parse` parcourt tous les
   éléments de premier niveau du Segment jusqu'à sa fin, donc l'en-tête de
   chaque Cluster (un toutes les 1 à 3 s dans ce corpus). Chaque en-tête tombe hors du tampon
   de 16 Kio et déclenche une lecture de 16 Kio : 46,8 Mio lus pour 12 octets
   utiles par Cluster. Les métadonnées utiles (Info, Tracks, Tags) précèdent les
   Clusters ou sont désignées par le SeekHead, que le code suit déjà. Chaque
   Cluster compte aussi dans `MAX_ELEMENTS` (100 000) : un MKV d'environ 28 h
   à un Cluster par seconde serait refusé.
2. **MP4 : un film de 2 h est refusé** (`Invalid or oversized MP4 timing
   table`). La piste vidéo de `movie-2h-*.mp4` a 100 856 entrées `stts`, au-delà
   de `MAX_ELEMENTS`. C'est le cas courant d'un MP4 refait à partir d'un MKV : les
   horodatages à la milliseconde donnent des durées d'images irrégulières, soit
   environ 0,58 entrée par image (37 821 entrées pour l'épisode de 45 min). Une
   vidéo à fréquence variable, d'un téléphone par exemple, est dans le même cas.
   L'import du film échoue alors que ffprobe le lit. La lecture se fait déjà par
   blocs de 512 entrées, et la taille de la table est déjà vérifiée face à celle
   de la boîte : la limite du nombre d'éléments ne protège rien ici.
3. **AVI de plus de 1 Gio : durée fausse.** `movie-2h.avi` donne 1 704 s au lieu
   de 7 200 s : `avih` ne compte que les images du premier segment RIFF
   (1 704 418), alors que `dmlh` (OpenDML) et `strh` de la piste vidéo en
   donnent 7 200 085.
4. **AVI : codec audio `wav_0xfffe`.** Un `WAVE_FORMAT_EXTENSIBLE` (AC-3 5.1 ici)
   n'est pas résolu par son sous-format ; ffprobe lit `ac3`.
5. **MOV : langue manquante.** Pour `tears_of_steel_720p.mov`, le `mdhd` porte
   un code de langue Macintosh (0 = anglais) ; Mynou ne lit que les codes ISO 639
   compactés, ffprobe donne `eng`.

Sur les fichiers restants, les résultats de Mynou et de ffprobe concordent.

## Correctifs

Les cinq problèmes sont corrigés dans la même branche, chacun couvert par un
test de `tests/media_formats.rs` qui échoue sur v0.28.0. La règle commune est
de ne jamais parcourir les données : le plancher est une lecture de 16 Kio.

1. **MKV** : une fois Info et Tracks lues ou indexées par un SeekHead, le
   parcours du Segment s'arrête au premier autre élément (Cluster, Cues,
   pièces jointes) ; le SeekHead mène au reste, y compris par un second SeekHead
   écrit après les Clusters. Sans SeekHead, le parcours reste complet.
2. **MP4** : `stts` n'est plus limitée par `MAX_ELEMENTS`. La fréquence d'images
   vient d'une courte table `stts`, déjà dans le tampon après `stsd`, sinon du
   nombre d'images de `stsz` et de la durée de `mdhd` ; la longue table n'est
   lue que si les deux manquent. Les boîtes sont lues une à une au lieu d'en
   collecter les en-têtes d'abord, ce qui évitait de sauter deux fois chaque
   piste ; la lecture s'arrête après `moov`, et les pistes autres que vidéo et
   audio ne sont pas ouvertes.
3. **AVI** : le nombre total d'images vient de `dmlh` (OpenDML) quand il existe,
   sinon d'`avih`. La lecture s'arrête à `movi` une fois `hdrl` lu, au lieu
   d'aller lire l'en-tête d'`idx1` 1 Gio plus loin puis de revenir.
4. **AVI** : un `WAVE_FORMAT_EXTENSIBLE` est résolu par son sous-format.
5. **MOV** : un code de langue Macintosh de `mdhd` (inférieur à 0x400) est
   traduit en code ISO 639-2, comme le fait ffprobe.

| Fichier, à froid | v0.28.0 | Corrigé | ffprobe |
| --- | --- | --- | --- |
| movie-2h.mkv | 556 ms, 2 997 lectures | **0,71 ms, 1 lecture** | 83 ms |
| episode-45m.mkv | 212 ms, 1 126 lectures | 0,72 ms, 1 lecture | 82 ms |
| Sintel.2010.1080p.mkv | 312 ms, 791 lectures | 0,79 ms, 1 lecture | 54 ms |
| movie-2h-cues-front.mkv, à chaud | 2 998 lectures | 1 lecture | — |
| movie-2h-faststart.mp4 | échec | 2,3 ms, 8 lectures | 107 ms |
| movie-2h-moov-end.mp4 | échec | 3,0 ms, 9 lectures | 109 ms |
| episode-45m.mp4 | 2,8 ms, 39 lectures | 2,7 ms, 8 lectures | 89 ms |
| movie-2h.avi | durée fausse, 3 lectures | 0,79 ms, 1 lecture | 206 ms |
| tears_of_steel_720p.mov | langue absente, 15 lectures | 1,3 ms, 4 lectures | 43 ms |

Un `moov` de 256 Kio au plus est lu d'un seul tenant (`PRELOAD`) : le clip
d'une minute passe de 3 lectures dont 2 sauts à 2 lectures contiguës. Lire tout
`moov` d'un coup a aussi été essayé à froid, avec un seuil de 1 puis de 16 Mio.
Sur ce SSD c'est plus lent : 5,5 à 7,3 ms au lieu de 2,5 à 3,2 ms pour le film
de 2 h, et 1,6 ms au lieu de 1,3 ms pour les 584 Kio de Tears of Steel. Un saut
y coûte à peu près autant que 256 Kio lus d'affilée.

À l'import (`import_source` dans `src/engine.rs`), les fichiers d'un
téléchargement sont triés par taille sans être ouverts. Mynou analyse un seul
fichier pour un film, et seulement le fichier nommé pour l'épisode dans un pack
de saison. Un fichier illisible, comme un échantillon tronqué, est ignoré au lieu
de faire échouer l'import.

Mynou et ffprobe concordent sur les 13 fichiers du corpus, et la sortie JSON
complète d'`analyze` des autres fichiers est inchangée. Les lectures qui restent
pour un MP4 passent d'une piste à la suivante dans `moov` ; les éviter
demanderait de lire tout `moov`, 10 Mio pour un film de 2 h, comme le fait
ffprobe.

## Reproduire

```sh
cargo run --release --example media_bench -- /private/tmp/mynou-media-bench/corpus
FFPROBE=/chemin/vers/ffprobe JSON=/private/tmp/media.jsonl \
  cargo run --release --example media_bench -- /private/tmp/mynou-media-bench/corpus
ONCE=1 cargo run --release --example media_bench -- /Volumes/image/movie-2h.mkv
```

Le corpus synthétique se recrée avec ffmpeg : `testsrc2` plus `noise`, encodé
une fois en clip de 60 s, puis `-stream_loop -1 -c copy -t <durée>` pour chaque
conteneur (`-movflags +faststart` pour `moov` au début, `-reserve_index_space`
pour les Cues au début, `-bsf:v h264_mp4toannexb` pour l'AVI).
