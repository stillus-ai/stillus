# Development

[Back to README](../README.md) · [Contributing](../CONTRIBUTING.md)

## Toolchain and commands

Use the root Makefile for project commands. Rust checks, tests, linters, audits,
benchmarks, and Git inspection run in the Docker Compose `toolchain` service.
Docker with Compose and a running engine are required. The Dockerfile pins
Rust 1.88.0 and the audit tools. MinGW-w64 and the `x86_64-pc-windows-gnu`
target support Windows cross-compilation on both macOS and Linux hosts. Named Docker volumes retain build and download
caches between runs.

Native Apple Silicon operations run through `make build-macos` (also available
as `make build`), `make native-smoke`,
and `make native-external-smoke`. They require macOS, Xcode Command Line Tools,
and `python3`. The build installs pinned Rust under `.host-build/`, uses the
system Xcode SDK, and leaves global Rust directories and shell profiles alone.
`native-external-smoke` requires an already built bundle.
CI also provides `make NATIVE=1 SOURCE_REVISION=<HEAD-SHA> native-check` without
Docker. See [GitHub CI](ci.md) for runners, caches, artifacts and validation.

| Command | Purpose |
|---|---|
| `make help` | Reminder of aggregate check selection |
| `make ci-validate` | Pinned actionlint and merged CI Compose validation |
| `make NATIVE=1 SOURCE_REVISION=<HEAD-SHA> native-check` | Pinned macOS build and native smoke checks without Docker |
| `make status` / `make diff` / `make log` | Inspect the checkout, changes, and recent commits |
| `make image` | Build the pinned Docker toolchain image |
| `make test` / `make test-release` | Workspace Rust tests in debug / release mode |
| `make test-<crate>` | Focused tests for crates with a Makefile target |
| `make lint` / `make fmt-check` | Clippy / formatting check |
| `make fmt` | Apply Rust formatting |
| `make ui-click-<scenario>` | Diagnose a particular UI acceptance scenario |
| `make ui-check` | UI smoke tests and click-driven acceptance scenarios |
| `make audit` | Source boundary, dependency license/source, and RustSec checks |
| `make check` | Full gate, including container builds and UI checks, without a native macOS build |
| `make` | Full gate, native Apple Silicon build, then native external-file smoke |
| `make build-macos` / `make build` | Build release `dist/Stillus.app` and `dist/Stillus.dmg` on an Apple Silicon Mac |
| `make build-windows` | Cross-build and inspect a portable x64 Windows release in `dist/windows/x86_64/Stillus.exe` |
| `make test-windows-build` | Compile Windows test executables and package a PowerShell runner without requiring Rust on Windows |
| `make build-linux` | Build a stripped Linux release executable in `dist/linux/<architecture>/stillus` through Docker |
| `make build-linux-smoke` | Build and launch that Linux release executable under Xvfb in a temporary workspace |
| `make native-smoke` | Generate demo data, build, and launch the bundle in a temporary workspace |
| `make native-external-smoke` | Test Finder/Launch Services delivery to an existing bundle |
| `make cache-size` | Show Docker Cargo build-cache sizes by profile and platform |
| `make clean` | Remove Docker debug Cargo artifacts for Linux, Windows, and the macOS cross-check |
| `make clean-all` | Empty the Docker Cargo target volume, including release and UI artifacts |
| `make test-cache` | Verify cleanup on a disposable Docker volume |

Use focused targets while developing. Before submitting a change, run
`make ui-check` for UI changes, `make check` for the full container check, or
`make` to include the native build and external-file smoke. Each includes its
prerequisites, so separate build and check runs are unnecessary. After fixing
a failure, rerun the selected check.

### Build-cache size and cleanup

Local Docker dev/test builds use `line-tables-only` debug information: backtraces
retain filenames and line numbers without full type and variable information.
Incremental compilation remains enabled for fast rebuilds. CI retains its
separate settings, and native macOS builds are unchanged.

For full Docker debug information, override both profiles for the required run:

