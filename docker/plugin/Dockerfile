# A plugin image (Spec I §6.3): one static plugin binary. The context from
# scripts/release/image-context.sh holds it as `plugin`, whichever plugin it
# is. distroless/static carries the CA bundle matrix's HTTP client needs.
# The daemon does not run plugins from images; it runs the release package.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:1c2c046bc09ed40fad370b599a0b1ae7987f55b01e247cf27a7c27cd97e5bbc7
COPY --chmod=0755 plugin /usr/local/bin/balerix-plugin
ENTRYPOINT ["/usr/local/bin/balerix-plugin"]
