# Benchmark torrent du 27 octobre 2026

La référence est **Mynou v0.26.0, `976510b2ae89a4e69a2dafa46aed1e8b4d6a99e9`**.
`git ls-remote origin HEAD refs/tags/v0.26.0` a confirmé que le HEAD distant
et la dernière version publiée désignent ce commit au moment de l'essai.
Les changements évalués ensuite sont locaux et ne constituent pas une release.

La [comparaison des fonctionnalités](torrent-comparison.md) couvre aussi
libtorrent/qBittorrent, Transmission et rqbit, avec leurs sources primaires.
Les débits sont mesurés contre **libtorrent 2.1.1.0** (binding Python 2.1.1)
et **rqbit 9.0.1**. L'interface qBittorrent et Transmission ne sont pas exécutées
dans cette campagne ; aucune performance mesurée ne leur est attribuée.

## Méthode

- Apple M1 Pro, 8 CPU logiques, macOS 27.2 arm64, Rust 1.99.1,
  build release avec LTO ; aucun ajout de dépendance
  Cargo. Outils, compilation, journaux et contenus synthétiques sous `/tmp`.
- Un seed libtorrent commun, un seul pair TCP connu, découverte désactivée,
  aucun tracker et aucun essaim public. Les contenus de 16 et 64 Mio répètent
  un bloc déterministe non nul de 1 Mio, avec des pièces de 1 Mio. Chaque résultat exige une vérification
  SHA-256 complète du fichier reçu après la sortie du processus.
- Le même relais TCP Python ajoute 0 ou 50 ms de propagation aller-retour à
  chaque moteur. Lecture et envoi sont découplés : le relais ne dort pas une
  fois par bloc de façon à imposer artificiellement un plafond de débit.
  Les connexions TCP terminent au relais : leurs ACK restent locaux. Il s'agit
  d'un délai applicatif aller-retour, pas d'une émulation complète du RTT TCP.
  Le relais ne simule ni pertes, ni congestion, ni variation de latence.
- Les comparaisons de débit configurent libtorrent en protocole clair
  (`--peer-encryption disabled`) ; le relais compte les poignées de main et
  refuse un essai qui en voit une autre. v0.26.0 ne parle pas MSE/PE ; le
  réglage par défaut de libtorrent et le chiffrement forcé sont mesurés à part.
- Trois répétitions, avec rotation des moteurs et ordre des scénarios permuté.
  Le débit du tableau est `taille / durée du processus`, de son lancement à
  sa sortie, donc démarrage et arrêt inclus. Les temps internes `ready_s`
  sont conservés séparément et ne servent pas au classement.
- CPU et pic RSS proviennent de `/usr/bin/time -l` sur le seul processus
  téléchargeur, sans le seed ni le relais. Le binding libtorrent inclut son
  interpréteur Python : son RSS ne représente pas le coût isolé de la librairie.
- Mynou synchronise les fichiers et son état durable avant publication de la
  complétion. Les concurrents sont vérifiés après sortie, mais cet essai
  n'établit pas des garanties identiques après coupure électrique.

Les petites tailles font ressortir les coûts fixes ; ces résultats ne sont
pas des plafonds de téléchargement de médias de plusieurs Go. Le seed est
vérifié avant mesure et ses données sont en cache ; les destinations sont
neuves. La campagne retenue est exécutée sans compilation ni autre test en
parallèle. Conserver les répétitions et leur dispersion, et éviter
d'interpréter de petits écarts sans latence comme des gains établis.

## Résultats retenus

Les [résultats JSON](torrent-benchmark-results.json) conservent les transferts,
la charge protégée avant/après, l'essai sur essaim réel, les versions et les
SHA-256 des exécutables. Les journaux détaillés restent sous
`/tmp/mynou-torrent-bench`.

