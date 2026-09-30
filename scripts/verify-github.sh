#!/usr/bin/env bash
# Spec M's manual check against a real GitHub App on a scratch repository.
# Not part of any CI tier: it needs an App, its key, a repository the App
# is installed on, and a listener GitHub can reach.
#
# Required environment:
#   GITHUB_APP_ID          the App's numeric id
#   GITHUB_APP_KEY         path to the App's private key (PEM)
#   GITHUB_WEBHOOK_SECRET  the webhook secret configured on the App
#   GITHUB_REPO            owner/name of a scratch repository the App is installed on
#   GITHUB_LISTEN          host:port the plugin listens on (default 127.0.0.1:8787);
#                          front it with a tunnel and point the App's webhook URL at
#                          https://<tunnel>/webhook
#
# What it does: starts nothing; writes the daemon config under
# target/tmp/verify-github and prints what to do and look for.
set -euo pipefail
cd "$(dirname "$0")/.."

for var in GITHUB_APP_ID GITHUB_APP_KEY GITHUB_WEBHOOK_SECRET GITHUB_REPO; do
  if [[ -z "${!var:-}" ]]; then
    echo "verify-github: $var is not set" >&2
    exit 1
  fi
done
listen="${GITHUB_LISTEN:-127.0.0.1:8787}"

root="$PWD/target/tmp/verify-github"
data="$PWD/target/tmp/verify-data"   # shared with verify-matrix-local: tool installs only
target="${CARGO_TARGET_DIR:-target}"
case "$target" in /*) ;; *) target="$PWD/$target" ;; esac
balerix="$target/debug/balerix"
# The daemon's XDG_CONFIG_HOME moves under $root, and with it where
# `HostPaths` looks for gh's hosts.yml; without this the crew's
# `ensure-crew` fails with "git.auth is gh but no gh token was provided".
gh_dir="${GH_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/gh}"
rm -rf "$root"
saved_umask="$(umask)"
umask 077
mkdir -p "$root/config/balerix" "$root/secrets"
cp "$GITHUB_APP_KEY" "$root/secrets/github-app.pem"
printf '%s' "$GITHUB_WEBHOOK_SECRET" > "$root/secrets/github-webhook"
chmod 600 "$root/secrets/github-app.pem" "$root/secrets/github-webhook"

(umask "$saved_umask"; mise run package-plugins github)   # the packaged plugin keeps normal modes

cat > "$root/config/balerix/plugins.yaml" <<YAML
plugins:
  - name: github
    source: $PWD/target/plugins/github
    secrets:
      privateKey: $root/secrets/github-app.pem
      webhookSecret: $root/secrets/github-webhook
    config:
      appId: $GITHUB_APP_ID
      listen: "$listen"
      idleTimeout: 3m
YAML

cat <<NOTES
verify-github: config written. Now, by hand:

  1. Push a .balerix.yaml to $GITHUB_REPO's default branch:

       apiVersion: balerix/v1
       kind: Fleet
       crews:
         repo:
           repo: $GITHUB_REPO

  2. Build the daemon (\`mise x -- cargo build -p balerix\`) and start it,
     with a logged-in \`claude\` in your real HOME (the agents use its
     credentials):

       env XDG_CONFIG_HOME=$root/config XDG_STATE_HOME=$root/state XDG_DATA_HOME=$data GH_CONFIG_DIR=$gh_dir HOME=$HOME $balerix serve

     GH_CONFIG_DIR keeps your gh login visible to the daemon now that
     XDG_CONFIG_HOME points elsewhere. In another shell with the same
     XDG_* variables (never a shell you run \`gh\` in: it would look for
     its login under them), confirm
     \`$balerix plugin list\` shows github ready, and that the App's
     webhook URL reaches $listen (GitHub's "Recent Deliveries" shows a 200
     on the ping).

Then check, on $GITHUB_REPO:

  - open an issue and comment "@<app slug> please summarise this issue":
    the comment gets eyes, a status comment appears and turns "ready", the
    first prompt reaches Claude (the status line "session started"), the
    comment gains +1 when Claude takes it (the first Enter is lost while
    Claude starts and the plugin presses it again every 5 s, #99; a
    "not confirmed after 90s" line means none of them was taken), and
    Claude's turn appears as a comment;
  - a further comment from you (write permission) reaches the agent with
    eyes then +1; a comment from an account without write does nothing;
  - ask the agent to use AskUserQuestion: the question posts as a comment,
    a comment answering with a number gets +1 and an "answering" echo, and
    the echo gains hooray when Claude records it;
  - open a pull request from a branch of this repository and mention the
    App in its body: the agent runs on the PR's head branch (\`balerix
    status\` shows the branch); submit a review with two inline comments:
    the status comment says "review from @you delivered" and the agent's
    next turn shows it read them;
  - close the issue: the agent is removed (\`balerix status gh-…\` no longer
    lists it), the status comment says "closed"; comment again: confused
    and "this session has ended";
  - wait past the three-minute idleTimeout on the PR session: "stopped
    after 3m idle"; mention the App again on the PR: the session resumes
    and the first prompt carries the "Earlier work" line;
  - edit .balerix.yaml on the default branch to set \`defaults: { sandbox:
    { extends: none } }\` and mention the App on a new issue: the refusal
    "defaults.sandbox: not allowed in a plugin-applied fleet file; the
    host's default applies" is posted with confused, and nothing starts;
    revert the edit;
  - restart the daemon: the sessions survive (a comment on the open issue
    still reaches its agent).

When done, stop the daemon and delete $root/secrets (the App key and the
webhook secret): rm -rf $root/secrets
NOTES
