<!-- Copyright 2026 Evgeniy Udodov -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->
# GitHub CI

[Development commands](development.md) · [Windows acceptance](windows.md)

`.github/workflows/ci.yml` runs for pull requests targeting `master`, pushes to
`master`, pushes to the moving `latest` tag, and manual dispatch. It does not change branch protection or PR merge
rules, publish Releases, or require a write-enabled repository token.
Every action is pinned to a complete commit SHA, checkout credentials are not
persisted, and repository permissions stay at `contents: read`.
A newer CI run cancels the older run for the same PR or branch.

| Job | Runner | Command and scope | Timeout |
| --- | --- | --- | --- |
| Linux | `ubuntu-24.04`, x64 | Build the Compose toolchain; `make ci-linux` executes `make check-linux` (tests, audits and Linux/macOS checks) and packages Linux | 120 min |
| UI acceptance | `ubuntu-24.04`, x64 | `make ci-ui` builds the application and runs `make ui-check` (UI smoke tests and click-driven acceptance scenarios) | 90 min |
| Windows build | `ubuntu-24.04`, x64 | `make ci-windows-build` cross-compiles the Windows application and test kit, then packages both | 90 min |
| macOS | `macos-15`, Apple Silicon | `make NATIVE=1 ci-macos` builds with pinned Rust and executes the native launch and Finder smoke checks | 90 min |
| Windows | `windows-2025`, x64 | Verify and unpack the Windows build job's test package from this run; execute its existing PowerShell test runner and native smoke checks | 30 min |

Linux checks, UI acceptance, macOS and the Windows cross-build start independently.
UI acceptance has no job dependencies or downloaded build artifacts: it waits
only for an available runner, its own toolchain setup and application build.
Native Windows tests wait only for the Windows build job; Linux UI failures do
not block them.
The local `make check` still includes the full gate. Its Windows build targets
are grouped under `check-windows-build`, while `check-linux` runs the remaining
non-UI checks. CI runs these groups and `ui-check` on separate runners without
duplicating the acceptance suite or Windows compilation. The UI runner compiles
its own debug application. `ci-package-linux` packages Linux only; `ci-package-windows` packages
the Windows application and test kit with the same source SHA.
Tests remain in Makefile and the shared test scripts, including desktop file
opening, multiple processes, localization, crash reporting, clipboard, recovery
and password-change acceptance. YAML contains orchestration rather than copies
of those tests. Windows uses the same compiled test list and native runner as
local Windows acceptance; manual GPU, IME and accessibility checks remain manual.

## Local validation and native commands

```sh
make ci-validate
make check
make diff
```

`ci-validate` runs actionlint 1.7.12 from its digest-pinned Docker image over all
workflows and checks the merged `compose.yaml` + `compose.ci.yaml`, including cache mounts and profile
settings. It is included in `make check`. Artifact privacy, permissions, transfer
integrity and source-revision validation have tests in `tools/test_ci.py`.

On an Apple Silicon Mac with Xcode Command Line Tools and Python, Docker is not
needed for the following commands. Supply the full SHA of the actual checkout:

```sh
make NATIVE=1 SOURCE_REVISION=<40-character-HEAD-SHA> native-check
make NATIVE=1 SOURCE_REVISION=<40-character-HEAD-SHA> ci-macos
```

`native-check` reuses `native-smoke` and `native-external-smoke`; the former reuses
`build-macos` and `demo-data`. `ci-macos` additionally writes sanitized reports
and a distributable archive. Empty, shortened, symbolic or mismatching revision
values fail before native compilation. In Actions, `SOURCE_REVISION` is
`github.sha`, checked against `git rev-parse HEAD`; for a pull request this is the
actual tested merge checkout rather than the PR head branch SHA.

Without `NATIVE=1`, existing commands retain their Docker-backed behavior and
local builds continue to record the automatic revision, including `-dirty`.
Native Rust and downloads remain inside ignored `.host-build/` with the existing
pinned Rust/rustup and system Xcode SDK. Global Rust and shell profiles are unchanged.
The macOS CI wrapper explicitly starts native Make through `arch -arm64`, so an
Intel Python installation running under Rosetta cannot change the build architecture.

## Caches and runner storage

All three Ubuntu jobs set `COMPOSE_FILE=compose.yaml:compose.ci.yaml`. Buildx Bake
reads those same Compose files, loads `stillus-toolchain:ci`, and restores/exports
Docker layers through the GitHub Actions cache backend. No image is pushed to a
registry. The local Compose configuration and its named volumes are unchanged.

