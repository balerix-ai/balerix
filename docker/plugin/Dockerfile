# A plugin image (Spec I §6.3): one static plugin binary. The context from
# scripts/release/image-context.sh holds it as `plugin`, whichever plugin it
# is. distroless/static carries the CA bundle matrix's HTTP client needs.
# The daemon does not run plugins from images; it runs the release package.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:18f4cc26c9ffa7f20a359298be7deadac5c607c8e03a02b8119f1a5fc2f82c99
COPY --chmod=0755 plugin /usr/local/bin/balerix-plugin
ENTRYPOINT ["/usr/local/bin/balerix-plugin"]