Les changements ont été mesurés en deux étapes. La première (pipeline, quotas,
SHA-1, binaire `e879ccb5…`) est le tableau ci-dessous. La seconde ajoute le
l'attente raccourcie du tunnel, MSE/PE, le partage pendant le téléchargement et
rarest-first/endgame (binaire `final`, voir
[la version finale](#version-finale)). Sur ces scénarios directs, des essais
alternés entre les deux binaires ne montrent pas d'écart : quota 16 sur 16 Mio,
11,9–12,2 contre 11,8–12,2 Mio/s à 0 ms et 7,3–7,5 contre 7,3–7,4 à 50 ms.

Avant/après Mynou (étape pipeline), médianes de trois essais ; Mio/s utiles,
démarrage et arrêt inclus. « Quota 16 » signifie un plafond global de 16 Mio/s.

| Scénario | v0.26.0 | Version locale | Rapport après/avant |
| --- | ---: | ---: | ---: |
| Téléchargement 16 Mio, un pair, 0 ms, sans quota | 32,11 | 39,53 | 1,23× |
| Téléchargement 16 Mio, un pair, 50 ms, sans quota | 3,84 | 22,95 | 5,98× |
| Téléchargement 16 Mio, un pair, 0 ms, quota 16 | 9,34 | 12,25 | 1,31× |
| Téléchargement 16 Mio, un pair, 50 ms, quota 16 | 0,28 | 7,27 | 26,0× |
| Téléchargement 16 Mio, quatre pairs, 50 ms, quota 16 | 1,07 | 9,62 | 8,99× |
| Téléchargement 64 Mio, un pair, 0 ms, sans quota | 83,57 | 132,93 | 1,59× |
| Téléchargement 64 Mio, un pair, 50 ms, sans quota | 4,25 | 61,09 | 14,4× |
| Partage 64 Mio vers libtorrent, 0 ms | 68,27 | 82,88 | 1,21× |
| Partage 64 Mio vers libtorrent, 50 ms | 4,09 | 39,94 | 9,77× |

Aucun scénario direct ne régresse. Une version intermédiaire perdait environ
24 % à 0 ms avec quota ; la taille des lots limités suit désormais le délai
observé et ce cas progresse aussi. Sans latence sur 64 Mio, les intervalles se
touchent presque (77,6–93,3 contre 92,8–134,3) : le gain est probable mais
la dispersion reste forte. Les 16 Mio à 50 ms restent dominés par les coûts
fixes (connexion, métadonnées, mise en route) : le transfert dure moins
d'une seconde.

Dans le profil à quatre pairs, les quatre seeds ont chacun fourni 4 Mio à
chaque essai, avant et après. Le débit après varie de 9,50 à 9,73 Mio/s ; un
quota reste un plafond et ne garantit pas sa saturation.

Côté seed, le processus Mynou qui sert les six transferts de 64 Mio, démarrage
et vérification initiale compris, passe de 50,4 s et 2,88 s CPU à 7,7 s et
1,25 s CPU ; son pic RSS passe de 18,7 à 22,7 Mio.

### Version finale

Comparaison sur **64 Mio, un seed libtorrent commun, protocole clair, aucun
quota**, binaire final :

| Délai simulé | Téléchargeur | Mio/s médian [min ; max] | CPU médian, s | Pic RSS médian, Mio |
| --- | --- | ---: | ---: | ---: |
| 0 ms | Mynou final | 127,94 [88,41 ; 144,19] | 0,16 | 19,7 |
| 0 ms | libtorrent Python | 70,68 [56,76 ; 71,64] | 0,26 | 33,8 |
| 0 ms | rqbit | 87,20 [85,17 ; 122,37] | 0,56 | 14,3 |
| 50 ms | Mynou final | 62,36 [60,89 ; 63,90] | 0,17 | 24,8 |
| 50 ms | libtorrent Python | 40,84 [37,95 ; 47,41] | 0,25 | 33,9 |
| 50 ms | rqbit | 29,53 [29,42 ; 29,72] | 0,51 | 14,5 |

Sur ce banc, Mynou passe devant les deux autres moteurs à 50 ms, alors que
v0.26.0 y plafonnait à 4,25 Mio/s. Sans délai, les intervalles de Mynou et
rqbit se recouvrent : une campagne précédente, perturbée par un appel vidéo
sur la machine, donnait rqbit devant (130,8 contre 90,5 Mio/s) ; trois paires
alternées v0.26.0/final y donnaient 58–97 contre 88–127 Mio/s. Les chiffres à
0 ms mélangent coûts fixes, disque et relais et ne sont pas des plafonds
réseau. La mémoire est le prix de la fenêtre : jusqu'à huit pièces en vol par
pair font monter le pic RSS de Mynou à environ 25 Mio à 50 ms. Ces résultats
portent sur un seul pair rapide sans pertes ; l'[essaim réel](#essaim-réel)
complète ce banc.

### Chiffrement MSE/PE

Mynou accepte désormais les connexions entrantes chiffrées (RC4 ou clair
après échange Diffie-Hellman, en préférant le clair quand le pair le propose,
comme libtorrent), et réessaie une connexion sortante en MSE quand le pair
ferme ou réinitialise la poignée de main en clair. Partage de 64 Mio par Mynou
vers libtorrent, médianes de trois essais :

| Délai simulé | v0.26.0, libtorrent par défaut | Final, libtorrent en clair | Final, libtorrent par défaut | `ready_s` médian, clair → défaut |
| --- | ---: | ---: | ---: | ---: |
| 0 ms | 35,97 | 77,49 | 79,29 | 0,74 → 0,72 |
| 50 ms | 24,83 | 39,46 | 37,48 | 1,54 → 1,62 |

v0.26.0 refusait la connexion MSE (« Invalid incoming handshake ») et
libtorrent ne revenait au clair qu'environ une seconde plus tard. Le relais
voit maintenant une seule poignée de main MSE par essai, et la seconde perdue
disparaît. Dans l'autre sens, Mynou télécharge depuis un libtorrent par défaut
en clair du premier coup (91,8 et 58,9 Mio/s).

L'interopérabilité chiffrée a été vérifiée avec libtorrent forcé en RC4
(`--peer-encryption forced`), dans les deux sens et sur des torrents v1, v2 et
hybrides : chaque transfert vérifie ses fichiers et le relais ne voit aucune
poignée de main en clair. MSE ne protège pas le contenu : l'intérêt est
l'interopérabilité, la confidentialité restant assurée par le transport
protégé.

Les transferts v2 purs et hybrides par magnet depuis un seed libtorrent
réussissent également avec le binaire final : Mynou et libtorrent, 2 Mio
vérifiés chacun, une exécution par variante. Ce sont des contrôles
d'interopérabilité, pas des statistiques de performance v2.

## Charge protégée

Le banc [torrent-load](torrent-load.md) tourne sans mode smoke : 4 Mio par
torrent, deux propriétaires WireGuard locaux, seeds et téléchargeur natifs,
puis comparaison des fichiers. CPU/RSS incluent tout le processus de test.
v0.26.0 est mesurée trois fois (médiane) ; la version finale une fois, plus
des répétitions ciblées ci-dessous.

| Torrents | Actifs / pairs configurés | Flux autorisés | v0.26.0, Mio/s | Final, Mio/s | Pic RSS final, Mio | Flux maximum | Connexions refusées |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 2 / 4 | 64 | 4,6 | 10,0 | 16,7 | 1 | 0 |
| 8 | 2 / 4 | 64 | 14,6 | 19,3 | 23,1 | 8 | 0 |
| 16 | 4 / 8 | 64 | 18,4 | 22,8 | 31,5 | 16 | 0 |
| 16 | 16 / 4 | 64 | 20,8 | 25,6 | 50,4 | 52 | 0 |
| 16 | 16 / 4 | 16 | 10,0 | 16,0 | 31,3 | 16 | 32 |

Les 57 torrents terminent et leurs fichiers sont corrects. Les maxima restent
sous les limites configurées ; le dernier niveau réessaie les connexions
refusées et conserve sa génération. Les niveaux à plusieurs torrents sont
surtout limités par le CPU des deux propriétaires et des seeds dans le même
processus. Le premier niveau ne dure qu'environ une seconde ; cinq répétitions
alternées donnent :

| Un torrent | v0.26.0, s (CPU s) | Final, s (CPU s) |
| --- | ---: | ---: |
| 4 Mio | 0,86–1,02 (0,28–0,32) | 0,40–0,45 (0,15–0,17) |
| 16 Mio | 1,73–1,94 (0,80–0,89) | 0,73–0,77 (0,47–0,53) |

Le pic RSS à 16 torrents actifs passe de 44 à 50 Mio : plus de flux ouverts
(52 contre 43) et une fenêtre plus profonde dans le tunnel. Aucun résultat
n'est extrapolé à des centaines de transferts actifs, à de grosses pièces ou à
plusieurs heures de fonctionnement.

### Régression du tunnel : cause trouvée et corrigée

L'étape pipeline était d'abord 2,5 fois plus lente à travers le tunnel (4,4 s
au lieu de 1,7 s pour 16 Mio, un seul torrent), alors qu'elle gagnait en
direct. Deux symptômes avaient été isolés : l'en-tête et le bloc envoyés en
une écriture, et plus de 1 Mio en vol par pair (16 à 64 blocs prenaient
1,5–1,6 s, 128 et 512 blocs 4,1–4,2 s). L'étape pipeline les contournait en
écrivant l'en-tête à part et en plafonnant la fenêtre à 64 blocs sur le réseau
protégé.

La cause est dans le propriétaire du tunnel. Son thread attend les datagrammes
dans `recvfrom` jusqu'à 2 ms, et une commande ajoutée à sa file (écrire dans
un flux, lire) ne le réveillait pas. Avec une fenêtre profonde, le pair envoie
moins d'ACK ; chaque écriture de 16 Kio pouvait donc attendre la fin de cette
attente, ce qui espaçait encore les envois. Un réglage temporaire du pas à
200 µs suffisait à ramener 16 Mio à 0,62 s.

Le propriétaire n'attend plus que 200 µs tant que des commandes ou des
datagrammes sont passés dans les 50 dernières millisecondes, et garde les 2 ms
au repos ; il ne patiente pas du tout quand une commande est déjà en file. Les
flux attendent un signal du propriétaire plutôt qu'un pas fixe. 16 Mio passent
en 0,75 s, à 512 blocs, et le plafond de 64 blocs est retiré ; l'en-tête reste
écrit à part.

Une première correction réveillait le propriétaire par un datagramme vide
envoyé depuis une seconde socket de boucle locale : 0,63 s, mais 0,67 s de CPU
contre 0,47, et surtout une socket hôte de plus. Le contrat du tunnel est de
n'en ouvrir qu'une, vers l'extrémité déclarée, et
l'[observateur d'E/S hôte](host-io-observer.md) la refusait. Sur la charge
protégée, l'attente courte fait mieux à tous les niveaux (10,0, 19,3, 22,8,
25,6 et 16,0 Mio/s contre 8,8, 16,9, 20,6, 21,4 et 14,3) ; sur l'essaim réel
à travers le VPN, deux essais alternés donnent 36,8 et 51,9 Mio/s contre 37,1
et 57,1, pour 23 s CPU/Gio dans les deux cas.

