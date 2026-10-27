# Comparaison du moteur torrent

État de référence : Mynou v0.26.0 (`976510b`), sources consultées le
27 octobre 2026. Les observations du tableau décrivent cette version publiée ;
les changements locaux qui en découlent et leurs mesures figurent dans le
[compte rendu de benchmark](torrent-benchmark.md) et sont résumés plus bas. Cette note distingue
les observations dans Mynou, les fonctionnalités documentées des autres moteurs
et les pistes à mesurer. Elle ne constitue pas un classement de débit entre
clients : les chiffres publiés par leurs auteurs ne remplacent pas un essai
sur les mêmes fichiers, pairs, latences et disques.

## Périmètre et points de comparaison

Mynou possède son propre moteur Rust sans dépendances Cargo, avec son propre
transport Direct ou WireGuard. qBittorrent utilise libtorrent-rasterbar :
ce sont deux interfaces autour du même moteur pour cette comparaison
([README officiel](https://github.com/qbittorrent/qBittorrent#readme)).
Transmission est une seconde implémentation indépendante ; rqbit fournit un
comparateur Rust intégrable.

| Sujet | Mynou observé | Solution existante et conséquence |
| --- | --- | --- |
| Métadonnées et découverte | `.torrent`, magnets, metadata exchange, trackers HTTP/HTTPS/UDP, PEX ; recherche et annonces DHT IPv4/IPv6 bornées | rqbit documente également magnets, DHT, PEX et trackers UDP ; ce socle n'est pas le principal écart [R]. |
| v2/hybrides | Parsing, SHA-256, preuves Merkle et identité v1/v2 dans `metainfo.rs` et `wire.rs` | libtorrent 2.0 gère les deux identités des hybrides [L2]. Support non évalué ici pour Transmission/rqbit. |
| Fenêtre de requêtes | 16 blocs de 16 KiB au maximum par pair ; anticipation dans la pièce suivante. Une limite de débit non nulle réduit la fenêtre à un seul bloc | libtorrent adapte sa file au débit ; Transmission tient aussi compte des limites et du `reqq` distant [LS, TP]. |
| Choix des pièces | Priorité des fichiers puis ordre croissant ; claims exclusifs de pièces entières | libtorrent et Transmission prennent en compte la rareté ; libtorrent sait distribuer les blocs d'une même pièce entre pairs [LP, TW]. |
| Fin de téléchargement | Passage de deux claims à un claim par pair quand il reste peu de pièces ; aucune duplication volontaire/endgame dans ce chemin | Une dernière pièce détenue par un pair lent peut retenir la fin ; la spec décrit les requêtes dupliquées puis annulées en endgame [B3]. |
| Partage pendant le téléchargement | Les connexions de téléchargement ne servent pas les requêtes de blocs ; le chemin de seeding exige `ready && config.seed` | Le partage entre téléchargeurs est au cœur de BEP 3 ; son absence peut réduire la réciprocité sur des essaims incomplets [B3]. |
| Disque | Vérification parallèle jusqu'à huit threads ; lecture/écriture par pièce avec ouverture des fichiers ; cache d'une pièce par connexion de seed ordinaire | libtorrent 2.0 sépare le sous-système disque et s'appuie sur le cache OS ; ses paramètres bornent aussi les écritures en attente [L2, LS]. |
| Redémarrage | Les chemins de reprise relisent et vérifient les pièces existantes ; index durable, pause et politiques persistées | libtorrent propose le fast resume pour éviter une vérification complète ; Transmission conserve des fichiers `.resume` [LP, TR]. |
| Transport des pairs | TCP à travers `Network`; pas d'implémentation µTP/MSE repérée dans les modules torrent | Transmission documente µTP/TCP et chiffrement MSE ; rqbit documente µTP. Ce sont des écarts d'interopérabilité, pas une preuve de débit supérieur [TC, R]. |
| Ressources | Budget de connexions sortantes 256, budget supplémentaire des workers 128 MiB par téléchargement avec minimum d'un worker même si ses métadonnées dépassent ce budget ; limite effective par taille des pièces/métadonnées ; `max_peers` par défaut 50 | Transmission documente 200 pairs globaux et 50 par torrent ; comparer aussi les limites effectives et les threads, pas seulement les valeurs configurées [TC]. |
| Transport protégé | Réseau immuable par génération, révocation et publication contrôlées, sans repli automatique | Un remplacement de moteur doit préserver ces contrats propres à Mynou ; un simple bind d'interface ne démontre pas leur équivalence. Voir [transport](torrent-transport.md). |

Sources locales : [moteur](../src/torrent.rs),
[coordination parallèle](../src/torrent/parallel.rs),
[protocole des pairs](../src/torrent/wire.rs),
[découverte](../src/torrent/discovery.rs),
[métadonnées](../src/torrent/metainfo.rs),
[configuration](../src/config.rs).

## Améliorations prioritaires

Les points 1 à 4 et 6 sont traités par les changements locaux : lots limités
dimensionnés sur le délai observé, fenêtre bornée par le `reqq` du pair jusqu'à
512 blocs, en direct comme à travers le tunnel protégé, et huit pièces en vol ;
partage des pièces vérifiées pendant le téléchargement ; rarest-first dans
chaque niveau de priorité, puis endgame avec annulation et deux copies au plus ;
MSE/PE entrant et sortant. Mesures et compromis mémoire dans le
[compte rendu de benchmark](torrent-benchmark.md). Les points 5 et 7 restent
ouverts, et l'effet du partage et de la rareté sur un essaim sans seed complet
reste à mesurer.

1. **Mesurer la chute avec une limite de débit élevée.** Dans
   `Peer::download_piece`, une limite non nulle désactive presque tout le
   pipeline, même lorsqu'elle dépasse largement le débit disponible. À RTT
   50 ms, le modèle simplifié `fenêtre / RTT` donne 5 MiB/s pour 16 blocs,
   contre 0,3125 MiB/s pour un bloc, par pair. Ce calcul est un plafond du
   modèle et non une mesure du client. Comparer les deux modes avec un même
   seed et un délai artificiel ; conserver ensuite plusieurs crédits de blocs
   sans attendre une réservation en bloquant la lecture des réponses déjà en
   vol. Vérifier débit réel, équité entre pairs, annulation et limites globales.

2. **Adapter progressivement la fenêtre pour les pairs rapides et distants.**
   Une fenêtre bornée tenant compte du débit/RTT permettrait de dépasser les
   256 KiB en vol actuels. Respecter le `reqq` annoncé par le pair, les budgets
   mémoire et le transport protégé ; ne pas multiplier arbitrairement toutes
   les fenêtres. Transmission fournit un exemple concret d'ajustement [TP].

3. **Servir les pièces vérifiées avant la complétion du torrent.** C'est une
   amélioration fonctionnelle majeure pour les essaims sans seeds complets.
   Elle demande un état de disponibilité partagé, un traitement duplex des
   messages et le maintien des quotas, pauses, générations et garde-fous de
   cross-seed. Une mesure contre des seeds complets ne révélera pas cet écart.

4. **Ajouter rarest-first puis traitement des pairs retardataires.** Préserver
   les priorités de fichiers ; calculer la rareté parmi les pairs connectés,
   traiter explicitement les disponibilités inconnues, et renouveler les HAVE.
   Mesurer avec un pair qui possède seul une pièce rare et disparaît ensuite.
   Pour l'endgame, introduire des annulations et un plafond de doublons avant
   d'autoriser les claims concurrents.

5. **Profiler disque et reprise à grande taille.** Essayer des torrents de
   plusieurs Go, beaucoup de petits fichiers, un disque lent et 16–64 torrents.
   Évaluer ensuite un pool de fichiers borné ou une file disque séparée ;
   préserver les contrôles de confinement et de fichiers modifiés. Le fast
   resume nécessiterait une politique explicite de confiance/invalidation.

6. **Accepter le chiffrement MSE/PE.** Avec son réglage par défaut, libtorrent
   tente d'abord MSE, essuie un refus de Mynou et ne revient au protocole
   clair qu'une seconde plus tard : un téléchargement local de 64 Mio depuis
   Mynou tombe de 82,9 à 36,0 Mio/s. Les pairs qui exigent le chiffrement
   restent inaccessibles. MSE ne protège pas le contenu ; l'intérêt est
   l'interopérabilité, pas la confidentialité, que le transport protégé assure.

7. **Amortir la découverte DHT entre torrents.** Le code repart des bootstrap
   et d'un identifiant aléatoire dans chaque recherche ; il ne conserve pas une
   table de routage de session. Réutiliser des contacts validés pourrait réduire
   le coût de découverte. Invalidation obligatoire au changement de génération
   réseau ; ne pas présenter l'implémentation actuelle comme un nœud DHT complet
   à table persistante. Voir le modèle de routage de [BEP 5][B5].

## Limites des données historiques et protocole comparatif

[La mesure de charge existante](torrent-load.md) reste une preuve historique
sur boucle locale WireGuard avec seeds Mynou et petits contenus. Le code
consulté est plus récent : sauvegarde périodique toutes les 30 s, budget de
connexions 256 et configuration par défaut 50 pairs, contre respectivement
1 s, 64 et 4 dans cette ancienne campagne. Ses chiffres ne décrivent donc pas
les performances actuelles et ne comparent pas Mynou aux autres moteurs.

Pour un comparatif reproductible, utiliser des contenus synthétiques identiques,
un seed commun et les exécutables/configurations épinglés dans `/tmp`.
Séparer : transfert TCP direct, transfert protégé, découverte, vérification,
reprise et seeding. Ne pas comparer un téléchargement WireGuard à un concurrent
en TCP direct comme s'il s'agissait uniquement du coût du moteur. Mesurer
plusieurs répétitions (médiane et dispersion), octets vérifiés, débit utile,
temps jusqu'au premier bloc et à la complétion, CPU, RSS et threads. Ajouter
RTT 0/20/50/100 ms, quota nul/élevé/faible, pairs lents et corrompus,
pièces rares, ainsi qu'un redémarrage avec données complètes/partielles.

## Sources primaires

- [R] [rqbit : fonctionnalités et limites annoncées](https://github.com/ikatson/rqbit#readme).
  Ses remarques de performance sont anecdotiques et ne sont pas utilisées
  comme résultats comparatifs ici.
- [L2] [libtorrent : passage à 2.0](https://libtorrent.org/upgrade_to_2.0-ref.html).
  Les descriptions ARC/cache de certaines anciennes pages de fonctionnalités
  ne doivent pas être extrapolées au sous-système 2.0.
- [LS] [libtorrent : paramètres de file, disque et connexions](https://libtorrent.org/reference-Settings.html).
- [LP] [libtorrent : fonctionnalités, picker et reprise](https://libtorrent.org/features.html).
- [TP] [Transmission : `max_available_reqs` et contrôle des requêtes](https://github.com/transmission/transmission/blob/main/libtransmission/peer-msgs.cc).
- [TW] [Transmission : ordre des candidats du picker](https://github.com/transmission/transmission/blob/main/libtransmission/peer-mgr-wishlist.h).
- [TC] [Transmission : configuration documentée](https://github.com/transmission/transmission/blob/main/docs/Editing-Configuration-Files.md).
- [TR] [Transmission : fichiers de reprise](https://github.com/transmission/transmission/blob/main/docs/Configuration-Files.md).
- [B3] [BEP 3 : partage, pipelining et endgame](https://www.bittorrent.org/beps/bep_0003.html).
- [B5] [BEP 5 : table de routage et recherche DHT](https://www.bittorrent.org/beps/bep_0005.html).

[B5]: https://www.bittorrent.org/beps/bep_0005.html
