<!--
Captured verbatim from https://shared-human-agent-workspace-detailed-fkpn.pages.grok.bot/
on 2026-11-07 and converted to Markdown. Layout-only elements (page chrome, the
contents fold, step-number badges) were dropped; the section 15 figure is reconstructed
as text. Wording is otherwise unchanged.
-->

# Shared human-agent workspace — detailed product specification v0.3

A proposed standalone work system for people and agents: visible task structure, permissioned context, portable execution evidence and selectable project views. Detailed critical specification, 7 November 2026 (AEDT). Unbuilt; research and writing only.

On this page

- [1. Executive judgement](#1-executive-judgement)
- [2. First user, jobs and product boundary](#2-first-user-jobs-and-product-boundary)
- [3. A complete project, including failure](#3-a-complete-project-including-failure)
- [4. Navigation and everyday experience](#4-navigation-and-everyday-experience)
- [5. Object model and authoritative state](#5-object-model-and-authoritative-state)
- [6. Task, run and review semantics](#6-task-run-and-review-semantics)
- [7. Permissions, disclosure and trust](#7-permissions-disclosure-and-trust)
- [8. Index and retrieval contract](#8-index-and-retrieval-contract)
- [9. Proposed proprietary context engine](#9-proposed-proprietary-context-engine)
- [10. Portable execution and compatibility](#10-portable-execution-and-compatibility)
- [11. Coordination and failure recovery](#11-coordination-and-failure-recovery)
- [12. Calendar and capacity planning](#12-calendar-and-capacity-planning)
- [13. Modalities and artifact safety](#13-modalities-and-artifact-safety)
- [14. Plugin platform boundaries](#14-plugin-platform-boundaries)
- [15. Architecture, sync and device trust](#15-architecture-sync-and-device-trust)
- [16. Performance contract](#16-performance-contract)
- [17. Competition and defensible IP](#17-competition-and-defensible-ip)
- [18. MVP, milestones and economics](#18-mvp-milestones-and-economics)
- [19. Evaluation, kill gates and recommendation](#19-evaluation-kill-gates-and-recommendation)
- [Sources and evidence limits](#sources-and-evidence-limits)

## 1. Executive judgement

Conditional PASS for focused validation—not approval to build the platform.

The opportunity is a reliable shared place where humans and independent agents can understand, perform and review the same project. The largest threat is not a missing feature: incumbents already combine tasks, notes and agents, and a coordination layer can add more supervision than it removes.

Recommended product promise: define a goal once, see its breakdown, assign work to yourself or named agents, and retain evidence of what was authorized, attempted and accepted. The workspace manages work; external runtimes execute agent work. This distinction must remain understandable even when an agent changes harness or fails halfway through a task.

The possible differentiated core is a permission-aware context engine coupled to a portable explicit execution record. It should preserve critical constraints, cite original sources and invalidate stale context; handoff should preserve completed work and approvals without pretending to transfer hidden reasoning. These are engineering hypotheses, not proven inventions or patentable assets.

Build-versus-validate judgement: the full vision is coherent, but too large for an undifferentiated first release. Validate a thin standalone project workspace with one human, two real runtime adapters, task tree, Markdown notes, due-calendar lens, approvals and bounded context. Do not regress to a notes sidecar or generic retrieval app; equally, do not begin by rebuilding Notion, sync, mobile and a plugin marketplace simultaneously.

No demand, performance, compatibility or defensibility claim is established. All algorithms, schemas, targets and release gates here are proposals. Nothing in this document authorizes code, prototypes, installations, account access, spending, external messages or publication.

## 2. First user, jobs and product boundary

| Question                  | Opinionated answer                                                                                                                                                                                                         |
|---------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| First customer hypothesis | A technical solo builder or research lead using multiple agent harnesses on one evidence-heavy project. Start with research → specification → deliverable, not arbitrary autonomous company management.                    |
| Primary job               | Coordinate personal and agent commitments without losing dependencies, permissions, decisions or evidence when conversations and runtimes change.                                                                          |
| Pain hypotheses           | Repeated briefing, hidden subtasks, uncertain agent status, results detached from source notes, duplicate attempts and unclear review responsibility. These require user evidence; they are not established market demand. |
| Human-only value          | Capture notes, decompose work, assign self, schedule blocks, view deadlines, attach deliverables and review completion without AI or a subscription.                                                                       |
| App owns                  | Project objects, task states, context disclosure, approvals, evidence, run records, views and coordination policy.                                                                                                         |
| Runtime owns              | Model calls, process/session execution and approved tools/environment. A runtime binding identifies host/version/provider; it is not the task's assignee identity.                                                         |
| Not the initial target    | Enterprise PM migration, full Notion databases, notebook collectors, realtime whiteboard teams, or people needing universal native harness support immediately.                                                            |

A named agent represents a continuing work role, such as Researcher, sponsored by a human. That role can run through different compatible bindings. A person is also an assignee; the same project can contain a human interview task and an agent source-comparison task. One task has one responsible assignee initially; joint work is represented by subtasks, avoiding responsibility hidden behind a list of names.

The future vision includes multiple humans, guests, shared calendars, multimedia, diagrams and plugins. The initial schema anticipates these without pretending their UX, security or sync is already solved. ‘Shared’ initially means one human and their authorized agents—not permission to combine K's private data with S's or existing shared material.

## 3. A complete project, including failure

Illustrative project: evaluate a new product opportunity and produce an internal decision brief. The human owns the outcome and performs interviews. Agent A gathers evidence; Agent B synthesizes and critiques. A separate reviewer role can be filled by the human initially and another authorized person later.

1. **Goal and safe context.** Human creates a project in a permitted area, defines success criteria and a due date, and links three approved source notes. A personal financial note remains private even if related. The project does not expose every backlink.

2. **Proposed breakdown.** Under planning-only access, A proposes six tasks: source map, competitor evidence, interviews, synthesis, independent review and final decision. It marks prerequisites and estimate ranges. Human accepts the structural diff; indentation is not interpreted as dependency or execution consent.

3. **Assign and allocate.** Interviews go to the human; evidence to A; synthesis to B, blocked until evidence is accepted. Human reserves an interview block and leaves agent work in a separate capacity lane. Moving a block does not move the decision deadline.

4. **Start deliberately.** Human chooses A's binding, previews outbound context/provider and approves one run with scoped reads, artifact drafts, time/tool limits and zero additional spend. Coordinator grants a fenced claim only after readiness checks. Assignment alone did not start a process.

5. **Failure with usable evidence.** A saves a source table and factual checkpoint, then its host disconnects. UI shows last heartbeat and unknown/stale status. Task is not Done. Coordinator fences new writes; it does not assume A stopped or dispatch a second irreversible attempt.

6. **Controlled handoff.** Human selects another tested binding. After checking no unknown effects remain, a new run receives task ID, accepted inputs, checkpoint, artifact hashes and remaining work. It does not receive credentials or hidden session memory. The old run's late output is quarantined, not silently committed.

7. **Review, delegation and revocation.** Human accepts evidence and explicitly dispatches B with only the synthesis pack. B requests an additional private note; human refuses. If A's grant is revoked or an input source changes, dependent packs become unusable for further authorized effects. Already disclosed material cannot be recalled.

8. **Decision and completion.** B returns a cited brief with uncertainty and a proposed recipient email. Agent review can flag omissions, but human accepts the final artifact and separately decides whether to send. Rejecting the email does not reject the brief. Completion requires criteria and reviewer acceptance, not a successful process exit.

Every step has a manual equivalent. The human can produce the evidence themselves, mark a blocker, attach a document and seek review. Agent work should not create a separate privileged workflow that makes human tasks second-class.

## 4. Navigation and everyday experience

| Surface             | Purpose                                                                                                                                                                                |
|---------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Inbox / My work     | Quick capture, unprocessed notes, tasks assigned to the human, approvals and review requests. Keyboard capture works locally and does not invoke a model.                              |
| Areas → projects    | Permissions and long-lived responsibility live in areas; project home shows goal, next actionable work, blockers, notes and deliverables. Search and views stay within current access. |
| Project work canvas | Explicit selector: list, board, task tree, dependency diagram, calendar, note/document or artifact preview. These render the same object graph, not separate databases.                |
| Task detail         | Criteria, owner/assignee/reviewer, dependencies, linked notes, due/scheduled fields, current attempt, evidence and review decisions. Show what is missing before offering Start.       |
| Runs / review queue | Last observed state, actual checkpoints, errors, requested approvals and proposed diffs. Group routine events; elevate blocked, failed, stale or review-ready outcomes.                |

Human interaction loop: capture → clarify → link → plan → act → review. A note can create a linked task using selected text, but note and task then have explicit identities: changing the note does not silently change a task's approved instructions. Source-change warnings make that relationship visible.

View changes are user-controlled. Predictable rules may choose an initial renderer—open a note in its editor, event in agenda, PDF in preview—with remembered override. AI may suggest a more useful view but cannot rearrange the workspace mid-action. Saved views store query/layout settings and reapply permissions when opened; sharing a view does not share its hidden results.

Use counts and plain status, not activity theatre: accepted deliverables, remaining criteria, blockers and last evidence time. No fabricated progress percentage from model confidence, tool calls or tokens. Notifications default to in-app exception bundles; lock-screen text hides content. Approvals use trusted first-party UI, never an agent-authored HTML card.

This product's canvas means its selected work surface. This specification Page is only a document about that product, not an interactive app prototype or a competitor-interface imitation.

## 5. Object model and authoritative state

| Object                      | Minimum fields and relationships                                                                                                                                                                               |
|-----------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Common envelope             | Immutable ID, schema version, workspace/area, revision/hash, creator, timestamps, provenance and tombstone. Human-readable task number is an alias, not identity.                                              |
| Workspace / area / project  | Workspace isolation; area grants and inheritance boundary; project goal, owner, criteria and one home area initially. Cross-area links do not grant access.                                                    |
| Person / agent / binding    | Principal ID and role; agent human sponsor and allowed capabilities; binding has harness/version, host/endpoint, execution mode and local credential reference. Credentials never become normal note fields.   |
| Note / block                | Note title/body and ordered stable-ID blocks; typed content, links and private/agent-readable labels. A document view can assemble linked material without copying authority.                                  |
| Task / subtask              | Project, optional parent, goal, criteria, owner, one assignee, reviewer, priority, state, due date/time, estimate range, context links and evidence requirements. Subtask is the same type with a parent edge. |
| Dependency                  | Prerequisite ID, successor ID, condition and revision. Initial condition: prerequisite accepted before successor starts. Dependency DAG and containment tree are distinct, both cycle-checked.                 |
| Event / schedule            | Task reference, interval/all-day dates, zone, wall-clock intent, participants, recurrence/exception IDs and optional external mapping. Human block, due marker and agent start window are distinct concepts.   |
| Source / artifact           | Original origin/bytes/hash, extraction version, author/run, MIME, source locators, citations and acceptance state. Derived text/OCR/summary is separate from the original.                                     |
| Run / checkpoint            | Task and attempt IDs, binding, input snapshot, scope, lease generation, status, events, cost observations, checkpoints and output hashes. Checkpoint stores work facts, not hidden reasoning.                  |
| Context pack / approval     | Pack budget, purpose, constraint inventory, source revisions, omissions and policy epoch. Approval exact actor/action/payload/target versions, destination, expiry, decision and receipt link.                 |
| View / action / audit event | Permission-filtered query/renderer; action is an intended mutation or disclosure; audit event identifies principal/run, command, revisions, decision and effect receipt.                                       |

Owner is accountable; assignee performs; reviewer accepts criteria. These roles do not imply equal permissions. Project ownership remains human in MVP. A reviewer agent can critique or accept specifically delegated low-risk criteria, but cannot grant itself send/delete rights or widen another agent's context.

Use a transactional write service: validate a command against expected revision, policy and invariants, then atomically persist changed objects and a durable event. Materialized tables, full-text index, graph adjacency and UI views are projections. Store snapshots/checkpoints to avoid replaying unbounded history at startup. A file export is not a second writer.

Parent tasks close only when required children and parent-level acceptance criteria are satisfied. Optional/canceled children remain explicit; canceled prerequisites block successors until plan revision. Scope changes update the tree and denominator visibly. Deleting an object preserves a tombstone and invalidates citations; restoration creates an auditable revision rather than rewriting history.

## 6. Task, run and review semantics

| State or transition                   | Meaning and guard                                                                                                                                                                                                       |
|---------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Proposed → Ready                      | Human accepts goal/decomposition and criteria; assignee, inputs and approved scope exist. Dependencies must be accepted before execution can begin. Proposed plan edits never auto-dispatch.                            |
| Ready → In progress                   | Human self-starts or coordinator accepts an authorized run claim. Claim is a run-level state; waiting for a host is not proof work started.                                                                             |
| Claimed → Running                     | Binding acknowledges the correct input snapshot and lease generation. UI reports last observed runtime state, not asserted omniscience.                                                                                 |
| Blocked / Waiting approval            | Task records blocker and return state; run pauses for missing input or exact approval. A request does not widen permission. Unblocking rechecks current sources and grants.                                             |
| Succeeded → In review                 | Run produced required artifact candidates. Exit code or model declaration alone cannot establish accepted task completion.                                                                                              |
| In review → Done / changes requested  | Authorized reviewer checks criteria/evidence, accepts or returns work. Acceptance records artifact/source versions; changed deliverables require renewed review.                                                        |
| Failed attempt                        | Run ends with reason and retained evidence. Task returns Ready or Blocked, with failed status visible. Explicit abandoned/failed task outcome is separate from temporary execution failure.                             |
| Cancel requested → Canceled / Unknown | Fence future effects, invalidate approvals, request upstream cancellation. Acknowledgement/reconciliation determines final run status; already-made effects remain recorded. Reopening creates an explicit new attempt. |

Running, failed and canceled are not interchangeable across task and run layers. One task can have three failed runs and one accepted outcome. Never erase failed attempts when retrying. A stale heartbeat yields Unknown, not auto-reassignment. The interface can simplify labels but must retain these semantics.

Distinguish due date, human allocation, agent start window and run timeout. Estimates include author, unit, range, uncertainty and timestamp, not a guaranteed completion clock. Show ‘3 of 5 required deliverables accepted’, unresolved blockers and elapsed actual time; do not manufacture ‘80% complete’.

## 7. Permissions, disclosure and trust

Deterministic rule: effective rights are the intersection of human sponsor, area/object grants, agent principal, runtime/run scope, plugin scope and tool capability; applicable denial wins. The policy engine, not a prompt or probabilistic ranker, enforces this on every read, draft, commit and disclosure.

| Control                      | Concrete behavior and failure case                                                                                                                                                                                                                          |
|------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Inherited but bounded grants | Area rights inherit only within declared maximum objects/actions/expiry. Object denials narrow access. A link, dependency, search hit or cached summary never expands scope.                                                                                |
| Context agreement            | Preview included objects/passages, destination provider/binding, purpose, time/expiry and token/byte limit. Purpose is recorded intent, not provable downstream use. Remote disclosure requires explicit consent even when the runtime costs nothing extra. |
| Read / draft / write / send  | Separate capabilities. Draft proposes a patch; write commits allowed changes; send crosses a recipient/provider boundary. Assignment or artifact acceptance implies none of the others. Bulk/deletion rights are separate.                                  |
| Approval identity            | Bind approval to actor, task/run, exact payload/diff hash, target/source revisions, destination, policy epoch and expiry. Revalidate immediately before effect. Changed payload or stale version invalidates it; deny cannot be routed around.              |
| Delegation                   | Agent proposes task/recipient/context; human approves initial dispatch. Child grant cannot exceed sponsor authority or permitted subset, and cannot silently inherit another run's token. MVP has no autonomous cross-agent sends.                          |
| Area move                    | Preview newly exposed data; require source/destination rights, freeze affected runs and invalidate grants/packs/approvals. Linked private notes keep their labels. Reviewer cannot drag a task into a broader area to export its context.                   |
| Audit and redaction          | Record IDs/hashes, who read/disclosed what, authorization and receipts. Keep sensitive payload audit access narrower; redact secrets and ordinary telemetry. Hash chaining can expose tampering, not make administrator compromise impossible.              |
| Revocation                   | Stop future gateway access at authority and fence runs. Expire leases when freshness is unknown. Offline devices learn later; provider-retained context, exported files and old backups cannot be recalled.                                                 |

Imported notes, PDFs, messages, manifests and agent outputs are untrusted data. Isolate source content from policy instructions, sanitize rendering, validate tool schemas, restrict network/effects and test hostile sources. An instruction embedded in a competitor article cannot create an approval. Injection detection is a weak extra signal, never the authorization boundary.

Protect metadata too: unauthorized titles, counts, relationship edges, snippets, search timing and summary generation may leak. Filter before generating summaries, cache by principal/scope/version, and avoid reporting hidden existence. Private K, private S and shared spaces retain separate principals and indexes. Secret fields use credential references outside retrieval/export; scanners may miss secrets, so narrow selection and human preview remain necessary.

Future multi-human design needs guest read/comment scopes, reviewer-only access and authorized governance changes; do not treat admin as invisible consent from another person. MVP has one human, but adversarial tests must already cover multiple synthetic identities. A runtime with independent filesystem/network credentials can bypass this app: strong guarantees require constrained execution, not merely an app permission screen.

## 8. Index and retrieval contract

The durable workspace is not a model context window. A generic harness discovers it only through an explicitly configured MCP endpoint, narrow CLI/SDK bridge or mounted bootstrap manifest. A short project instruction can point to that entry; never dump the entire graph into AGENTS.md. Remote-only clients cannot magically reach desktop localhost.

| Contract       | Required response shape                                                                                                                                                                                                                       |
|----------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Bootstrap      | Schema version, workspace ID, endpoint/transport, auth requirements, supported operations and visible scopes after authentication. No secrets or private project inventory in an unauthenticated manifest.                                    |
| Index / search | Paginated/filterable area/project/task records: ID/type, short goal, relevant snippet, revision/hash, freshness and authorized parent/dependency IDs. Search supports project/state/assignee filters; no full-graph payload default.          |
| Fetch / expand | Select fields/blocks or source passage by stable ID; bounded neighbors and cursors. Return citations to object/block/PDF page or media timestamp. Reauthorize every expansion.                                                                |
| Pack           | Task/purpose/budget, constraints, accepted decisions, evidence snippets, unresolved questions, source versions, omissions and retrieval handles. Known tokenizer counts plus byte limit; estimated token counts explicitly labeled otherwise. |
| Delta          | Opaque principal/scope-bound cursor returns changed/deleted IDs and invalidations. Cursor expiry triggers bounded resync. Policy changes purge connected-client caches; exported copies are outside live control.                             |

Illustrative authenticated manifest—not an implemented protocol: schema=1; workspace=W7; transport=configured-local-MCP; capabilities=\[index, search, fetch, pack, draft, status\]; visible scope=area-product/project-P12; max_page=50; policy_epoch=E9; delta_cursor=C18. Endpoint configuration and authentication are supplied separately; no bearer token belongs in the file.

Illustrative pack: task=T24@r7; purpose=compare alternatives; budget=2,000 input tokens plus 400 retrieval-overhead allowance; required constraints=\[local-only, no external send\]; evidence=\[N8/B3@h1, PDF4/page6@h2\]; uncertainty=\[pricing not independently verified\]; omitted=\[long background appendix\]; freshness=current at policy epoch E9. These are design examples, not measurements or actual workspace data.

Start with explicit project hierarchy, full-text search and source links. A relationship graph improves dependency/history expansion; embeddings can rescue vocabulary mismatch but add model cost, stale indexes and difficult ranking explanations. Compare these progressively rather than presume a vector database is essential. None makes compression lossless.

Every summary retains source dependency hashes and grants. Editing a source marks dependent packs stale immediately in authoritative metadata; asynchronous recomputation cannot leave them marked current. A cache key includes policy epoch and engine version. Critical actions require current inputs or stop; ordinary reading may show a clearly stale preview. User corrections can improve local ranking, never weaken permissions.

## 9. Proposed proprietary context engine

This is a proposed algorithm worth testing, not a renamed promise that retrieval will solve everything. The technical objective is maximum useful, constraint-preserving evidence under a budget and disclosure policy. Unknown constraints cannot be formally guaranteed simply because an LLM says they were preserved.

1. **Establish deterministic eligibility.** Authenticate principal/run, resolve current policy epoch and allowed objects/actions/provider. Filter candidates before snippets, summaries or model processing. Reject missing/expired scope. Record snapshot and budget.
2. **Build a task requirement inventory.** Extract explicit goal, acceptance criteria, user-pinned constraints and accepted decisions. Separate verbatim mandatory rules from model-inferred helpful context. Human can correct the inventory. Inference cannot create authority or override a pinned rule.
3. **Retrieve and expand deliberately.** Retrieve bounded hierarchy/full-text candidates; optionally add semantic candidates. Expand accepted prerequisite outcomes, relevant source history and unresolved decisions with hop/object limits. Exclude unrelated personal context and superseded claims unless needed to explain a conflict.
4. **Rank for utility and diversity.** Score task relevance, evidence quality, freshness, accepted provenance and expected marginal coverage; penalize redundancy, stale summaries and unverified agent claims. Enforce source diversity and deduplicate quoted copies. A learned ranker may order eligible candidates but never decide permissions.
5. **Select under the real budget.** Reserve space for task/criteria and all known mandatory constraints first. Add evidence greedily by marginal coverage per token, with source/page boundaries preserved. Account for tool metadata and citation overhead. If mandatory content alone exceeds budget, return insufficient-budget/clarification, not a silently truncated constraint.
6. **Verify and emit.** Deterministically verify pinned-constraint presence, allowed scope, budget and current source hashes. A secondary model may flag contradictions or missing support but is not a proof. Emit selected evidence, citations, coverage map, omissions, uncertainty and source-retrieval handles; show reasons each item was included.
7. **Learn from explicit corrections.** Capture which source the reviewer needed, which omission mattered and whether the task was accepted after correction. Prefer correction-specific signals over treating every accepted answer as truth. Version local ranking changes and retain rollback/evaluation comparisons.

Example: a task asks for a deployment recommendation and the generic similarity search retrieves ten cloud-comparison notes. The project constraint ‘no paid services’ is less semantically similar but mandatory; it must survive selection. If an older accepted decision conflicts with a newer source, include the conflict and dates instead of summarizing both into a confident false consensus.

Poisoning risk is substantial: an agent could flood notes with self-endorsing claims or learn to game acceptance. Keep source reliability and acceptance as separate features; identify imported/agent-created material, cap repeated-source influence and preserve contradictions. Recompute critical facts from originals. A user accepting a convenient answer is not an independent truth label.

Default learning stays within that user's workspace. Retention is configurable; users can inspect/delete correction history and reset personalization. Cross-user improvement requires separately consented, licensed, minimized examples or synthetic/curated evaluation cases, with held-out privacy checks. No automatic pooling of private projects, embeddings or raw corrections. A local-only design limits centralized telemetry and makes a global data moat slower—not magically impossible or free.

## 10. Portable execution and compatibility

| Handoff record         | Minimum meaning                                                                                                                                                                                         |
|------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Identity / input       | Workspace/task ID, task revision, attempt ID, prior attempt, acceptance criteria hash and source snapshot. Preserve work identity when runtime changes.                                                 |
| Checkpoint / evidence  | Completed factual steps, output hashes/locations, source citations, verification receipts, outstanding blockers and next permitted step. No credentials or raw hidden model state.                      |
| Control state          | Lease generation, last confirmed action, unknown-effect list, remaining budget, cancellation state and approval decisions tied to payload versions. Old unused approvals do not transfer automatically. |
| Authorization / import | Receiving identity mapping, permitted subset and fresh grant from destination authority. Permission manifest describes intent; it is not a live token or proof enforcement exists there.                |
| Resume acknowledgement | New binding confirms compatible capabilities and accepted checkpoint; authority fences old attempt and creates new run. If state/effects are ambiguous, pause for reconciliation.                       |

Use existing interfaces: MCP resources/tools for bounded access and proposed changes; SDK/CLI for hosts needing a narrow local bridge; A2A only when peers actually expose compatible task/artifact/status capabilities. The app's policy and execution record are product contracts, not a new universal transport protocol. Data access, coordination messages, run control and visible UI actions are separate planes.

First adapter candidates are Claude Code and Codex CLI on a pinned OS/version. Their primary docs confirm stdio and HTTP MCP support \[S15–16\], not our full launch/status/cancel/checkpoint implementation. Certification must test these behaviors, authentication and restricted tool environments. A tools-only MCP connection qualifies as access-compatible, not execution-certified. Hosted web surfaces have different reach and cannot inherit local config.

Compatibility tiers: certified control with tested recovery; standards-compatible access after capability negotiation; explicit manual/export fallback with no live enforcement. Publish versions and feature boundaries. A2A version negotiation, streaming and push are optional \[S14\]. Never claim every agent implements A2A or that MCP guarantees general session migration.

A handoff reconstructs allowed work from explicit evidence. It cannot move hidden reasoning, provider memory, authentication sessions or guarantee equivalent outputs. Local bridges can run offline with genuinely local runtimes; cloud endpoints require approved network disclosure. The client may keep its own copy after revocation. ‘Take it anywhere’ therefore means portable data plus honest, tested adapters—not universal native execution.

## 11. Coordination and failure recovery

Choose one run authority per workspace initially. Commands use expected revisions and atomic claims; each lease has a monotonically advancing fencing generation. An old run cannot commit through the gateway once a new generation exists. This controls app-authorized effects, not an external process already holding unrelated privileges.

| Failure                     | Required behavior                                                                                                                                                                                                                            |
|-----------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Duplicate dispatch/event    | Durable outbox/inbox, unique command/action IDs, acknowledged at-least-once delivery and deduplication. Replayed messages do not restart work or reapply accepted patches. Order within a run where necessary; no global exactly-once claim. |
| Host stops heartbeating     | Proposed starting values: heartbeat 10 seconds, stale after 30, lease 60; tune under tests. UI shows timestamp/Unknown. Lease expiry fences new gateway writes, not proof the host is dead.                                                  |
| Effect times out            | Use provider idempotency key/receipt where supported; query known state before retry. Without reconciliation, outcome remains unknown and human must decide. Never automatically repeat a send or purchase.                                  |
| Cancel or deny              | Void queued approvals and fence new effects before requesting upstream stop. No further approval while canceling. Late output is retained separately. Compensation for completed effects is a new reviewed action.                           |
| Runtime retry               | Safe transient reads get bounded backoff/jitter. New execution attempt records prior checkpoint and failure; non-idempotent work cannot restart only because a timeout elapsed.                                                              |
| Coordinator crash           | Recover persisted transaction/log and outbox, reconcile acknowledged versus unknown actions, then reissue bounded leases. Crash-after-save tests must not lose acknowledged work.                                                            |
| Replay or hostile transport | Authenticate/pair endpoint, validate origin/audience and schemas, bind commands to run/policy epoch, reject expired/replayed action IDs, rate-limit and cap payloads. Localhost alone is not authentication.                                 |

Budgets include elapsed time, model/token allowances if observable, tool limits and concurrency. Unknown provider billing cannot be promised as a hard cap. Default zero additional spend/local-only mode; connecting a subscription-backed remote harness is a separately disclosed mode, not free API access or local privacy. Suspended/idle hosts mean no always-on work.

No arbitrary shell is exposed to plugins, artifacts or generic agents. A vetted first-party runtime adapter may launch a specifically approved executable with structured arguments under a restricted environment; that is distinct from giving a plugin an eval/command string. If a harness only supports unrestricted independent effects, certify a weaker mode or reject it rather than advertise control it cannot enforce.

## 12. Calendar and capacity planning

| Concept                 | Semantics                                                                                                                                                                                             |
|-------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Task due date           | Date-only or exact deadline with zone. Calendar marker, not availability reservation. Changing it is an audited task mutation separate from moving a block.                                           |
| Human allocation        | One task can have multiple work intervals. Human availability/conflicts matter; estimated effort is not booked until user allocates it. Dragging a block never silently rewrites the due date.        |
| Agent capacity          | Binding concurrency, execution window and budget, not human free/busy. A planned agent slot indicates permission to attempt within a window, not guaranteed completion.                               |
| Recurring occurrence    | Stable rule/occurrence ID, IANA zone, wall-clock or fixed-instant intent, exceptions and skip/completion policy. Date-only is not midnight UTC; DST gaps/overlaps have explicit previewable handling. |
| External provider later | Read-only availability/import first; approved writes only after recurrence mapping, provider IDs, conflict detection and idempotency tests. Internal assignment does not invite attendees.            |

For example, the decision is due Friday while the human reserves two Thursday blocks and A can run overnight. Moving the second block to Friday changes allocation, not the deadline. If prerequisites slip, show the conflict and proposed alternatives; do not automatically rearrange other people's calendars.

Scheduled runs recheck grant freshness, dependencies, binding, budget and lease at trigger time. Missed windows default to review, not a burst of catch-up executions. New recurrence creates bounded occurrences, not an unbounded task explosion. MVP contains due-calendar and simple manual blocks; advanced recurrence, autonomous schedules and provider synchronization follow later reliability gates. This is not initially a complete calendar replacement.

## 13. Modalities and artifact safety

| Format                     | Initial versus later understanding                                                                                                                                                                    |
|----------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Markdown / plain text      | MVP edit, import/export, full-text index and block citations. Rich formatting remains deliberately modest. Code/math are displayable content, never executed.                                         |
| Images / PDF / HTML        | Initial file storage, hash/provenance and opening safe previews where supported. Text PDF extraction and page citations can follow bounded parser tests. HTML is inert snapshot, not an embedded app. |
| Scans / handwriting        | Later local OCR with language/confidence and correctable derived text. Stored image does not mean indexed text or reliable semantic understanding.                                                    |
| Audio / video              | Store originals before transcription. Later explicit jobs produce timestamped transcript/segments and provenance; diarization and visual understanding require independent quality tests.             |
| Structured files / imports | Versioned native archive highest fidelity. CSV/Markdown imports report unsupported fields. Notion formulas/rollups, plugin-specific syntax and recurrence do not magically round-trip.                |

Distinguish stored, previewable, extracted, indexed and understood. A scanned PDF may be stored but not searchable. A transcript may be searchable but inaccurate. Derived content records parser/model version, language, source hash and compute mode; original remains available for verification. Citations use page, image region or media timestamp, not just a filename.

HTML preview must isolate origin, disable scripts/network/forms/popups/downloads and parent access, sanitize content and neutralize tracking resources. Links open only through explicit safe navigation. PDF active content stays disabled; parsers get size/time/resource limits. Never execute imported macros, plugin code or downloaded artifacts. Preview safety is a maintained engineering boundary, not a one-time sanitizer checkbox.

Local processing avoids automatic external disclosure but consumes battery and may perform poorly on accents, handwriting or layout. Paid/cloud inference is never default; it needs provider/data/cost consent. Recording/transcription also needs lawful participant consent. ‘All modalities’ is the destination, not the initial release promise.

## 14. Plugin platform boundaries

First-party core owns task/time semantics, permissions, sync/recovery, approval UI, execution ledger and default renderer. Plugins extend views, imports, retrieval/knowledge-base adapters and bounded automation; they cannot replace authority or bypass review. Runtime-control adapters start first-party because they cross the most dangerous boundary.

| Contract                 | Design constraint                                                                                                                                                                                               |
|--------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Versioned capability API | Semantic versions, explicit host/capability requirements, schema namespacing and documented deprecation window. Internal database/DOM is not the extension API.                                                 |
| Isolation                | Restricted runtime plus sandboxed declarative UI, no Node/host filesystem, arbitrary shell, remote code loading or unrestricted network by default. Worker threads alone are not a security sandbox.            |
| Data access              | Scoped object handles through the same policy gateway. Request explicit file selections and allowlisted destinations; no raw SQL or whole-index access. Privilege changes require renewed consent.              |
| Quotas / scheduling      | Bounded memory, CPU, output, object pagination and background jobs; suspend/terminate abusive extensions. Limits must be measured on desktop/mobile rather than copied from a server runtime.                   |
| Lifecycle                | Pinned package hash, publisher attribution, permission diff before update, safe mode/disable and rollback. Signing proves attribution/integrity, not trustworthy behavior or absence of malicious dependencies. |
| Schema migration         | Dry-run versioned migration, transactional backup/rollback and user-owned data export. Uninstall stops jobs/revokes grants; user chooses preserve/export/remove plugin data.                                    |

No marketplace MVP. Prove the API with a few first-party extensions, then independent developers. A restrictive system is safer but less powerful and harder to port existing Obsidian plugins into. Mobile app-store/background constraints require a supported subset, not parity by assertion. A marketplace creates ongoing review, dependency and incident-response obligations; it is not free product breadth.

## 15. Architecture, sync and device trust

```text
Local client and selectable views      Task/tree · notes · due calendar · review
        │
Policy + command gateway               Deterministic scope, revisions and approvals
        │
Transactional object store + event log Durable state, history, snapshots and blob manifest
        │
Derived index + context engine         FTS · bounded graph · optional embeddings
        │  scoped packs
Single execution coordinator           Claims, fencing, outbox and checkpoints
        │  authorized dispatch  ▲ events / drafts / effects
Certified adapters → external runtimes Actual model/tool execution

Encrypted sync and backup relay        Later devices; ciphertext by default (encrypted changes)
```

*Diagram reconstructed as text from the original page's figure; the edge labels are the original's (authorized dispatch, events / drafts / effects, scoped packs, encrypted changes), their placement is approximate.*

Recommended initial storage is SQLite-style transactions, full-text search, indexed adjacency tables and an append-only audit/change log with materialized objects. File/blob vault stores large immutable content by hash; introduce cloud object storage only for managed sync later. Pure file authority simplifies manual edits but complicates IDs, atomic task transitions and permissions. A dedicated graph database or vector service adds deployment burden before scale warrants it.

Local-first editing and centralized execution are deliberately asymmetric. Initial authority lives on one desktop. Later an offline phone can capture a note or propose assignment, but execution-critical changes remain pending until authority validates them. Show local saved, accepted and synced states separately. Conflict on assignee, deadline, scope or approval cannot silently last-writer-win.

For later multi-device text, compare proven CRDT merge with explicit conflict review; CRDTs merge text, not intent. Control/permission log remains serialized. Coordinator transfer requires leadership/fencing and reconciliation; split-brain execution is not solved by a sync library. No bespoke encryption or claimed new consensus protocol is needed.

E2EE envelope design should encrypt object content, attachments and backups with device/user-held keys and minimize relay metadata. Key distribution, recovery and rotation require specialist review; do not claim E2EE also permits server plaintext search. Headless user-owned agents need keys on their host; hosted agents need approved scoped plaintext or an explicitly trusted key-holding environment. Browser-delivered decryption code adds server-update trust.

Protect local index/cache at rest and lock with device keys where feasible; an unlocked compromised endpoint can read data regardless of sync encryption. Independent encrypted backup snapshots, checksums, retention and restore drills are mandatory. Lost recovery material may mean permanent loss. Long-term platforms: desktop macOS first provisionally, then Windows/Linux, mobile capture/review on actual pilot OS mix, web last with clear local-runtime bridge limitations.

## 16. Performance contract

All figures are unmeasured targets. Reference baseline: Apple M2, 16 GB RAM, SSD, pinned supported OS/release build; 10,000 tasks, 50,000 note blocks, 100,000 run events and 10 GB attachment bytes, with completed local indexes. Validate additional larger corpora and Windows/Linux reference devices before expanding claims.

| Operation                | Initial p95 target                                                                                                                                                                                                      |
|--------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Input / local navigation | Ordinary-note input-to-paint under 16 ms; cached task/note open under 100 ms. IME, accessibility and long notes tested separately.                                                                                      |
| Search / views           | Keyword first results, filtered task tree and due-calendar under 150 ms. Paginate/virtualize; don't render all graph nodes.                                                                                             |
| Start / persistence      | Cold start to editable local work under 1 second; acknowledged local mutation under 100 ms; local claim decision under 250 ms, excluding model/tool runtime.                                                            |
| Context retrieval        | Built-index deterministic pack under 300 ms; optional semantic/reranking path under 1 second before any slow summarization. Never block writing on a pack job; timeout to simpler retrieval or explicit pending status. |
| Memory / scheduling      | Candidate idle client budget under 500 MB excluding optional model runtime. Indexing/parser/plugin workers have backpressure and yielding; no embeddings on keystroke.                                                  |
| Later sync               | Small accepted edit visible online within 2 seconds at 100 ms RTT/20 Mbps, excluding initial/blob sync. Offline reconciliation progress visible; no blanket bound for large attachments.                                |

Benchmark cold and warm states, empty cache, first import, index rebuild, background ingestion, maliciously large artifacts, busy plugins and power-saving mode. Record p50/p95/p99, sample counts, errors, dropped frames, CPU, peak memory, battery and durability. At least 30 cold and 100 warm trials per deterministic scenario; publish hardware/corpus and distribution rather than a cherry-picked demo.

Allocate UI priority, make context/semantic tasks cancellable, incrementally index changed blocks and lazy-load graph/media. Optional expensive models may miss pack targets; label actual latency rather than quietly changing the workload. No comparative ‘blazing fast’ claim exists until identical-workload measurements support it.

## 17. Competition and defensible IP

Research observed on 7 November 2026 (AEDT): Linear documents visible Agent Sessions and plan preview \[S17–18\]; Plane documents task hierarchy, dependencies, linked pages, views and scoped native agents \[S19–20\]. Notion already promotes shared MCP context \[S2\]. Obsidian, Amplenote, Anytype, AFFiNE and Tana Outliner cover substantial notes/planning/agent-access territory \[S3–12\]. These are documented, not independently tested, and we do not claim absent features merely because a page omitted them.

Substitutes also include staying in a coding-agent session/dashboard, using a normal task tool plus documents, or assembling memory/context infrastructure. Category tradeoff: less setup and no migration versus our proposed shared evidence/control layer. No unresearched dashboard or memory vendor feature is asserted here. Generic harness/context references are not the Harness or Context.dev services; no herdr feature set is assumed.

| Candidate asset                 | Copyability and accumulation                                                                                                                                                                      | Protection and falsification                                                                                                                                                                         |
|---------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Permission-aware context engine | Pipeline outline and ranking ideas easy to copy. Harder accumulation: measured constraint coverage, source invalidation correctness, adversarial cases and tuned local ranking across real tasks. | Keep implementation/tuning/eval assets confidential where practical; protect original code, not the idea. Fails as a moat if ordinary hierarchy/search or top-k retrieval matches outcomes cheaply.  |
| Portable execution record       | IDs, DAGs, checkpoints and leases are familiar patterns. Valuable accumulation: conformance tests, recovery edge cases and integrations users trust across exact runtime versions.                | Publish interoperable record spec if useful; retain quality implementation/testing. Fails if most work remains in one harness or destination cannot honor control semantics.                         |
| Evaluation corpus / corrections | Raw private data is not ours to pool. Harder asset: licensed curated task/failure cases, explicit consent, high-quality adjudications and reproducible held-out tests.                            | Track provenance/access/retention; user-local personalization default. Fails if labels are noisy, leak test answers, don't generalize or users decline participation.                                |
| Workflow / ecosystem / brand    | Views and approval screens easy to copy. Reliability, support, useful migrations and developer adoption compound slowly; network effects are not automatic.                                       | Original design/code/brand protection plus execution. Win switching through usefulness and open exit—not trapped data. Fails if incumbents bundle equivalent reliability and distribution dominates. |

Proposed ownership strategy: retain a proprietary engine implementation and confidential evaluation/tuning assets while documenting open data/export and adapter interfaces. This is not contradictory, but shipping local binaries limits secrecy and reverse-engineering resistance. Patent disclosure and trade-secret protection are different strategies; publishing an algorithm may defeat secrecy while a patent may be expensive, narrow or unavailable.

IP Australia's guidance distinguishes copyright in specific code/expression from ideas/methods, and warns that an app using generic computers for a business idea is not thereby patentable \[S21\]. No novelty, inventive step, patentability, authorship or freedom-to-operate conclusion has been established for this proposal. We would not claim ownership of MCP, task DAGs, vector stores, standard leases, context compression in general or ‘humans assigning tasks to agents’.

Before public technical disclosure, a qualified attorney should assess specific experimentally demonstrated technical contributions, prior art, jurisdiction and ownership—not this broad concept alone. IP Australia advises secrecy before filing and notes grace periods differ internationally \[S22\]; do not rely on a grace period as a disclosure strategy. Contributor agreements, employment/contractor assignments, dependency/model/dataset licences and provenance logs need review. AI-assisted output does not by itself settle ownership. This is product/IP strategy, not legal advice or a guarantee.

## 18. MVP, milestones and economics

| Milestone                          | Deliverable and risk retired                                                                                                                                                                                                                                             |
|------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| M0 · authorized feasibility later  | Two pinned harness control/access contracts, small task kernel and deterministic pack baseline; fault-injection harness and threat model. Retire unsupported runtime-control assumptions before UI/platform breadth.                                                     |
| M1 · thin standalone private slice | One human, multiple agents, one desktop OS; task list/tree/dependencies, Markdown notes, due-calendar/manual blocks, approvals, artifacts, bounded index/packs, run events and export/restore. Two real tested connectors required for acceptance, not mock-only badges. |
| M2 · evaluated pilot               | Held-out task comparisons, permission/recovery tests and real workflow observation. Prove less coordination burden and source-faithful outcomes before optimizing proprietary ranking.                                                                                   |
| M3 · portable desktop / sync       | Windows/Linux checks and encrypted second-device capture with single run authority; migration dry-run reports and restore rehearsal. Retire sync/identity conflicts, not just add a cloud checkbox.                                                                      |
| M4 · people, mobile and ecosystem  | Multiuser/guest review after isolation tests; phone capture/review, then web. Add external calendars, scheduled runs, richer safe previews and restricted plugins only through separate gates.                                                                           |

Explicit MVP exclusions: realtime collaborative rich editor, public marketplace, arbitrary shell, autonomous cross-agent sends, advanced recurrence, full provider calendar replacement, audio/video understanding, enterprise governance and every-format migration. The schema preserves the vision; it does not make implementation cheap. A robust platform requires runtime/security, storage/sync, editor/UI, mobile and operations expertise—not a credible few-day estimate.

Local-only remains useful with no subscription. Monetization hypothesis: paid managed encrypted sync/backup, optional reliable coordinator hosting and later team governance/support; do not charge merely for each agent identity. No context resale, ads, compulsory AI subscription or default model markup. Optional compute charges require explicit provider/data/budget consent. Remote subscription-backed execution is still remote disclosure.

Cost drivers are compatibility maintenance, security reviews, storage × history × replication, egress, restore support, headless uptime and mobile engineering. For planning only, model light and heavy workloads: 1 GB versus 20 GB active attachments, multiple retained snapshots and repeated new-device downloads. These are illustrative scenarios, not customer behavior, prices or forecasts.

Test willingness-to-pay with a specific later proposition rather than assume a price. Candidate sustainability gate: at least 70% modeled contribution margin after direct infrastructure and routine support at the chosen heavy-user tier, stress-tested for retention/egress. It is not a forecast and excludes much development overhead. If reliability cannot be funded without selling private context, change the business model, not the privacy promise.

## 19. Evaluation, kill gates and recommendation

Evaluate the whole workflow, not just retrieval. Baselines: ordinary selected documents, hierarchical index+full-text search, simple top-k semantic retrieval, then proposed context engine; compare actual project coordination against the user's current task/notes/harness workflow and documented incumbents. Use the same inputs/models/budgets where possible; separate model improvements from workspace improvements.

| Gate                   | Proposed acceptance, not results                                                                                                                                                                                                                                                                               |
|------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Task usefulness        | 30–50 project tasks across research/planning/review; human-adjudicated acceptance, correction burden and source-grounded success. Pre-register critical constraints; no critical omission allowed in release set. Report task-type failures, not only averages.                                                |
| Efficiency             | Candidate median total input-token reduction at least 30% against full selected context with no worse accepted outcomes; include discovery/tool/citation overhead, number of calls and latency. Smaller packs alone do not pass.                                                                               |
| Recovery / portability | Two exact harness versions pass claim, dispatch, revoke, stale-input, cancel, duplicate-event and handoff tests. Crash-after-ack loses no acknowledged work. Unknown effects block unsafe retries; export restores IDs/links/artifact hashes.                                                                  |
| Trust                  | At least 200 adversarial cases covering hidden metadata, links, moved tasks, stale summaries, poisoned notes, cross-user caches and changed-diff approvals. Zero unauthorized read/write/send in suite; passing is not proof of complete safety.                                                               |
| Human adoption         | Proposed eight directional interviews: five demonstrate real recurring coordination friction; six later complete assignment/tree/review unaided; five voluntarily use one real project for two weeks. At least two accept a specific paid-reliability proposition. Small samples do not establish market size. |
| Performance            | Meet declared p95 targets under background indexing, publish full distributions and enforce evidence/freshness despite slow compute. A fast incorrect or unauthorized pack fails.                                                                                                                              |
| IP / learning value    | Hold out projects, source collections, time periods and some task types. Compare personalized/learned ranker to fixed rules on untouched data. Do not train on test corrections; track consent/licences and evaluate poisoning/overfit.                                                                        |

Adjudication needs independent human criteria, not only an LLM judge or the same agent scoring itself. Reviewers record omissions, unsupported claims and acceptance rationale; blind method labels where practical. Synthetic security/recovery fixtures supplement rather than replace real workloads. Keep a sealed held-out set and publish methodology/limitations without disclosing private user data or confidential invention details.

Sharpest risks: this may become another place to manage agents, not less management; approval fatigue may encourage unsafe defaults; evidence can look rigorous while being wrong; and external runtimes may not support the isolation/control promised. Incumbents have distribution and can copy visible features. Local-first/E2EE constrains always-on hosted execution and global learning. Broad modality/plugin/platform support can consume the team before the core improves outcomes.

Kill or narrow if current tools match accepted outcomes with lower setup; users cannot distinguish assign/start/approve; two runtimes cannot safely hand off; learned ranking adds no held-out benefit over hierarchy/search; or reliability and support economics fail. A failed proprietary-engine hypothesis need not kill a useful workspace, but it removes a key defensibility argument. Do not preserve a complicated engine merely to claim IP.

| Three later choices      | Recommended default                                                                                                                                                                                       |
|--------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Initial scope / platform | Thin standalone workspace, one human and two tested bindings, provisionally macOS. Confirm actual pilot devices later; no six-platform launch.                                                            |
| Authority / review       | Single local coordinator, offline draft capture, human-approved decomposition/delegation/sensitive effects. No unsupported universal cancellation or implicit autonomy.                                   |
| IP / learning boundary   | Proprietary implementation/evals, open portable data/interfaces, user-local learning and explicit separate research consent. Attorney assessment before public invention disclosure; no patent claim now. |

Recommended first engineering validation—only if later authorized.

Test one six-task mixed human-agent project through two pinned runtimes, an approval-bound effect stub, forced failure/handoff, source invalidation and a 2,000-token constraint-preserving pack. Compare against plain project search before building richer UI or learning. This recommendation is not a request or permission to start.

Current status: detailed product specification complete; conditional PASS for validation, full build withheld. No prototypes, code, installs, services, purchases, agent contacts or RAG work occurred. Research ledger preserves earlier evidence and this revision's source/IP notes; the canonical product write-up lives here. No decisions are requested now.

## Sources and evidence limits

Primary documents S1–S20 were opened during the earlier research/revision on 7 November 2026 (AEDT); S21–S22 were opened for this revision. Vendor capability statements are documented, not independently tested. The MCP 2025-11-25 tools page is a versioned baseline, not a claim it is newest. Living A2A/host docs require version checks before implementation. No account setup instructions were executed.

- [S17 · Linear Agent Sessions, activities, plan preview](https://linear.app/developers/agent-interaction)
- [S18 · Linear stop/other agent signals](https://linear.app/developers/agent-signals)
- [S19 · Plane work items, hierarchy, dependencies and views](https://docs.plane.so/work-items/overview)
- [S20 · Plane native agents and scope/availability](https://docs.plane.so/ai/agents/overview)
- [S13 · MCP tools · 2025-11-25 baseline](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)
- [S14 · A2A specification](https://a2a-protocol.org/latest/specification/)
- [S15 · Claude Code MCP](https://code.claude.com/docs/en/mcp)
- [S16 · Codex MCP · living redirected docs](https://developers.openai.com/codex/mcp/)
- [S1 · Notion offline scope](https://www.notion.com/help/use-pages-offline)
- [S2 · Notion MCP/shared context](https://www.notion.com/help/notion-mcp)
- [S3 · Amplenote plugins](https://www.amplenote.com/help/developing_amplenote_plugins)
- [S4 · Amplenote notes/tasks/calendar API](https://www.amplenote.com/help/developing_amplenote_plugins/app_interface)
- [S5 · Amplenote security design](https://www.amplenote.com/help/amplenote_security_design)
- [S6 · Obsidian plugin security](https://obsidian.md/help/plugin-security)
- [S7 · Obsidian Sync security](https://obsidian.md/help/sync/security)
- [S8 · Obsidian official CLI](https://obsidian.md/help/cli)
- [S9 · Anytype agent skill](https://doc.anytype.io/anytype/features/integrations/anytype-agents-skill)
- [S10 · Anytype encryption/indexes](https://doc.anytype.io/anytype/data/privacy-and-encryption)
- [S11 · AFFiNE scoped MCP](https://affine.pro/mcp)
- [S12 · Tana Outliner local/hosted MCP](https://outliner.tana.inc/learn/features/local-api-mcp)
- [S21 · IP Australia: computer inventions and copyright](https://www.ipaustralia.gov.au/patents/what-are-patents/what-computer-related-inventions-can-be-patented)
- [S22 · IP Australia: secrecy, prior art and filing considerations](https://www.ipaustralia.gov.au/patents/how-to-apply-for-a-standard-patent/what-to-consider-before-applying-for-a-patent)

Research ledger: /workspace/people/K/products/agent-notes/product-definition-2026-11-07.md. All numeric budgets, SLOs and gates are proposed; none are benchmark results. No prior-art search, patent opinion, security audit or freedom-to-operate analysis has been completed.