Actions caches only downloaded Cargo registry and Git sources on Linux and
macOS. Keys include OS, architecture and lockfile/toolchain inputs. Cargo target,
incremental artifacts, native binaries, test workspaces, settings and credentials
are never cached across runs. Linux target remains a named volume on the
disposable runner; macOS target remains on that runner's local filesystem.
Before cache archiving, Linux returns ownership of `.ci/cargo` to the runner.
Some published crates contain owner-only source files; extraction by Docker's
root user otherwise makes those files unreadable to the host cache action.
The UI acceptance and Windows build jobs perform the same ownership cleanup and
use separate Cargo cache keys, with Linux sources as a fallback. The jobs share the Docker
toolchain layer cache but have separate working directories and target volumes.

Only CI disables incremental compilation and debug symbols for dev/test via
environment variables. Debug assertions, overflow checks, test features, release
tests, linters and audits retain their normal behavior. No Cargo profile in the
repository is weakened. Linux removes only the unused Android and .NET SDKs on
the disposable GitHub-hosted runner, reporting free space before cleanup, before
the gate and at job completion (including failure).

## Artifacts and diagnostics

Artifacts are retained for **one day**, the minimum supported retention period:

- `stillus-linux` and `stillus-macos`: `.tar.gz` archives preserving executable modes.
- `stillus-windows`: portable application ZIP, without the test executables.
- `windows-test-package`: separate ZIP containing that Windows application,
  runtime DLLs and the compiled test kit for the dependent job in the same run.
- `reports-linux`, `reports-ui`, `reports-macos`, `reports-windows-build`, `reports-windows`: status and cleaned diagnostics,
  uploaded also when checks fail or a run is cancelled after checkout.

Build archives include the project license, existing runtime notices where
applicable, `SOURCE_REVISION.txt` and a `build.json` file with per-file SHA-256
checksums. The Windows job rejects a different revision, changed bytes, unexpected
archive members, links and escaping paths before running any supplied executable.

Reports record the source SHA, platform, exit status, known check names and Rust
diagnostic source locations. Python failures retain test names, tool source
locations and exception types. The diagnostic filter excludes arbitrary test output,
panic payloads, thread names, editor text and temporary paths. Neither whole
workspaces nor screenshots, raw application logs, recovery files or caches are
uploaded. Native Windows CI reports omit temporary workspace/log paths and
exception payloads; the default local PowerShell runner still retains its usual
diagnostic workspace. The Windows kit runs every Rust test executable before
reporting failure; `windows-results.json` records failed test names, sanitized
diagnostics and the current test phase. It uses the same diagnostic filter as
the CI console, and does not upload raw test logs. CI logs are deliberately
reduced: reproduce a failing named check locally to inspect unrestricted fixture
diagnostics.

The Linux `search` acceptance scenario waits for painted result rows and a
stable result area that excludes the blinking input caret. Before typing an edit,
it requires the search controls to close and the expected note path to reach
workspace settings. It then checks the marker in that note's canonical file.
Editor readiness allows only the appearance or disappearance of the narrow
accent-colored caret; text changes, caret movement and selection overlays still
reset the stability interval. This remains valid when slow frame capture samples
opposite caret blink phases on every iteration, without extending the timeout.
Search diagnostics use fixed stages (`initial/index`, `query/results`,
`selection/open`, `selection/save`, `external/index`, `rebuild/index`, and
`final/validation`), without logging queries, paths or note contents. A failed
wait stops the scenario; neither selection nor text entry is retried.
Search steps in the secure scenarios also wait for painted, stable results,
using only in-memory color counts and image signatures, without screenshot files.

The `rss_filters` scenario checks field focus through Copy and exact text values,
without sampling caret pixels or blink phases. It clears stale clipboard content
and retries only Select All / Copy while focus settles. Distinct multiline drafts
in both fields must survive focus changes, cancel correctly, and save together
without an AI connection. The scenario distinguishes Save from Save and Apply,
checks invalid regexp rejection, whitelist exceptions, revealing previously
hidden entries, and navigation past hidden articles.

The `rss_cards` scenario is a basic open/read smoke check with one cached article
restored at startup. After first paint, one title click must open the expected
URL through a local browser stand-in and persist the read marker. Browser calls
use atomic records, and both results are checked again after normal shutdown.
The scenario does not compare screenshots, text colors, card heights, hover
frames, or a fixed Tab sequence. Markdown presentation and URL validation have
Rust unit tests; feed keyboard navigation has its own `rss_keyboard` scenario.