```sh
CARGO_PROFILE_DEV_DEBUG=2 CARGO_PROFILE_TEST_DEBUG=2 make ui-build
```

Changing debug settings requires recompilation and can leave old artifacts in
the target volume. Use `make cache-size` to inspect it and `make clean` once when
switching to the smaller defaults. This removes the top-level `debug` directory
and the `debug` directories under `aarch64-apple-darwin` and
`x86_64-pc-windows-gnu`. It retains release builds and UI acceptance artifacts.

Use `make clean-all` when the entire build cache is no longer needed. It removes
all contents of the `cargo-target` volume, including hidden files and UI
artifacts, while retaining the volume's mount point. Both cleanup commands
preserve Cargo registry and Git caches, benchmark data, packaged outputs in
`dist/`, workspaces, and the native `.host-build/` directory. They operate on the
fixed Docker target mount, regardless of a `CARGO_TARGET_DIR` override.

Run cleanup only between builds and tests, with no other process using the
volume. Cleanup is manual; it does not run after each command. The first build
after cleanup recompiles the removed artifacts, and later builds reuse the new
cache. `make test-cache` exercises the actual cleanup recipes on a separate
anonymous Docker volume that is removed with its test container.

## UI acceptance

UI acceptance drives an ordinary Floem window through XTEST in disposable
workspaces under Xvfb. It covers editing, autosave, recovery, conflicts, search,
settings, protected notes, and RSS. Visual checks examine transitions, text
visibility, and hover/tooltip surfaces without stored pixel-exact PNG baselines.

Up to two independent scenarios run in separate containers at once. Standard
and `test-utils` scenarios run in successive groups, with a build before each
group so the binary is not replaced during a test. The timing-sensitive caret
test runs separately before parallel groups. Other aggregate stages remain
sequential. Set `UI_JOBS=1` for sequential diagnosis or `UI_JOBS=2` for the default.

## Dependencies and native file opening

