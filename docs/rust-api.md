# Direct Rust application API

The API is in `app/stillus/src/application/`. It does not start an MCP listener,
HTTP server, chat loop or text-generation service. Note, settings, recovery and
request-journal formats are unchanged; no data migration is required.

Create an `Application`, supply global coordinators and workspace preferences,
and call `poll()` on the owner thread when `next_deadline()` is reached.
Native adapters call typed application handlers. The direct-call registry uses
`dispatch(Caller, Command)` and `query(Caller, Query)`; it does not execute
storage or network operations itself.

## Tool calls and sessions

Capture `ToolContext::capture(&application)` once for an assistant operation,
then pass that context to `tools::call`. Every call checks the captured
`SessionId` before reading or modifying state. A workspace switch by the user
invalidates old contexts. Calls never silently switch to the newly selected
workspace. Workspace initialization and selection are trusted UI operations;
there is no general JSON settings editor or workspace-switch tool. Workspace
loading and initialization return an accepted operation. A bounded worker loads
and restores the target; only `poll()` installs the prepared session, after
checking current persistence and security blockers again. A failed or cancelled
load keeps the current editor. The owner retains completion status across a
successful switch and rejects old tool contexts even if their status query
caused that switch to finish. Closing the application joins the loader and
releases its prepared search worker. The UI consumes `take_workspace_switch_result()`
before starting another switch and projects the restored preferences before
staging fresh UI settings. An initialization already writing finishes
safely; a plain load can be cancelled before it is applied.

`catalog::ACTIONS` is the source of tool names, descriptions and availability.
`tools::list()` adds argument schemas for the available handlers. Existing names
are retained, and new names use slash separators. The catalogue covers notes,
external files, editor history, category ordering, search, RSS, settings, AI,
protection, updates, journal access and operations. UI-only entries include
secret input, destructive confirmations and the mandatory explicit Restart
click. `security/disable` returns `RequiresUserInteraction`.

## Document identity and versions

Note and external-file IDs are opaque handles within the workspace session.
An application rename retains the handle without adding IDs to YAML or changing
storage formats. `external/open` requires an absolute path and preserves the
existing engine type, symlink and duplicate-file rules.

Read before changing an object. `notes/read` returns a bounded UTF-8 body slice
and an opaque version; it includes the open editor's unsaved buffer. It also
supports attached external document handles. `notes/update` replaces a UTF-8
byte range. Active-document edits pass through editor history and autosave;
inactive edits stream through versioned atomic storage without switching the
editor. Protected bodies are unavailable through tools, even if the native UI
has unlocked them. Unresolved recovery, active persistence and stale versions
are explicit failures. Note, AI and ordinary-settings read versions share a
256-entry session budget. `notes/restore` can restore an inactive plain document
in a worker using the same recovery validation and persistence as the editor;
it preserves the visible buffer and refuses a changed disk base.

RSS mutations use the corresponding subscription, read-state or filter version.
RSS filter arguments contain only `blacklist`, `whitelist` and their version;
unknown persisted fields are preserved but cannot be supplied through a tool.
Native and addressed RSS metadata use the same normalization, version checks
and timestamped engine write.

`catalog/categories` returns the catalogue version and current display ordering.
`catalog/order` accepts both note and RSS identifiers. `catalog/sort` saves a
category's name/created/modified sort direction, or removes an explicit sort
when `field` is omitted. `catalog/categories/order` saves category display order.
Both settings writes return tracked operations; the UI receives their projection.

`settings/read` returns a version and the public language/automatic-update fields.
Pass that version to `settings/locale` or `updates/automatic`. The write verifies
the changed field under the config lock and merges independent field changes.
No workspace path or other configuration field is writable through these tools.
Model-alias changes use a captured AI settings version. AI query results contain
provider, catalogue and aliases, with no credential references or pending
credential deletions. API keys and passwords are accepted only in trusted typed
commands, which do not implement JSON deserialization.

## Completion and servicing

Acceptance and durability are distinct. A changed buffer reports `saved: false`;
background work returns an operation ID. Poll `operations/status` to receive a
final status, or `operations/progress` for status plus phase, completed work and
an optional total. An addressed file write reports `Saved` with a separate
`reconcile_error` if the subsequent catalogue update failed. RSS refresh reports
its actual fetch and persistence outcome. Search results include addressable
note handles. Global AI and journal operations continue to finish when their
settings page is closed.

Only the owner applies operation-status changes. Worker messages use bounded
queues; progress may be coalesced, while final results are retained. The session
keeps the last 64 completed operations by completion order, in addition to at
most eight tracked active operations. Search can overlap other coordinators;
only one catalogue file write can be active.
`operations/cancel` cancels a queued write before the worker claims it. An
already executing write finishes safely. AI cancellation uses the coordinator's
cancellation flag, and the final result still comes from completion processing. Search shutdown drains its bounded
result channel while waiting for the worker, avoiding a full-channel deadlock.
Application shutdown also drains in-flight file and security writes before
releasing the workspace.

Journal access is paged. Clearing completed records stays behind the existing
confirmation; retrying journal persistence does not repeat the HTTP request.
See [storage details](storage.md) for retention and protected-content rules.
