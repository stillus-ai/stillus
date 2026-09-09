# Agent instructions for Stillus

## Getting started

1. Read [README.md](README.md) in full.
2. Run `git status --short --branch` and `git diff` on the host to inspect the
   checkout and user changes. Use `git log` only when history is relevant and
   the user's task permits it.
3. Work directly on the requested task without separate planning files or
   progress journals.
4. Do not overwrite unrelated user changes or include them in your change set.

## Product boundaries

- Stillus is a local native Rust/Floem application without a WebView, JavaScript,
  network services, or a database as the authoritative store.
- The only authoritative source of workspace notes is UTF-8 Markdown with YAML
  front matter in `<workspace>/notes/`. Preserve unknown fields, files, and
  directories.
- Opening or scanning a workspace must not rewrite notes.
- Saves and metadata operations must preserve atomic/no-overwrite,
  conflict/recovery, and bounded-memory guarantees.
- Protected notes leave YAML and filenames readable while storing the Markdown
  body as an authenticated age envelope. Body plaintext must not reach a
  persistent index, recovery record, cache, diagnostics, or temporary files.
- Project-owned Rust must remain safe-only. Do not add `unsafe`, SQLite,
  WebView/browser runtimes, JavaScript runtimes, or general process execution.
  The sole process-launch exception is `app/stillus/src/restart.rs`: after an
  installed update and an explicit Restart click, it may launch only the
  installed Stillus executable directly, with a bounded stdin handoff and no
  shell. Save conflicts and security operations must block restart.
  The only network exceptions are the restricted `ureq` HTTP/HTTPS client in `stillus-rss`
  (any host or port), the HTTPS client in `stillus-ai` (fixed OpenAI/Anthropic
  model catalog endpoints and the fixed OpenAI Responses endpoint only), and the
  HTTPS client in `stillus-update` (release
  metadata and release assets on a fixed GitHub host allowlist, redirects
  checked per hop); HTTP/HTTPS opening in the system browser is allowed only
  through the RSS crate's dedicated hardened opener. The test-only RSS and
  update HTTP integration targets may use a loopback server.
- Updates verify every downloaded byte against the published checksum list and
  the package manifest, and replace only the application's own installation by
  renaming files in place. An update must never write into a workspace,
  restart the application without an explicit Restart click, or install a
  release that an automatic check found less than 24 hours after publication.
- AI settings are global. API keys belong only in the OS credential store via
  `stillus-platform`; config files contain opaque references and model aliases.
  Never send notes while checking a key or listing models. Connecting creates
  the provider-specific `default` model alias.
  It can be edited but not deleted or renamed; missing aliases resolve through
  `default`. Existing unavailable selections must fail explicitly.
- `.stillus/` contains settings and potentially unsaved recovery work as well
  as derived caches. Never treat the whole directory as disposable. Preserve
  `.stillus_security/` and `.stillus_backups/`; see [storage documentation](docs/storage.md).

## Development and checks

- The root Makefile is the primary command interface. Rust toolchain commands,
  tests, linters, audits, and benchmarks run through the Docker Compose
  `toolchain` service. Run all Git commands directly on the host, never in
  Docker or through Makefile targets that run Git in Docker.
- On the host, use `git`, `make`, `docker`/`docker compose`, and file editing. Native
  Apple Silicon operations are exposed by `make build`, `make native-smoke`,
  and `make native-external-smoke`. Builds use pinned Rust in ignored
  `.host-build/` and the system Xcode SDK without changing global Rust or shell
  profiles. The external-file smoke uses host Python and an existing bundle.
- `make publish` runs its Python orchestrator and locally authenticated Codex
  on the host; it calls GitHub's REST API directly and runs Rust in Docker.
- Write or update tests with behavior changes. Do not weaken tests to hide a
  defect.
- Use scoped UI styles. Do not apply global theme/style overrides for local
  changes.
- Build shared controls through `app/stillus/src/ui/`: buttons, inputs,
  textareas, selects, secret-input surfaces, menus, tooltips and modal shells.
  Fix interaction and appearance in the component, never in a per-screen copy.
  Components receive values and callbacks, not application controllers.
- Every action button starts with an icon. Standard standalone actions use
  only their icon; custom actions retain text. Dialogs, menus and navigation
  retain their labels. Declare standard actions with `ButtonAction`.
- Every icon-only button MUST have a nonempty localized tooltip on hover,
  including unavailable buttons. Use the shared button's title argument;
  update it with the action state. Preserve keyboard-focus hints too.
  Use the component's enabled predicate for unavailable buttons; applying
  Floem `.disabled()` to an icon-only button or its wrapper suppresses hover dispatch.
- Use `TextArea` for form multiline input. Never insert placeholders into
  its document or implement a screen-specific caret/blink workaround.
  The bounded Markdown document editor and OS-owned dialogs are specialized
  exceptions; they must not become alternative form-control implementations.
- Keep `make audit-ui-components` and `make ui-click-components` passing.
- After changes, run fast unit tests first, then only tests related to the
  changed behavior. Select specific packages, test filters, and UI scenarios
  based on the affected code.
- Do not run `make`, `make check`, `make ui-check`, or other long full-suite
  checks unless the user explicitly requests them.
- After fixing a failure, rerun the relevant checks without expanding to a
  full-suite run.
- For documentation-only changes, review the diff and run `git diff --check`
  on the host; application tests are not required.
- If a full aggregate is explicitly requested, do not run `make ui-build`
  separately before an aggregate that includes it.
- Keep project license metadata and SPDX notices consistent with `GPL-3.0-only`.
  Preserve dependency license notices and do not bypass audits.
- After each completed task, create a local Git commit if all selected tests
  and checks passed. For documentation-only changes, the checks above suffice.
  Include only changes belonging to the task; do not commit if required checks
  failed or could not be completed.
- Push only when the user explicitly requests it. Creating a commit does not
  authorize a push.

## Finishing a task

1. Run the selected checks according to the rules above.
2. Review `git diff` on the host and confirm that original user changes are preserved.
3. If all selected checks passed, commit the task changes locally without pushing.
4. Report what changed, exactly which checks ran and their results, the commit
   hash (or why no commit was created), and any
   remaining failures or risks.