Floem comes from crates.io. Finder file opening uses `floem-winit 0.29.5` from
the public [Stillus fork](https://github.com/stillus-ai/winit/tree/macos/open/files).
The `macos/open/files` branch is based on upstream revision
`69fa86042d44e58c04e0fee71f171470ade45792`. It adds an `application:openURLs:`
handler and an ordered `take_opened_files()` queue.

The root Cargo manifest pins the fork's complete Git revision, and Cargo.lock
records the resolved dependency. Cargo downloads its source into its own cache;
there is no vendored copy in this repository. Updating the patch requires an
explicit revision and lockfile update. The default `make` gate includes native
tests for delivery of multiple files at startup and to a running application.

Keep Cargo.lock committed and retain `publish = false` for workspace packages.
The root workspace defines `GPL-3.0-only`, inherited by all project packages.
The license audit includes these packages even though they are not published
to crates.io. Dependencies retain their own licenses.

## Known dependency warnings

On **2026-09-05**, `make audit` completed successfully but `cargo audit` reported
**14 allowed warnings** for the locked dependency graph. A successful exit is
not a claim that these issues are resolved. Repeated versions count separately.

| Kind | Crates and locked versions | Advisory |
|---|---|---|
| Unmaintained | `bitmaps 2.1.0` | [RUSTSEC-2026-0247](https://rustsec.org/advisories/RUSTSEC-2026-0247) |
| Unmaintained | `im 15.1.0` | [RUSTSEC-2026-0248](https://rustsec.org/advisories/RUSTSEC-2026-0248) |
| Unmaintained | `im-rc 15.1.0` | [RUSTSEC-2026-0250](https://rustsec.org/advisories/RUSTSEC-2026-0250) |
| Unmaintained | `paste 1.0.15` | [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436) |
| Unmaintained | `rustybuzz 0.14.1`, `0.18.0` | [RUSTSEC-2026-0206](https://rustsec.org/advisories/RUSTSEC-2026-0206) |
| Unmaintained | `sized-chunks 0.6.5` | [RUSTSEC-2026-0251](https://rustsec.org/advisories/RUSTSEC-2026-0251) |
| Unmaintained | `ttf-parser 0.20.0`, `0.21.1`, `0.24.1`, `0.25.1` | [RUSTSEC-2026-0192](https://rustsec.org/advisories/RUSTSEC-2026-0192) |
| Unsound | `im 15.1.0`: aliasing violation in `OrdSet` insertion | [RUSTSEC-2023-0126](https://rustsec.org/advisories/RUSTSEC-2023-0126) |
| Unsound | `lru 0.16.4`: potential use-after-free in `LruCache::pop()` | [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253) |
| Unsound | `sized-chunks 0.6.5`: panic-safety issues, use-after-free / double-free | [RUSTSEC-2026-0255](https://rustsec.org/advisories/RUSTSEC-2026-0255) |

Dependency upgrades and an assessment of affected call paths are separate
follow-up work. Do not suppress these warnings to make a release appear ready.

## Benchmarks and generated assets

`make benchmark` compares Ropey and Lapce buffers; `make benchmark-editor`,
`make benchmark-viewport`, and `make benchmark-search` exercise the respective
project layers. These targets generate 10 MB, 100 MB, and 1 GB datasets in a
Docker volume and can take substantial time and disk space. They are separate
from the final quality gate.

`make demo-data` recreates the generated notes in `examples/demo-workspace`.
The ignored demo folder is for disposable data, not personal notes.

The vector source is `app/stillus/assets/stillus-app-icon.svg`; the bundle uses
`Stillus.icns` from the same directory. To regenerate the icon after editing it:

```sh
docker compose run --rm toolchain python3 -B tools/generate_app_icon.py \
  app/stillus/assets/stillus-app-icon.svg app/stillus/assets/Stillus.icns
```

Generate the ICNS in the Docker toolchain and commit it alongside the SVG.
`make package-macos-smoke` compares all decoded RGBA pixels, allowing at most four
single-level channel differences across the entire icon for ImageMagick's
arm64/x64 resize rounding. Dimensions and ICNS representations remain exact;
larger color changes or accumulated differences fail the check.
PNG encoding can differ between rendering toolchains even when the displayed
pixels are identical.

## Packaging and release limitations

### Windows

`make check` includes both `make build-windows` and `make test-windows-build`.
The former uses locked release compilation without `test-utils`, compiles the
icon/version/PerMonitorV2 manifest resources with MinGW windres, verifies PE32+
x86_64 and the GUI subsystem, and checks DLL imports recursively. System DLLs
remain supplied by Windows; any MinGW runtimes are copied with their notices.
Only Windows package files are replaced; macOS and Linux artifacts are retained.

`make test-windows-build` compiles the workspace tests for Windows with the test
features in a separate profile. The release application is never substituted
with a test-feature build. Copy the whole Windows directory to local NTFS and
run `tests/Run-Tests.ps1`; see [the Windows acceptance guide](windows.md).
A successful cross-build does not establish that the native tests passed.

Platform-specific persistence lives in `stillus-platform`. Windows file identity
uses `winapi-util = 0.1.11`, replacement uses `atomicwrites = 0.4.4`, and ACLs use
`windows-acl = 0.3.0`. All are pinned in manifests and the lockfile, and the
Windows target participates in dependency audits. Windows metadata snapshots
also hash file contents with bounded buffers because the pinned safe handle API
does not expose NTFS change time. Large-file polling performance needs to be
measured on Windows; memory usage stays bounded.

### Linux

`make build-linux` uses `cargo build --locked --release` for the application
binary with its normal features, without `test-utils`. It copies and strips the
result from the Docker Cargo cache into `dist/linux/<architecture>/stillus`.
The architecture is that of the toolchain container (`aarch64` or `x86_64`);
this target does not perform cross-compilation. Repeated builds replace that
generated executable while preserving the macOS bundle and other architectures.

`make check` includes `make build-linux-smoke`, which launches the copied
release executable under Xvfb with software rendering, a temporary home, and a
disposable demo workspace. The launch must exit successfully within 30 seconds.

The binary dynamically links against the Debian 12 toolchain's system libraries
and requires glibc 2.36 or newer on a compatible Linux desktop. Runtime support
includes X11/Wayland libraries, libxkbcommon, EGL/OpenGL for GPU rendering, fonts,
and an XDG desktop portal backend for native file dialogs. Inspect the actual
linked dependencies with `ldd dist/linux/<architecture>/stillus` on Linux.
System libraries are not bundled. This command does not produce an AppImage,
DEB/RPM package, installer, or desktop registration. Test GPU rendering, Wayland,
file dialogs, and the intended distribution before publishing binaries; retain
dependency license notices and provide the corresponding source.

### macOS

You can package an existing arm64 binary from a controlled macOS runner inside
Docker. Set `SOURCE_REVISION` to the actual source revision of that binary
(the value below is only a placeholder):

```sh
make package-macos \
  MACOS_BINARY=/workspace/artifacts/stillus-app \
  SOURCE_REVISION=0123456789abcdef
```

The packager checks for a Mach-O arm64 slice, refuses to overwrite an unknown
output, and records SHA-256, version, and source revision in
`Contents/Resources/release.json`.

Native builds seal the completed bundle with an ad-hoc signature. This protects
bundle integrity but does not identify an Apple-registered developer, so a user
must explicitly approve a downloaded release in **System Settings → Privacy &
Security → Open Anyway**. The Docker-only `make package-macos` target remains
unsigned because `codesign` is a macOS system tool.

After signing, `make build-macos` also creates `dist/Stillus.dmg` using the
system `hdiutil`. The compressed, read-only image contains `Stillus.app` and
an `Applications` link: open the image, drag the app onto Applications,
eject the image, and launch the installed app. DMG packaging does not add
Developer ID signing or notarization. A failed image creation or verification
preserves the previous DMG; the app bundle has already been built at that point.
Use `make package-macos-dmg` to package the existing `dist/Stillus.app` without
rebuilding Rust. `make package-macos-smoke` covers packaging and failure cases
in Docker; `make package-macos-dmg-smoke` creates and mounts a temporary image
from the existing signed bundle on macOS and verifies its contents and signature.

Before distributing a release without that manual step, complete native macOS
functional, APFS filename, Unicode/IME, accessibility, large-file UI, and
protected-note UX checks; address the open dependency warnings; prepare the
license notices and corresponding source; and complete Developer ID signing,
notarization, and a Gatekeeper smoke. Docker checks and a native launch smoke
alone do not establish release readiness.

## Platform feature audit

The application has equivalent features on its three existing targets. Platform
adapters remain where the OS interfaces or filesystem guarantees differ.

| Area | macOS | Linux | Windows |
| --- | --- | --- | --- |
| External files in the app / command line | Shared open pipeline | Shared open pipeline | Shared open pipeline |
| Desktop delivery | Pinned Finder callback adapter | Desktop Exec arguments; new process | Open With command arguments; new process |
| Registration | Bundle document types | Opt-in Register.py, XDG applications/icon | Opt-in Register.ps1, HKCU ProgID |
| Fatal error report | RFD + native clipboard | Separate GTK4 loop + GDK clipboard | RFD + native clipboard |
| Primary keyboard hints | Cmd | Ctrl | Ctrl |
| Editing, search, RSS, encryption, recovery | Shared implementation | Shared implementation | Shared implementation |
| File identity / permissions / commit barriers | Unix handles and modes | Unix handles and modes | Windows handles, ACLs, NTFS barriers |
| Home directory | HOME | HOME | USERPROFILE, HOMEDRIVE/HOMEPATH fallback |
| Native acceptance | Native smoke + Finder delivery | Xvfb UI and desktop smoke | Compiled test kit; native results recorded separately |

Linux crash UI uses gtk4 0.9.7 on its own initialized thread, without restarting
Floem or launching a dialog helper process. GTK4 runtime libraries are required;
file/folder dialogs retain the existing XDG portal backend. In headless mode a
fatal error writes the same redacted report and exits. Copy keeps the Linux
report dialog alive so a desktop without a clipboard manager can still paste.
The `--smoke-panic` synthetic worker failure exists only with `test-utils`.

`make ui-click-external` exercises both CLI and real GIO desktop delivery,
including package paths containing spaces, Unicode, quotes, dollar and percent
characters. The installed Exec uses `/usr/bin/env --` to avoid GIO's initial
executable lookup treating literal percent characters as unexpanded field codes.
The same acceptance starts two processes on one workspace, checks stale-save
conflicts, and verifies that subsequent edits preserve the losing window's
recovery without changing the winning file.
`make ui-click-crash` checks the GTK report, clipboard and headless fallback.
Both scenarios participate in the existing final aggregates. Windows registration
tests use a disposable registry subtree, not the real user associations.

The source audit found that other `cfg(unix)` / `cfg(windows)` branches implement
file identity, permission preservation, secure storage and sync barriers, or
provision OS-specific link fixtures. They must remain. Unsupported-platform
fallbacks exclude systems outside the three targets. Linux font-coverage tests
check the controlled Docker font set; native font/IME acceptance remains a
separate environment check. Apple Silicon packaging and Windows x64 targeting
are architecture boundaries, not missing application features.

No inter-process routing is introduced. Short reentrant operation locks serialize
checked note writes, recovery writes, security changes and startup repair.
Concurrent note saves still compare their original file versions under the lock.
Recovery stores remember the record they observed/wrote, so an unrelated window
cannot overwrite or automatically remove that record using its own revision
counter. Settings use three-way field merging and report competing edits.
Password rotation rechecks the encrypted target set while holding the operation
lock. Stale protection and recovery jobs reauthenticate against the current
workspace verifier before publishing ciphertext.

## Application commands

`application::Application` owns the active workspace and coordinates document
persistence, recovery, protection, RSS and search on one owner thread. Its
`poll` method consumes worker completions and starts due work; `next_deadline`
provides the next monotonic wake-up time. The native window schedules this
pump independently of settings pages and search visibility. Tests can replace
the clock, RSS executor and workspace loader without creating a window or
reactive scope. Workspace switching loads and restores the candidate in a
worker; the owner rechecks blockers before applying it. Failed and cancelled
loads preserve the current session. Loader completion uses a single bounded
message, and shutdown joins the loader and drains its prepared search worker.

Global AI, update and journal coordinators survive workspace changes. They own
their worker channels and retain results for the UI to project. The preferences
coordinator debounces writes, merges independent changes and flushes outstanding
settings at session handoff and shutdown. Window geometry, scrolling, focus,
clipboard handling, localization and password-entry widgets stay in the UI.
The UI receives a read-only workspace projection and idempotent view effects.
`make audit-source` checks this execution boundary.

`application::api` defines typed commands, queries, results and caller contexts.
JSON parsing and serialization for direct tool calls live in
`application::tools`; handlers delegate to the same application object.
The action catalogue records each handler and its tool availability, including
reasons for UI-only operations. See [the Rust API reference](rust-api.md).

Run `make test-actions`, `make test-core`, `make test-ai` and `make test-update`
for the action and domain regression gates. `test-actions` runs the fast app
unit tests, including native adapter regression tests and headless dispatcher
scenarios. Normal UI scenarios and `test-utils` scenarios must run in separate
Make invocations so that each group uses the matching application build.
The full aggregates are not prerequisites for this targeted workflow.

The additional `make test-rss` target runs the RSS crate's unit and local HTTP
fixture tests. It is useful when changing shared subscription metadata handlers.
Ordinary UI scenarios and those needing `test-utils` must use separate build
groups; do not reuse a feature-enabled binary for ordinary acceptance scenarios.
