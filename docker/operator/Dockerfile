# The operator image (Spec O §13): one static binary on distroless static,
# as the plugin images are. The context from the release scripts
# (sub-project 5) or scripts/kind-up.sh holds it as `balerix-operator`.
# distroless/static carries the CA bundle the Kubernetes client needs.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:516757106de35205efb70b91ba03f6f1298424a0678c603255cf2852a7000087
COPY --chmod=0755 balerix-operator /usr/local/bin/balerix-operator
ENTRYPOINT ["/usr/local/bin/balerix-operator"]
CMD ["run"]
