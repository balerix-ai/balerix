# The operator image (Spec O §13): one static binary on distroless static,
# as the plugin images are. The context from the release scripts
# (sub-project 5) or scripts/kind-up.sh holds it as `balerix-operator`.
# distroless/static carries the CA bundle the Kubernetes client needs.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:1c2c046bc09ed40fad370b599a0b1ae7987f55b01e247cf27a7c27cd97e5bbc7
COPY --chmod=0755 balerix-operator /usr/local/bin/balerix-operator
ENTRYPOINT ["/usr/local/bin/balerix-operator"]
CMD ["run"]