Native test kits also emit fixed-vocabulary `NATIVE_IO`, `NATIVE_SAVE`,
`NATIVE_OPERATION`, `NATIVE_CLEANUP`,
`NATIVE_TEMP`, `NATIVE_RESULT`, `NATIVE_ASSERT` and `NATIVE_PATH` records.
I/O failures distinguish lock creation, validation, opening and acquisition;
metadata opening, inspection, permission capture, hashing and cursor restoration;
atomic replacement; and temporary cleanup. Records contain only known operation,
stage and error-kind labels, numeric OS codes (zero when unavailable), cleanup
outcomes, temporary counts and boolean path-comparison results. They never contain
error messages, path strings or file contents. Malformed records are rejected
before the legacy diagnostic extractors, and filtering the records again is safe.
The same records survive in `checks.log` and `windows-results.json`.

The Windows runner requires the same responsive window and persisted selection
continuously for 500 ms, within one monotonic 60-second deadline. It then requests
a normal close within a shared 30-second shutdown deadline. For up to five
seconds it retries only requests that Windows has not accepted; after acceptance
it only waits for the process to exit. Startup uses a synthetic note so that a
selection change actually requires settings to be saved. External-file launch checks both paths
in order, the selected file, and unchanged contents after closing. These are
launch/state smoke checks, not visual UI acceptance.

Each Rust test executable has a ten-minute limit. Failed and timed-out
executables remain failures while the remaining executables still run.
`windows-results.json` is checkpointed after each executable and adds
`durationMs`, `stage`, `reason`, and `smokeChecks`; a timed-out executable has
no invented exit code. `NATIVE_RUNNER` console records contain only fixed stage
and reason labels and a numeric duration. `process/close/rejected` at
`close/request` means no request was accepted; `process/exit/timeout` at
`close/wait` means an accepted request did not finish within the deadline.
`process/exit/code` retains the actual nonzero exit code in the JSON report.
`NATIVE_WINDOW` records the last process/window/responsiveness state, whether
close was accepted, and the attempt count before forced cleanup.

Native smoke launches opt into `STILLUS_NATIVE_DIAGNOSTICS=1`. Fixed-vocabulary
`NATIVE_LIFECYCLE` records show entry into `WindowClosed`, success or failure of
the close-time settings flush, return from the event loop, the final settings
flush, and the end of main's explicit shutdown code. `ShutdownComplete` precedes
the remaining local destructors; the runner still requires actual process exit.
Both application output streams are collected after cleanup, including on
failure; only sanitized records enter the CI console
and each smoke check's `diagnostics`. A collection failure is recorded separately
and never replaces the original smoke failure. Raw stdout/stderr remain local.
The checksummed test package includes `windows_test_support.ps1` and its
standalone behavior tests, which run before the native kit. They exercise
delayed readiness, shared deadlines, early exits, closing/cleanup, continued
execution after failure, and real process exit-code/output collection.
No failed scenario is retried automatically.

Rust captures these diagnostics with each test, so successful tests remain quiet
and failed tests retain their diagnostic context. The platform instrumentation is
enabled by `test-utils` (included in the native kit's `--all-features` build);
ordinary release builds do not emit it. The opt-in lifecycle records above are
also available in the packaged release executable. A failing first-use lock test
identifies creation/acquisition failures separately from save conflicts. A Windows rerun is
still required to establish which remaining native failures are resolved.

UI acceptance failures additionally emit `UI_ACCEPTANCE_DIAGNOSTIC` lines to
the Actions console and `checks.log`. They retain the scenario, a fixed stage,
an allowlisted exception type (or `Exception` for an unknown type), and known
test-tool source paths with line numbers. `password_change` distinguishes setup,
protection, settings validation, clipboard checks, rotation, backup verification,
restart, old-password rejection and new-password unlock. These lines are flushed
before screenshots or cleanup; failures in those operations are reported separately
and do not replace the original failure. No exception messages, source-line text,
command arguments or local variables are included, including in local UI failure
output. Existing `UI_ACCEPTANCE_PASS`/`UI_ACCEPTANCE_FAIL` markers and exit codes
remain unchanged. This diagnostic addition does not establish the cause of an
earlier failure whose report omitted those details.

## Verify the first GitHub runs

After pushing the workflow commit, open **Actions → CI**. A first successful run
must show cache misses/build steps, all four jobs passing, and the nine named
artifacts above. Download the archives before they expire and inspect their
source SHA, license and checksums; the Unix executable bits are inside the tar
archives. Use **Run workflow** on the same branch to create a second run, verify
Cargo cache hits and cached Docker layers, then verify all jobs and artifacts
again. A cache hit alone is not proof that checks executed.

Record links and conclusions for both runs when they exist. Local checks and a
Windows cross-build do not establish a passing GitHub-hosted Windows run.
If repository or organization policy disables Actions or blocks the pinned
actions, the repository owner must enable the CI workflow/allow those actions.
No change to branch protection or workflow write permissions is needed.