## Défaut reproduit et changements locaux

Sur la version publiée, activer une limite de téléchargement quelconque
ramène le pipeline de 16 requêtes à une seule. À 50 ms de latence et avec un
plafond pourtant élevé de 16 Mio/s, les 16 Mio prennent environ 60 secondes,
contre environ 4,2 secondes sans limite. Sans limite, la fenêtre fixe de
16 blocs (256 Kio) plafonne aussi le débit à distance vers 4 Mio/s.

Téléchargement :

- La fenêtre suit le `reqq` annoncé par le pair, jusqu'à 512 blocs (8 Mio),
  en direct comme à travers le tunnel protégé ; sans annonce, ou avec une
  valeur nulle ou négative, elle reste à 16 blocs.
  Jusqu'à huit pièces peuvent être demandées d'avance et arriver dans le
  désordre. Le budget mémoire de 128 Mio compte toutes ces pièces, résultats
  en attente du coordinateur compris : le nombre de workers baisse avec de
  grosses pièces (15 au lieu de 29 pour des pièces de 4 Mio).
- Avec un quota, chaque lot est dimensionné sur deux allers-retours observés
  (4 à 100 ms) de la part courante du quota global/local, puis entièrement lu
  avant la réservation suivante. Un garde compte les lecteurs qui possèdent
  une pièce ; plusieurs pairs ne dimensionnent donc pas chacun leur lot sur
  tout le plafond, et les réservations FIFO imposent toujours le plafond réel.
  Sans quota, le remplissage reste continu, avec une réservation sans attente
  si une limite vient d'être activée.
