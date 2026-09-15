# The balerix runtime image (Spec I §6.2): the static `balerix` plus every
# tool `serve` looks for on PATH. Built by scripts/release/image-context.sh's
# context: `balerix` (a built binary) and the repository's mise.toml.
FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS base

FROM base AS tools
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG TARGETARCH
ARG MISE_VERSION=2026.9.2
ARG MISE_SHA256_AMD64=9b75416600bb52ef12c54bc64d85f2f756c4d8980a981a7878ada273f56acda9
ARG MISE_SHA256_ARM64=df427c304c48c10323fe9ed5ec99e7f39bdc01d1842e304cd3f15ea5a6049320
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/* \
  && case "$TARGETARCH" in \
       amd64) arch=x64; sha="$MISE_SHA256_AMD64" ;; \
       arm64) arch=arm64; sha="$MISE_SHA256_ARM64" ;; \
       *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
     esac \
  && curl -fsSLo /tmp/mise.tar.gz \
       "https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-${arch}-musl.tar.gz" \
  && echo "$sha  /tmp/mise.tar.gz" | sha256sum --check - \
  && tar -xzf /tmp/mise.tar.gz -C /tmp \
  && install -m 0755 /tmp/mise/bin/mise /usr/local/bin/mise
# gh, nono and tmux at exactly the versions the repository pins. The config
# stays in this stage: a system mise config in the final image would enter
# every agent's and plugin's tool resolution.
COPY mise.toml /build/mise.toml
WORKDIR /build
ENV MISE_DATA_DIR=/opt/mise MISE_CACHE_DIR=/tmp/mise-cache MISE_TRUSTED_CONFIG_PATHS=/build
RUN --mount=type=secret,id=github_token,env=GITHUB_TOKEN \
  mise install gh nono tmux \
  && mkdir /out \
  && for tool in gh nono tmux; do install -m 0755 "$(mise which "$tool")" "/out/$tool"; done

FROM base
# `upgrade` pulls the security archive's fixes for packages the pinned base
# digest predates; without it the trivy gate fails on the base image's own
# packages until upstream rebuilds trixie-slim.
RUN apt-get update \
  && apt-get upgrade -y --no-install-recommends \
  && apt-get install -y --no-install-recommends ca-certificates git tini \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --uid 10001 --create-home --shell /bin/bash balerix
COPY --from=tools /usr/local/bin/mise /usr/local/bin/mise
COPY --from=tools /out/ /usr/local/bin/
COPY --chmod=0755 balerix /usr/local/bin/balerix
ENV LANG=C.UTF-8 HOME=/home/balerix
USER 10001:10001
WORKDIR /home/balerix
# The XDG state, config and data roots all live under $HOME.
VOLUME /home/balerix
ENTRYPOINT ["/usr/bin/tini", "--", "balerix"]
CMD ["serve"]
