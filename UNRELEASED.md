## Added: Tab completion

`mynou completion` prints a completion script for zsh or bash, chosen from
your login shell or with `--shell zsh` or `--shell bash`. Add
`eval "$(mynou completion --shell zsh)"` to `~/.zshrc` and Tab completes the
commands, their options, the actions of `mynou service` and the data folder.
The install script shows that line until your shell file has it.

## Changed: A cinema look

The interface moves from green to cinema red: velvet red for what you act on,
brass for the second series of charts, a warm paper by day and a dark hall by
night, and a velvet curtain while the library is still empty. Errors now lean
vermilion so they never pass for the brand red. Every page but the overview
opens with a compact header that names it, and the current page, tab or view
is a soft tint rather than a black pill. On a phone, New request is a round +
in the top bar, and the featured title wraps instead of ending in an ellipsis.

## Changed: The cat

Mynou's cat is redrawn in pixel art: a slender red cat sitting with its tail
curled, brass eyes, in the sidebar, the phone's top bar, the New request page,
the sign-in window, the tab icon and at the top of the README. It is alive
without asking for attention:

- Its eyes follow the pointer, or the field you are typing in, and come back
  to the middle once the pointer rests.
- Now and then it blinks, flicks its tail, twitches an ear or glances aside.
- A pointer that comes to rest on it gets a slow blink, a cat's way of saying
  it is content.
- A success makes it hop with its tail up; an error lays its ears back.
- While Mynou is connecting it looks left and right; when it is out of reach
  its ears go back and its tail drops.
- After 75 seconds without a move or a key it falls asleep, breathing slowly,
  and wakes at the next one.
- While a password is typed at sign-in, every cat closes its eyes.

With reduced motion turned on in the system, the cat stays still.

## Changed: Settings

Settings are quicker to find, say what changed and tell a mistake where it is.

- Each page in the list has an icon and, when it needs a look, a state: the
  VPN's tunnel (a green dot when protected), **Set up** while Plex or the TMDB
  token is missing, and the number of indexers, in red when one fails. The
  pages are grouped: what Mynou fetches, what it connects to, then this browser.
- **Search settings** above the list finds a setting on every page at once,
  word by word ("download speed", "seed"), and paints in what matched. Enter
  opens the first result, Escape empties the search, and `/` goes to it.
- The bar at the foot appears only for a real change, and goes again when you
  undo it by hand. It names the pages with changes, and each changed row wears
  a brass edge, its page a brass dot. Save shows Saving…, Applying… while
  Mynou restarts its engine, then Saved. ⌘S (Ctrl S) saves, and closing the
  tab with changes unsaved asks first.
- A value Mynou would refuse is told under its field once you leave it, and
  goes as soon as it is fixed: a number, a full path, a port, an indexer
  address, and the limits the service sets (searching a title every 1 to
  720 hours, 1 to 120 titles per hour, up to 730 days after a cinema release,
  better versions checked every 1 to 1440 minutes). Saving with one left opens
  its page, scrolls to it and gives it the focus, instead of a toast.
- A field you can type in looks like one: a well with a clear edge, set into
  the card, that turns white under the pointer and wears a red ring once
  focused. A field that cannot be changed from here, such as network access
  opened from another computer, is flat, dashed and greyed, with a no-entry
  pointer.
- Folders, the TMDB token, host names and the password take the full width,
  so a long path reads whole, in the monospace. **Connect to Plex** is the
  page's main button.
- Switches spring over, an added or removed indexer slides, and a row that
  appears with its switch slides in.
- Live updates no longer rebuild the friend list and the VPN's state every
  second: with Mynou busy, the page went from about 26 changes to the page per
  update to none when nothing moved, and a switch under the pointer stays put.

## Added: An indexer's website

Each indexer in **Settings → Indexers** can take its website, the one you
open in a browser, next to the address Mynou searches. The ↗ button by its
name then opens that site in a new tab. Mynou never requests it, and only an
`http` or `https` address is accepted (`site_url` in the settings).

## Changed: Motion

Movement is rebuilt on what Safari now does natively. The new page rises in
as one piece (from above when you go back up the menu), and the sidebar stays
still. The chosen tab, view or settings page is a chip that glides to the
next one on a spring. A new theme spreads in a circle from the button that
chose it. The overview's title drifts away as you scroll past it, and posters
rise into place as they come on screen.
Buttons sink when pressed and spring back, posters move closer under the
pointer, and toasts, sheets and changes of state use real spring curves.

Gone: the light that followed the pointer over cards, the sheen running
through progress bars, figures lifting under the pointer though they are not
buttons, the tilt of the featured poster, and the entrance of every block of
a page, which also replayed whenever a list was drawn again. The first page
of a visit still builds itself, once.

## Changed: Theme and version in Settings

The sidebar no longer shows the version, the connection state or the theme
buttons. The theme is in **Settings → Appearance**, and the version and the
connection state in **Settings → About**.

## Removed: The search bar

