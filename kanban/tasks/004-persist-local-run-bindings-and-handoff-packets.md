---
id: 4
title: Persist local run bindings and handoff packets
status: review
priority: high
created: 2026-08-20T13:01:03.405685+10:00
updated: 2026-08-20T13:18:37.092031+10:00
started: 2026-08-20T13:01:03.455188+10:00
tags:
    - persistence
    - handoff
class: standard
---

User outcome: Dock writes and reads strict versioned handoff packets from local-only storage without committing machine paths, pane identifiers, or transcripts.\n\nAcceptance:\n- atomic write avoids partially persisted JSON\n- read validates packet schema\n- path traversal is rejected\n- round-trip and corrupt-file tests pass\n\nOut: cloud sync, raw terminal logs, secrets, credentials.

[[2026-08-20]] Thu 13:18
Implemented atomic local-only JSON handoff storage. Strict load validation rejects corrupt data, unknown fields, unsupported schema versions, and traversal-like run IDs. Eleven tests plus save/load CLI smoke passed.
