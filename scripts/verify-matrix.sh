#!/usr/bin/env bash
# Spec G's manual check against a real homeserver. Not part of any CI tier:
# it needs credentials and a server, so it is run by hand before a release.
#
# Required environment:
#   MATRIX_HOMESERVER  https://matrix.example.org
#   MATRIX_USER_ID     @balerix-test:example.org
#   MATRIX_PASSWORD    the bot account's password
#   MATRIX_INVITE      @you:example.org
#
# What it does: starts a daemon under target/tmp/verify-matrix with the
# matrix plugin loaded, brings up a one-crew fleet with two agents, and
# prints what to look for. Everything it writes is under target/tmp.
set -euo pipefail
cd "$(dirname "$0")/.."

for var in MATRIX_HOMESERVER MATRIX_USER_ID MATRIX_PASSWORD MATRIX_INVITE; do
  if [[ -z "${!var:-}" ]]; then
    echo "verify-matrix: $var is not set" >&2
    exit 1
  fi
done

root="$PWD/target/tmp/verify-matrix"
rm -rf "$root"
mkdir -p "$root/config/balerix" "$root/secrets"
printf '%s' "$MATRIX_PASSWORD" > "$root/secrets/matrix-password"
chmod 600 "$root/secrets/matrix-password"

mise run package-plugins

cat > "$root/config/balerix/plugins.yaml" <<YAML
plugins:
  - name: matrix
    source: $PWD/target/plugins/matrix
    secrets:
      password: $root/secrets/matrix-password
    config:
      homeserver: "$MATRIX_HOMESERVER"
      userId: "$MATRIX_USER_ID"
      invite: ["$MATRIX_INVITE"]
YAML

cat <<'NOTES'
verify-matrix: config written. Now, by hand:

  1. Start the daemon with XDG_CONFIG_HOME, XDG_STATE_HOME, XDG_DATA_HOME
     and HOME pointed under target/tmp/verify-matrix, as `mise run serve`
     does, and confirm `balerix plugin list` shows matrix ready.
  2. `balerix up` a fleet with one crew and two agents, both with an empty
     `plugins: { matrix: {} }` block.

Then check, in your Matrix client:

  - exactly one room appeared, named "balerix <fleet>/<crew>", private and
    encrypted, with you invited;
  - each agent has its own thread, rooted on a session-started message;
  - a permission prompt from an agent appears in that agent's thread;
  - replying inside a live thread reaches the agent and the reply is
    acknowledged with a reaction;
  - a message posted at room level is refused with a reaction;
  - after SessionEnd, a reply in that thread is refused with a reaction;
  - restarting the daemon replays nothing into the room.

Five more, exercised only by a real server — argued so far from
matrix-sdk's source, with no test behind them:

  - Token refresh. Leave the daemon running past your homeserver's access
    token lifetime (shorten it server-side first if it is long), or force
    a refresh by revoking just the access token while leaving the refresh
    token valid. Expect: the sync keeps working with no interruption
    visible in the room, and `target/tmp/verify-matrix/.../plugins/matrix/scratch/`
    is written to again around the same time (the plugin re-sealing its
    session). Restart the daemon afterward and confirm it logs in from the
    rotated session, not the original one.
  - An outage and reconnect. With the daemon up and a thread open, cut its
    route to the homeserver (block the host, or take the homeserver down)
    for a few minutes, then restore it. Expect: `balerix plugin list`
    keeps reporting matrix ready throughout (a transient sync failure is
    not a health failure), server.log shows retries with growing backoff
    while the outage lasts, and a reply sent from your Matrix client
    during the outage reaches the agent on its own once connectivity
    returns — no daemon or plugin restart needed.
  - A dead session. From your homeserver's account settings, revoke the
    plugin's device (the one that appears as the matrix plugin's bot
    session). Expect: after a handful of consecutive sync failures — not
    the first one, since a single rejection can be a blip rather than the
    session — `balerix plugin list` reports the matrix plugin degraded,
    with a message naming that the homeserver rejected the session and
    that it will not recover until the plugin is restarted with a session
    the homeserver accepts. The plugin must not go quietly deaf: a reply
    posted after the revocation should visibly get no response, matching
    what plugin list already said.
  - A pinned room (G-4). Make a room yourself in your Matrix client,
    invite the bot to it, and add its internal id — not its alias — to the
    plugin's config as `rooms: { "<fleet>/<crew>": "!id:example.org" }`.
    Then `balerix down` that crew, `plugin remove --purge matrix` so the
    plugin starts on a store that has never seen the room, restart, and
    `up` again. Expect: no second room appears, and the crew's threads are
    rooted in the room you made, with the bot's membership going from
    invited to joined at the first event. Nothing else in the plugin ever
    joins a room, so a pinned room left merely invited would fail every
    send at the homeserver and show only as an `errors_total{kind="send"}`
    tick; this is the only check that proves otherwise.
  - Restart after revocation, with a password still configured. Revoke the
    plugin's device as above, but leave the `secrets` block in place, and
    restart the daemon. Expect: it logs in again on its own and `plugin
    list` returns to ready. Then watch the room, because the login pins
    the same `deviceId` against a crypto store that still holds the
    revoked device's keys, and no reading of the library settles what that
    costs: check that messages the bot posts after the restart are
    readable in your client, that a reply in a thread still reaches the
    agent, and that your client does not show the bot's new messages as
    coming from an unknown or unverified device. If any of that is broken,
    `plugin remove --purge matrix` discards the store so the bot starts
    over as a new device — at the price of the old device's history
    staying unreadable to it.

Finally, confirm the password can be removed: delete the `secrets` block,
restart, and check the plugin still logs in from its cached session.
NOTES
