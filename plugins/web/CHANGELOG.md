# Changelog

## 0.2.2 - 2026-10-07

### Features

- **core:** Restricted settings surface for plugin-applied fleet files, the operator's fleetDefaults layer, and the fleets-gated PUT answer (Spec M §12) (#86)
- **common:** Delivery confirmation, adopted by matrix as 📨 then 👍 (Spec M §8.7, #85) (#91)
- **github:** The GitHub plugin, an agent per issue or pull request (Spec M) (#93)
- **server:** Kubernetes mode and the balerix-agent sidecar over a TLS link (Spec O §6, §7) (#125)
- **operator:** The operator project, its five kinds and pure desired state; the sync and harvest Jobs (Spec O §20) (#126)
- **operator:** The controllers, envtest tests, the images, kind and e2e-k8s (Spec O §21) (#127)
- Plugins without a cluster (Spec O 4a, §23.1–§23.3) (#143)
- Plugins on a cluster (Spec O 4b, §23.4–§23.9) (#144)

### Bug fixes

- **common:** Press Enter again while a prompt is unconfirmed, adopted by github and matrix (#99) (#101)
- **flow:** Press Enter again while a submitted send is unconfirmed (#100) (#102)

## 0.2.1 - 2026-09-27

### Features

- **matrix:** Show and answer Claude's questions from the thread (Spec J) (#47)
- **common:** Extract balerix-plugin-common from matrix and web (Spec K)
- **core:** Plugin-managed fleets, owned records and a per-agent branch (Spec L)

### Bug fixes

- **examples:** Sandbox.network.mode is not a nono key (#35)
- **core:** Stop and refuse a 0.1.x agent at the first pass; a failed step gets the phase `failed` (#76)

## 0.2.0 - 2026-09-11

- Initial release.
