## Added: Invite-only IRC announce channels

An IRC source can now have an invite command, for private trackers whose
announce channel is invite-only: the bot's nickname and a message such as
`invite {secret}`, whose key is saved like the other secrets. Once registered
and identified (SASL or NickServ), Mynou sends the message to the bot, waits up
to 10 seconds for its invitation, then joins, and does the same after every
reconnect. When the channel still refuses the join, Mynou asks the bot again
three times, waiting longer each time, then reports the source as failed with
the reason and tries again after the source's longest reconnect delay. A source
without an invite command now says that the channel needs one. The key is never
shown, logged or recorded. The command is set on the source (`"invite"`)
through `POST /api/settings`; IRC sources have no settings page yet.

## Fixed: IRC problems show on the overview

The overview looked for IRC errors where the service does not put them, so a
failing IRC source never showed there. It now does, with its reason when Mynou
knows it.