- Le seau de jetons conserve jusqu'à 5 ms de crédit gagné, plafonnés à
  256 Kio partagés : un réveil tardif du thread ne jette plus de crédit.
- Les annonces HAVE attendent le prochain envoi de requêtes, ou la fin des
  pièces en vol. Écrire vers un seed qui vient de fermer après son dernier
  bloc autorisé provoquait un RST et effaçait des blocs reçus mais pas encore
  lus ; la fenêtre élargie rendait ce cas fréquent dans
  `a_ratio_limit_stops_seeding_but_retains_verified_files_and_can_be_cleared`
  (3 échecs sur 5 avant correction, 10 succès sur 10 après).

- Les pièces sont choisies par rareté parmi les pairs connectés, dans le
  niveau de priorité de fichier le plus urgent ; à rareté égale, l'ordre des
  fichiers est conservé et une disponibilité inconnue ne compte pas.
- Endgame : quand toutes les pièces manquantes sont attribuées, un pair inactif
  demande aussi une pièce détenue par un pair qui n'a livré aucune pièce
  depuis 2 s, au plus deux copies. La première copie vérifiée fait annuler les requêtes de
  l'autre (CANCEL), dont les blocs tardifs sont ignorés.
  `the_last_pieces_left_to_a_stalled_peer_are_fetched_from_another` termine en
  1,5 s, et échoue après 16 s quand l'endgame est désactivé. Une première
  version dupliquait aussi les pièces de pairs simplement lents et doublait
  les octets reçus dans `measured_parallel_transfers_improve_the_delayed_single_peer_baseline`.

