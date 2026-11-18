# The operator image (Spec O §13): one static binary on distroless static,
# as the plugin images are. The context from the release scripts
# (sub-project 5) or scripts/kind-up.sh holds it as `balerix-operator`.
# distroless/static carries the CA bundle the Kubernetes client needs.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931
COPY --chmod=0755 balerix-operator /usr/local/bin/balerix-operator
ENTRYPOINT ["/usr/local/bin/balerix-operator"]
CMD ["run"]
