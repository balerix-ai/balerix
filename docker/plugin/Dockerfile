# A plugin image (Spec I §6.3): one static plugin binary. The context from
# scripts/release/image-context.sh holds it as `plugin`, whichever plugin it
# is. distroless/static carries the CA bundle matrix's HTTP client needs.
# The daemon does not run plugins from images; it runs the release package.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:8ce6dd87e9823c5b48dcfb2c8604714ccb351ce59607327bcc4957448da279b5
COPY --chmod=0755 plugin /usr/local/bin/balerix-plugin
ENTRYPOINT ["/usr/local/bin/balerix-plugin"]
