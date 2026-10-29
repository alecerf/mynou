## Added: Use Mynou from other computers on your network

Settings, Network can let other computers on your local network open the web
interface and the API: Mynou then listens on every address (`0.0.0.0`) on the
port you choose, 80 included. It needs a password first and refuses to start on
a network address without one. Other computers sign in once and keep a session
cookie for 30 days; the Mac running Mynou never signs in. Requests must name
this Mac (its addresses, `NAME.local` or a name you add) and come from no other
site's page. Failed sign-ins are limited and logged without the password. The
password is set and removed on the Mac only, kept as a PBKDF2 hash, and a new
password signs every other computer out. See
[network access](/docs/macos.md#network-access).
