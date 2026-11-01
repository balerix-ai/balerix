# The operator image (Spec O §13): one static binary on distroless static,
# as the plugin images are. The context from the release scripts
# (sub-project 5) or scripts/kind-up.sh holds it as `balerix-operator`.
# distroless/static carries the CA bundle the Kubernetes client needs.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:25ac8f0c3b2334a9eb05452266bd4697864a8c8ef0ef71e5e6480d7f92868329
COPY --chmod=0755 balerix-operator /usr/local/bin/balerix-operator
ENTRYPOINT ["/usr/local/bin/balerix-operator"]
CMD ["run"]