The search bar at the top of every page, and the ⌘K palette it opened, are
gone: what it found was already one click away, in the sidebar, the Library's
filter, **New request** or **Settings**. On a computer the top bar goes with
it and every page starts higher; while Mynou is out of reach, a small
"Connecting…" or "Offline. Reconnecting…" shows at the top of the page. On a
phone the top bar keeps the logo and the round +. New request moves to the top
of the sidebar, under the logo, and the empty overview no longer repeats it in
its cards.

## Added: A report to send with a problem

When something goes wrong, **Download a report** at the top of the Logs page (or
`GET /api/report`) downloads one file to send with the problem:
`mynou-report-YYYY-MM-DD.tar`. It holds every log file, the archive of the
last 30 days included; the Mynou version and build and the system it runs on;
and what Mynou showed of its state at that moment: status, settings, quality
profiles, VPN, update, jobs, transfers, library, series, sources, requesters
and statistics. Tokens, keys and passwords are hidden, in the state as in the
log, and `README.txt` inside sums it all up.

## Added: Thirty days of log

The log no longer forgets what goes past its 5 MiB. Each finished day is
compressed into `logs/archive/mynou-YYYY-MM-DD.log.gz` (UTC days), kept 30
days and at most 100 MiB, the oldest days making room. Mynou compresses it
itself, as it adds no dependency. The Logs page says how many days are
archived and what they take, and a report carries them.

## Changed: Live logs

The Logs page is one console that follows the log as Mynou writes it: each new
record comes in at the top within a fraction of a second and lights up
briefly, and the counts and the size the log takes on disk follow along.

- The levels, the search and the sources stay at the top of the window while
  you read down the records.
- A timeline shows when the records were written, from the oldest to now,
  each level in its colour: errors vermilion, warnings amber. Point at a bar
  to see what it holds; click it to go to its records.
- Each level says how many records it holds, errors in red, and the address
  keeps the level chosen (`#logs/warn`), for a reload or a link.
- A search field finds a word in every record kept, not only those shown, and
  marks it; `/` goes to it and Escape empties it. A list of sources, or a
  click on the source of a record, shows only that source.
- Records read like a terminal, in the monospace: figures with their unit,
  addresses, paths and quoted names stand out, each source keeps its own
  colour, and errors and warnings carry a coloured edge.
- The records fall under a heading for each day (Today, Yesterday, then the
  date), with the time to the millisecond; the full date and the UTC time show
  under the pointer.
- A record repeated several times in a row shows once, with its count; click
  the count to see each one.
- **Live** turns into **Paused** when clicked and keeps the list still. New
  records then wait above it, as they do while you read further down, behind
  a "3 new records" button; they come in when you go back up or click it.
- Each record has a copy button that copies it as the log file writes it.
- On a phone the four levels fit side by side, the count under the name, and
  each record puts its message under its time.

`GET /api/logs` also takes `q` (text to find in the message or the source)
and `source`, and tells how many records of each level match, every source,
the span of the log read, and a `timeline`: its `start`, the `step` of a bar
in seconds, and per bar the records of each level. **Look again** and
**Download** are gone, and so is `GET /api/logs/download`: the report
replaces them.

## Fixed: Copying over the local network

The copy buttons, for a file path or a log record, said the browser did not
allow copying when Mynou was opened over the local network rather than on the
Mac itself, where the browser offers no clipboard to a plain-HTTP page. They
now copy there too.

## Fixed: The empty featured card under the pointer

With nothing in the library yet, hovering the large card of the overview
replaced its gradient with a flat colour. It now keeps its look: only a
featured title, which opens when clicked, reacts to the pointer.

## Fixed: Safari errors for every menu

Safari logged a Content Security Policy error for each drop-down menu, 17 on
the Settings page alone, because it draws a menu with an inline style. Mynou
now allows inline style attributes, and still no inline style sheet or script.

## Fixed: Folders under /tmp and /var

On macOS `/tmp` and `/var` are links the system makes to `/private`. Mynou
refused any folder under them, the data folder or a manual import folder
alike, as it refuses links in a storage path; these system links are now
accepted. Mynou's own tests also run again with the temporary folder macOS
gives each user.

## Fixed: Pages opening scrolled down

Opened straight from an address such as `#jobs`, `#transfers`, `#library` or
`#logs` (a reload, a bookmark, a link), these pages started scrolled past
their title: the browser went to the list that bore the same name. Each page
now opens at its top.

## Fixed: Pages moving sideways

With scroll bars always shown, as macOS does while a mouse is plugged in,
going from a short page to one long enough to scroll, such as from
**Settings → VPN** to **Downloads**, moved the whole page left by the width
of the scroll bar. Its place is now kept on every page, so nothing moves.

## Fixed: Prefer freeleech

The **Prefer freeleech** switch in an indexer's rules was drawn with no width,
so it could neither be seen nor clicked. It now shows beside its name and
explanation, and turns on and off like the other switches.

