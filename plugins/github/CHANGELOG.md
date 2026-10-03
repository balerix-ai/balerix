# Changelog

## 0.1.0 - 2026-10-03

### Features

- **matrix:** Show and answer Claude's questions from the thread (Spec J) (#47)
- **common:** Extract balerix-plugin-common from matrix and web (Spec K)
- **core:** Plugin-managed fleets, owned records and a per-agent branch (Spec L)
- **core:** Restricted settings surface for plugin-applied fleet files, the operator's fleetDefaults layer, and the fleets-gated PUT answer (Spec M §12) (#86)
- **common:** Delivery confirmation, adopted by matrix as 📨 then 👍 (Spec M §8.7, #85) (#91)
- **github:** The GitHub plugin, an agent per issue or pull request (Spec M) (#93)
- **server:** Kubernetes mode and the balerix-agent sidecar over a TLS link (Spec O §6, §7) (#125)
- **operator:** The operator project, its five kinds and pure desired state; the sync and harvest Jobs (Spec O §20) (#126)

### Bug fixes

- **examples:** Sandbox.network.mode is not a nono key (#35)
- **core:** Stop and refuse a 0.1.x agent at the first pass; a failed step gets the phase `failed` (#76)
- **common:** Press Enter again while a prompt is unconfirmed, adopted by github and matrix (#99) (#101)
- **flow:** Press Enter again while a submitted send is unconfirmed (#100) (#102)
- **github:** Ignore a redelivered opening on a live row; deliver reviews in arrival order (#96) (#103)
- **github:** A fixed line for a failed action; three health kinds behind the one cell (#98) (#104)
- **github:** Write the closed row in end at once; prune closed rows after seven days (#97) (#105)
- **github:** Page review comments to 1000; percent-encode request URLs; round-trip tests of the 401 refetch (#95) (#106)
