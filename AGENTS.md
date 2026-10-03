# Mynou — règle de construction

- L'instruction utilisateur courante impose Rust et sa bibliothèque standard
  exclusivement : aucune dépendance Cargo, de développement ou de construction.
- Aucun code tiers copié, FFI, bloc unsafe, exécutable externe ou repli vers
  l'ancienne implémentation Go. La chaîne Rust est un outil de construction.
- Écrire des parseurs bornés, des erreurs explicites, des données persistantes
  vérifiées et des effets de bord limités. Les fonctions réseau utilisent std.
- Les tests automatisés réseau utilisent des pairs locaux et des médias synthétiques.
  Les lectures HTTPS de sites officiels sont permises pour diagnostiquer et
  vérifier la chaîne TLS ; aucun téléchargement torrent public dans les tests.
- Ne jamais annoncer une fonction non implémentée ni une performance non mesurée.
- Les données des versions Go restent préservées séparément et ne sont pas un
  format implicitement pris en charge par la nouvelle persistance Rust.
- Vérifier cargo metadata hors ligne (le seul package doit être mynou),
  cargo fmt, cargo clippy, cargo test et cargo build --release --offline.
- Conserver les diagnostics et les guides en français.
