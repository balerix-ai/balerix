# The agent image (Spec O §6.1, §13): the runtime image's contents (mise,
# git, gh, nono, tmux, `balerix`) plus `balerix-agent`. `BASE` is the
# runtime image of the same version; scripts/kind-up.sh passes a locally
# built one. The context holds the binary as `balerix-agent`.
ARG BASE=ghcr.io/balerix-ai/balerix:0.2.4
# hadolint ignore=DL3006
FROM ${BASE}
COPY --chmod=0755 balerix-agent /usr/local/bin/balerix-agent
# The pod names the command (`balerix-agent sidecar`, `balerix-agent run`,
# a Job's subcommand); tini reaps for all of them as it does for `serve`.
ENTRYPOINT ["/usr/bin/tini", "--", "balerix-agent"]
CMD ["sidecar"]