Partage et coûts fixes :

- Un téléchargement en cours sert ses pièces déjà vérifiées quand le seeding
  est autorisé : bitfield et HAVE, débouchage des pairs intéressés, requêtes
  de blocs et de métadonnées entre les pièces demandées, avec les mêmes quotas
  d'envoi que le seeding. Un pair qui télécharge depuis Mynou n'est pas recyclé
  pour inactivité. `a_download_in_progress_serves_its_verified_pieces_and_metadata`
  fait passer un torrent par un relais lent vers un client qui ne connaît que
  ce relais.

- Le seed annonce le même `reqq` et active `TCP_NODELAY` sur les connexions
  directes entrantes. Lectures, vérifications, quotas et file de seeding
  restent appliqués à chaque bloc ; les requêtes en attente restent dans le
  tampon TCP. Une version intermédiaire envoyait l'en-tête et le bloc en une
  seule écriture ; elle a été retirée, voir
  [la régression du tunnel](#régression-trouvée-puis-corrigée-dans-le-tunnel).
- Le compresseur SHA-1 déroule ses 80 tours ; sur 14 cas différentiels contre
  `hashlib`, le débit mesuré sur 512 Mio passe d'environ 415 à 760–785 Mio/s.
- MSE/PE : échange Diffie-Hellman 768 bits (exponentiation de Montgomery sans
  branche dépendant du secret), RC4 après abandon des 1 024 premiers octets,
  recherche du torrent par son haché `req2` sur toutes les pages de l'index.
- Des métadonnées inchangées ne sont plus réécrites, et un changement de
  compteurs seul ne réécrit plus le fichier de contrôle : le catalogue les
  conserve, et le fichier reste la barrière écrite avant publication des
  commandes. Le gestionnaire et l'écoute sont réveillés à l'ajout et à
  l'arrêt au lieu d'attendre leur pas fixe.

Le test
`torrent::wire::tests::an_advertised_request_queue_keeps_thirty_two_blocks_in_flight`
échouait sur l'ancien code : le pair attendait 32 requêtes avant d'envoyer
les données, et ne recevait que les 16 premières. Il exerce trois lots
successifs, les quotas global/local et une petite file distante.
`eight_piece_window_accepts_out_of_order_replies_before_the_first_piece`
couvre huit pièces servies dans l'ordre inverse.

## Essaim réel

Le service Mynou installé, avec le profil VPN du fournisseur, a téléchargé
`ubuntu-26.04.1-desktop-amd64.iso` (6,04 Gio) depuis l'essaim public pendant
90 s, en alternant v0.26.0 et le binaire final (avec le réveil par socket,
voir [plus haut](#régression-du-tunnel--cause-trouvée-et-corrigée)), VPN avec redirection de port
puis direct. Les fichiers sont supprimés entre deux essais, qui repartent donc
de zéro. Moyenne sur 90 s, mise en route comprise :

| Mode | v0.26.0, Mio/s | Final, Mio/s | CPU s/Gio, v0.26.0 → final |
| --- | ---: | ---: | ---: |
| VPN, essai 1 | 8,9 | 44,8 | 56 → 22 |
| VPN, essai 2 | 31,1 | 39,4 | 27 → 21 |
| VPN, essai 3 | 16,9 | 40,0 | 45 → 29 |
| VPN, essai 4 | 13,5 | 47,7 | 55 → 25 |
| Direct, essai 3 | 47,8 | 50,1 | 9,6 → 7,3 |
| Direct, essai 4 | 60,4 | 66,4 | 9,4 → 7,1 |

Les essais 1 et 2 sont ceux de l'étape tunnel, avant le partage et
rarest-first ; en direct, l'ISO y terminait avant 90 s et leur moyenne n'est
pas comparable. À travers le VPN, le débit est multiplié par 1,3 à 5 selon
l'essai, avec 50 pairs connectés dans les deux cas, et le coût CPU par Gio
baisse de moitié. Le direct progresse modestement ; v0.26.0 a mis 16 s à
recevoir son premier octet dans l'essai 3. Un essaim public varie d'une minute
à l'autre : ces chiffres montrent un ordre de grandeur, pas une médiane. Bien
servi par des seeds complets, l'essaim n'a presque rien demandé à Mynou
(0,1 Mio envoyé au plus) : il ne mesure ni le partage pendant le
téléchargement ni la rareté.

## Vérification avec le fournisseur VPN

Le profil fourni dans `Downloads` a été lu via une copie privée temporaire
mode 0600, supprimée après l'essai. Le fichier original n'a pas été modifié ;
aucune clé ni copie de configuration n'est enregistrée dans le dépôt ou les
résultats.

Le transport natif de Mynou a établi une session protégée en **0,039 s**, puis
inspecté les métadonnées de
[`ubuntu-26.04.1-live-server-amd64.iso.torrent`](https://releases.ubuntu.com/26.04/ubuntu-26.04.1-live-server-amd64.iso.torrent)
par DNS/TCP/HTTPS dans le tunnel en **5,03 s**. L'identité
`e4f19cf636409b72571766c2ae89c98fc21a109d` correspond à celle calculée
indépendamment avec libtorrent depuis les métadonnées officielles. La génération
est restée protégée et identique ; après annulation, elle a refusé une nouvelle
publication. L'ISO n'a pas été téléchargée.

Ce contrôle réel vérifie la connexion fournisseur et le chemin des métadonnées.
Il ne mesure pas le débit BitTorrent via ce fournisseur. Les transferts de
données, trackers, DHT et révocations sur réseau protégé restent couverts par
les tests synthétiques avec deux propriétaires WireGuard ; ne pas confondre
ces deux niveaux de preuve.

## Validation et suites

Sur le code final, `cargo test --release` passe 1 378 tests dans 65 binaires,
dont 860 tests de bibliothèque. L'[observateur d'E/S hôte](host-io-observer.md),
l'inventaire réseau et les tests Python de la CI passent aussi. Le seul
échec, `settings::tests::updates_are_automatic_by_default_and_only_a_staged_one_installs`,
n'apparaît qu'en `--release` et passe en debug, sur v0.26.0 comme sur le code
local. Clippy sur toutes les cibles avec avertissements interdits et le
formatage passent.

Les priorités restantes de la [comparaison sourcée](torrent-comparison.md)
sont le profilage disque/reprise et la table de routage DHT de session. Le
partage pendant le téléchargement et la rareté restent à mesurer sur un
essaim sans seed complet. La mémoire avec de
grosses pièces et des résultats en attente du coordinateur mérite une mesure
distincte : le budget nominal d'allocation des workers ne garantit pas à lui
seul le RSS total. Les délais de découverte, le coût des trackers/DHT sous
charge, les essaims publics, les pertes réseau, les disques froids/lents et
les performances de reprise sur plusieurs Go ne sont pas benchmarkés ici.

## Reproduire

```sh
python3 -m venv /tmp/mynou-torrent-venv
/tmp/mynou-torrent-venv/bin/pip --cache-dir /tmp/mynou-torrent-pip-cache install libtorrent==2.1.1
CARGO_TARGET_DIR=/tmp/mynou-torrent-target TMPDIR=/private/tmp cargo build --release --examples --offline --locked
/tmp/mynou-torrent-venv/bin/python .github/scripts/torrent_compare.py \
  --mynou /tmp/mynou-torrent-target/release/examples/torrent_bench \
  --rqbit /tmp/mynou-torrent-bench/rqbit \
  --output /tmp/mynou-torrent-bench/new-run \
  --peer-encryption disabled --mib 64 --repeats 3 --rtt 0 50 --rates 0
```

Le binaire rqbit macOS universel provient de sa
[release officielle v9.0.1](https://github.com/ikatson/rqbit/releases/tag/v9.0.1).
`--rqbit` est facultatif. `--peer-encryption negotiated` garde le réglage
par défaut de libtorrent, `forced` lui impose RC4. Le dossier de sortie doit être nouveau et sous `/tmp`.
`environment.json` conserve les versions et SHA-256 des exécutables ;
`results.json`, les journaux et les mesures BSD time conservent les résultats
bruts. `--seeds 4 --engines mynou --rtt 50 --rates 16777216` reproduit le profil
à quatre pairs ; les compteurs du seed vérifient leur contribution effective.
`--engines`, `--kind v2|hybrid` et `--seed-engine mynou` permettent de
séparer les chemins ; le support des moteurs doit être vérifié pour chaque
variante. Les outils ne changent ni les routes système, ni le VPN de la machine.

Pour le contrôle fournisseur, donner à `torrent_vpn_probe` un profil privé
mode 0400/0600 sur un chemin physique, puis l'URL HTTPS d'un `.torrent`.
Ce programme utilise uniquement le transport userspace et n'importe pas de
configuration dans l'instance installée.
