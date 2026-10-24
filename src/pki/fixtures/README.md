These certificates are local test data created for Mynou on October 3, 2026.
They are not part of the application's trusted root store. Their private key is
intentionally public and is only available under `cfg(test)` for simulated TLS
exchanges. Never use these keys on a real server.

The application and its tests use these fixtures without invoking external tools.

The `tmdb-*` files are the public certificate chain `api.themoviedb.org` served
on October 24, 2026: its leaf, the Amazon RSA 2048 M04 intermediate and Amazon
Root CA 1 cross-signed by Starfield Services Root G2. `amazon-root-ca-1.pem`
and `starfield-services-root-g2.pem` are those two public roots, copied from
the macOS trust store. They contain no private key.
