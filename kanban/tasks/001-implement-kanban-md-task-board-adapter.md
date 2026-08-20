---
id: 1
title: Implement kanban-md task-board adapter
status: review
priority: high
created: 2026-08-20T12:52:11.859071+10:00
updated: 2026-08-20T12:54:23.014331+10:00
started: 2026-08-20T12:52:11.891774+10:00
tags:
    - integration
    - kanban
class: standard
---

User outcome: Dock can read a kanban-md task, atomically claim it, and retain the task identifier as the durable side of a run binding.\n\nAcceptance:\n- fixture process tests cover list, pick, and move invocation shapes\n- Dock never mutates Markdown directly\n- a real local board smoke proves atomic claim and move\n\nOut: Herdr pane launch, Git worktree creation, automatic completion.

[[2026-08-20]] Thu 12:54
Dock now invokes kanban-md list/pick through a typed adapter. Unit tests verify one atomic pick command and invalid claim rejection; local smoke claimed task 2 through Dock. Ready for review.
