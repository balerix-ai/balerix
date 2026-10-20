# A plugin image (Spec I §6.3): one static plugin binary. The context from
# scripts/release/image-context.sh holds it as `plugin`, whichever plugin it
# is. distroless/static carries the CA bundle matrix's HTTP client needs.
# The daemon does not run plugins from images; it runs the release package.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:516757106de35205efb70b91ba03f6f1298424a0678c603255cf2852a7000087
COPY --chmod=0755 plugin /usr/local/bin/balerix-plugin
ENTRYPOINT ["/usr/local/bin/balerix-plugin"]
