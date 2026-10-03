# Formats et limites explicites

## Analyse média

L’analyse lit les métadonnées du conteneur. Elle ne décode pas les images, les
échantillons audio ou les sous-titres et ne vérifie pas qu’un lecteur puisse
effectivement décoder tout le fichier. Un format inconnu, des tailles incohérentes
ou des métadonnées excessives produisent une erreur en français.

| Conteneur | Informations analysées | Limites |
| --- | --- | --- |
| MP4/MOV | `moov`, pistes, descriptions codecs, dimensions, chronologie, langues, titre/date iTunes, configuration AAC | Une boîte `moov` est nécessaire ; pas d’analyse autonome d’un fragment `moof` sans initialisation ; pas de déchiffrement DRM |
| Matroska/WebM | EBML, Info, Tracks, tags globaux, dimensions, fréquence/canaux audio, langues | Les tailles inconnues sont acceptées pour Segment et permettent l’arrêt sur un Cluster inconnu ; des métadonnées suivantes restent accessibles si SeekHead les référence ; pas de reconstruction d’une durée à partir de tous les paquets |
| AVI | RIFF `hdrl`, `avih`, `strh`, `strf`, titres INFO | Chronologie décrite par l’en-tête principal ; pas d’indexation complète OpenDML/AVIX ni décodage des trames |
| WAV/RF64 | `fmt`, `ds64`, `data`, titre/date INFO | Durée déduite du volume audio et du débit déclaré ; les variantes compressées peuvent fournir une durée indicative plutôt qu’un comptage des échantillons décodés |
| FLAC | STREAMINFO, nombre d’échantillons, commentaires Vorbis TITLE/DATE | Pas de décodage des blocs audio ni de vérification MD5 des échantillons |
| MP3/MPEG audio | Deux en-têtes de trame cohérents, ID3 v2.2–v2.4, Xing/Info/VBRI | Durée disponible lorsqu’un index la décrit ; sans index elle reste inconnue ; tags chiffrés/compressés/unsynchronisés non interprétés |

Les titres embarqués priment sur le nom de fichier. La date reste absente si
aucune métadonnée interprétable ne la décrit. Un codec inconnu conserve son
identifiant de conteneur ; une valeur absente reste `null` dans le JSON.
L’analyse est bornée à 8 Mio par lecture de métadonnées, 64 Mio au total et
100 000 éléments. Les données média volumineuses sont franchies avec `seek`.

## BitTorrent

Le moteur implémente v1, v2 et hybride, le protocole pairs TCP, l’extension de
métadonnées, les preuves de hachage v2, les trackers HTTP/HTTPS/UDP, le client DHT
et la réception PEX. Il utilise une concurrence bornée par les paramètres de
configuration. Ce moteur n’implémente pas uTP, WebRTC/WebTorrent, les webseeds,
UPnP/NAT-PMP ou une table DHT persistante complète.

Chaque transfert utilise un pair actif et jusqu’à 16 blocs en vol ; le cache
de découverte est limité à 1 024 pairs par torrent. Le partage annonce les
événements `started`, `completed` et `stopped` et respecte les intervalles du
tracker, bornés entre 30 secondes et 24 heures. Un arrêt forcé ne garantit pas
l’envoi de `stopped`. Les compteurs de téléchargement et d’envoi décrivent les
octets de contenu transférés pendant la session courante ; ils repartent de zéro
au redémarrage et ne constituent pas un ratio historique.

Un magnet ne révèle pas le drapeau privé avant l’obtention de ses métadonnées.
Lorsqu’un tracker ou `x.pe` est fourni, la découverte publique attend la
classification du torrent. Un magnet sans indication peut rechercher ses pairs
par DHT, puis arrêter la découverte publique si les métadonnées le classent privé.
Pour une acquisition privée dont la confidentialité doit être connue immédiatement,
utilise le fichier `.torrent` ou un magnet avec son tracker privé.

La reprise recontrôle les données locales ; un fichier modifié ne devient pas
prêt par simple présence. Les chemins dangereux et les métadonnées contradictoires
sont rejetés. L’annulation d’une demande ne garantit pas l’arrêt d’un torrent
qui pourrait être partagé par une autre demande.

## HTTP, TLS et intégrations

Le client implémente HTTP/1.1 avec limites de taille, délais, redirections et proxy
HTTP/CONNECT. Il ne négocie pas HTTP/2 ou HTTP/3. L’API locale accepte des requêtes
HTTP/1.1 avec longueur annoncée ; elle refuse le transfert segmenté des requêtes
et les en-têtes ambigus.

Le client TLS utilise TLS 1.3, X25519 et ChaCha20-Poly1305/SHA-256 avec validation
du certificat et du nom d’hôte. Les signatures de certificats acceptent
RSA et ECDSA P-256/P-384 avec SHA-256/SHA-384. Les algorithmes ou extensions critiques absents
de l’implémentation produisent une erreur ; il n’existe aucun repli vers une
connexion non authentifiée. Les primitives sont vérifiées par vecteurs et tests
de protocole locaux, sans prétendre à un audit cryptographique indépendant.
L’extension X.509 NameConstraints n’est pas implémentée et provoque un rejet
explicite ; le client ne consulte pas de listes CRL ni de serveur OCSP.

Plex, TMDB et les indexeurs nécessitent leurs adresses et accès valides. Les tests
de réponses locales ne signifient pas que la version a été connectée à ton
installation personnelle. La sélection de sources est bornée par les formats
et critères implémentés ; elle n’effectue pas de recherche générale sur le Web.

Les réponses TMDB sont mises en cache une heure, avec 256 entrées et 32 Mio au
maximum. Les épisodes déjà présents dans une réponse sont filtrés par leur date
au moment de la synchronisation ; de nouvelles métadonnées sont découvertes après
expiration du cache et lors du prochain tour. La confirmation de disponibilité
Plex utilise toujours une réponse réseau fraîche.
La sélection automatique exige une correspondance stricte du titre et un fichier
identifié pour l’épisode demandé. Les packs et variantes de titres ne sont pas
résolus implicitement ; les épisodes spéciaux de saison zéro sont exclus de
l’expansion automatique des séries.

## Persistance et plateforme

Le journal Rust remplace SQLite et exige un seul propriétaire du même dossier.
Les fichiers des versions Go sont conservés séparément ; aucune migration
silencieuse de schéma ou de torrent n’est réalisée. Conserve aussi la bibliothèque
et les téléchargements quand tu changes de version.

L’historique conserve les 1 000 événements les plus récents de l’ensemble du
journal, puis filtre ceux d’une demande pour `events`. Les demandes et leur état
restent dans les snapshots ; l’historique d’événements n’est pas une archive
illimitée. Une compaction automatique est tentée lorsque le journal atteint
4 Mio. Les records et snapshots sont bornés à 16 Mio, et le journal à 64 Mio ;
un dépassement produit une erreur plutôt qu’une croissance sans limite.

La cible de livraison Docker est Linux x86_64 avec musl. Le projet n’installe aucun
gestionnaire de signaux Unix via FFI : l’arrêt authentifié de l’API est gracieux,
et la reprise du journal traite les interruptions forcées. Les fonctions de
sécurité qui utilisent `/dev/urandom` exigent un système qui fournit cette source.
