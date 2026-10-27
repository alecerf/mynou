## Changed: A web interface that looks like a media application

The web interface is redesigned around five views: an overview of what is on
its way, the library of films and series, jobs, transfers and settings. It
follows a dark ink and lemon palette that turns to paper and olive by day, and
shows typographic covers for titles. It is still plain HTML, CSS and JavaScript
embedded in the executable, with no build step, remote asset or framework.
Every wait shows: busy buttons spin, missing data shows a skeleton, searching
or importing jobs show a progress bar, and reduced motion keeps the states
without the movement. The JSON settings editor is gone; optional settings such
as `selection` and `requesters` are saved with `mynou settings --save` (#370).

## Added: Find a title on TMDB and choose a release by hand

**New request** searches the TMDB catalog and shows its posters
(`GET /api/catalog`). Adding a film lets Mynou search the indexers by itself,
and adding a series follows it. From a job, or from any episode of a followed
series, **Choose a release** lists what the indexers offer with why each
release was kept or rejected, and downloads the one you pick (`POST /api/grab`,
`POST /api/series/ID/search` and `/grab`;
[details](/docs/selection.md#choose-a-release-by-hand), #370).

## Added: TLS 1.2 for trackers and indexers without TLS 1.3

Mynou now reaches HTTPS servers that offer only TLS 1.2: ECDHE with X25519 or
P-256, AES-128-GCM or ChaCha20-Poly1305, secure renegotiation required and the
extended master secret whenever the server offers it. There is no session
resumption or renegotiation, a TLS 1.3 server's downgrade sentinel is refused,
and the certificate and host name are validated as with TLS 1.3
([details](/docs/limits.md)).
