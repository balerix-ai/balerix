# Changelog

## 0.2.0 - 2026-10-17

### Features

- **api:** [**breaking**] Non-exhaustive plugin-read types; WorkspaceTree.truncated (#240) (#266)
- Channel text hardening, an idle-only nudge, and a slash-command allowlist (#268)
- Report missed hook events to plugins; reset turn tracking on re-activation (#274)

### Bug fixes

- Kubernetes link and attach limits, silence deadlines, workspace reply caps (#229, #230, #231) (#244)
- **web:** Review page sweep fixes; bound SDK handshakes; strip control characters from sent text (#225, #226, #227, #228) (#242)
- **server:** Plugin hello, kv prefixes and metric names (#222, #223, #224) (#243)
- Attach writes see a viewer leave and end on a stall; sidecar side tested (#245) (#250)
- **common:** [**breaking**] Split always ends on long fence lines; a two-lane plugin queue (#254, #260) (#273)
- Make Claude act on pasted plugin messages; confirm wrapped deliveries (#279) (#282)
- **deps:** Rustls 0.23.46 and quinn-proto 0.11.19 for RUSTSEC advisories (#284)
- **deps:** Update cargo patch updates (plugins/common) (#320)

## 0.1.4 - 2026-10-10

### Bug fixes

- Plugin host leftovers batch (#16, #176, #13, #178, #175) (#201)

## 0.1.3 - 2026-10-09

### Bug fixes

- Plugin batch (#17, #42, #20, #49, #48) (#159)
- Daemon batch (#9, #11, #10, #12, #116) (#166)
- Plugin host batch (#170, #15, #14, #168, #6, #1, #169) (#172)
- Plugin hardening batch (#2, #3, #4, #5, #7) (#188)

## 0.1.2 - 2026-10-07

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

## 0.1.1 - 2026-09-27

### Features

- **core:** Plugin-managed fleets, owned records and a per-agent branch (Spec L)

### Bug fixes

- **core:** Stop and refuse a 0.1.x agent at the first pass; a failed step gets the phase `failed` (#76)

## 0.1.0 - 2026-09-23

### Features

- **matrix:** Show and answer Claude's questions from the thread (Spec J) (#47)
- **common:** Extract balerix-plugin-common from matrix and web (Spec K)

### Bug fixes

- **examples:** Sandbox.network.mode is not a nono key (#35)
