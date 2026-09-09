# Publishing a release

Run `make publish` on an Apple Silicon Mac from the repository's default branch
(currently `master`). The command publishes to the GitHub repository configured
as `origin`. Commit your source changes first: both the working tree and index
must be clean, including untracked files that are not ignored. Local commits
ahead of GitHub are included; a behind or diverged branch is rejected.

## Requirements

- The normal macOS build requirements: Docker Compose with a running engine,
  Xcode Command Line Tools, and system Python 3.9 or newer (`/usr/bin/python3`).
- A working local `codex` CLI in `PATH`, already signed in and able to use
  `gpt-5.6-luna`. The command explicitly selects `medium` reasoning effort.
  Set `CODEX=/absolute/path/to/codex` to select another installation explicitly.
- A `GITHUB_TOKEN` environment variable containing a fine-grained personal
  access token with `Contents: Read and write` permission for the repository.
  Repository rules still apply; publication does not bypass branch protection.

Python orchestrates publication on the host, invokes the host's Codex CLI and
calls GitHub's HTTPS REST API directly through the standard library. The GitHub
CLI and additional Python packages are not required. Git and Rust commands
run on the host and in the Docker toolchain, respectively.
`make publish` explicitly launches system Python as arm64, so an Intel-only
`python3` earlier in `PATH` or a terminal running under Rosetta does not trigger
the Apple Silicon prerequisite error. It does not change `PATH` or your Python
installation.
The token is passed to individual host Git commands in environment variables;
it is not written to the checkout, saved state or Git configuration. The
orchestrator removes `GITHUB_TOKEN` from the environment of Codex, builds and
other child processes. The existing SSH or HTTPS `origin` URL is preserved.
Run publication without putting
the token in shell history, for example:

```sh
read -s GITHUB_TOKEN && export GITHUB_TOKEN
make publish
unset GITHUB_TOKEN
```

Every external command is printed as `publish: run ...` before execution. Prompt
contents and credentials are never included in that line. Publication prints the
resolved Codex executable separately. Codex runs in the repository root with a
read-only sandbox and inspects Git history itself; Python does not send patches or
file contents in the prompt. Codex's internal output is captured so that prompts,
diffs and nonfatal diagnostics do not flood the terminal. It does not launch Codex
merely to check `--help`; the first launch is the release-notes request.

If macOS kills Codex or displays a security warning, do not bypass Gatekeeper.
Install or update the native standalone CLI with OpenAI's official installer:

```sh
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

Open a new terminal, or point publishing at that installation explicitly:

```sh
CODEX="$HOME/.local/bin/codex" make publish
```

## What the command does

1. Validates prerequisites, the checkout and the remote default branch.
2. Reads `app/stillus/Cargo.toml`. If the history has no commit that changed the
   application version, publishes an initial release with the current version
   and does not create a version commit. Its changelog covers the full history.
   Once that version has a matching GitHub tag and Release, later publications
   increase only the patch component: `0.1.0` becomes `0.1.1`, and `0.1.9`
   becomes `0.1.10`. Updates only the
   corresponding application entry in `Cargo.lock`; dependency versions stay
   unchanged. macOS and Windows package versions come from the app manifest.
3. Finds the last first-parent commit that changed the application's version
   value. It gives this boundary and the release commit to local `codex exec`.
   Codex inspects the repository and committed Git history itself. For the first
   release it analyzes all history reachable from `HEAD`. Codex writes English
   `Improvements` and `Bug fixes` sections; an empty category is `None.`. Its
   output is generated text and can still contain interpretation errors.
4. For releases after the initial one, creates `Release vX.Y.Z` with only the two
   version files, using `Evgeniy Udodov
   <1926460+flrnull@users.noreply.github.com>` as author and committer. The
   description is saved locally and used as the annotated tag's message; it is
   not added as a tracked changelog file.
5. Runs the full `make` aggregate on the release commit. Packages the resulting
   macOS arm64 application, Windows x64 application and Linux executable for the
   Docker architecture (normally aarch64 on Apple Silicon). Linux x86_64 is not
   implicitly cross-built. The macOS bundle receives an ad-hoc integrity
   signature but is not Developer ID signed or notarized, so its first launch
   requires explicit approval in **System Settings → Privacy & Security → Open
   Anyway**. The existing platform validation limitations still apply.
6. Checks archive manifests, source SHA and file hashes. Prepares three
   versioned archives and `SHA256SUMS`, retaining licenses and platform metadata.
   The Windows test kit is not a release asset.
7. Pushes the default branch and annotated `vX.Y.Z` tag atomically, without a
   force push. Creates a draft Release, uploads the archives and checksums,
   verifies GitHub's SHA-256 digest for every uploaded byte, then publishes it
   as Latest. Only after publication succeeds, creates or moves the lightweight
   `latest` tag to the same commit as `vX.Y.Z`, using a separate push with an
   explicit `--force-with-lease` for `refs/tags/latest`. The expected previous
   raw tag object is saved before publication; concurrent changes cause an error.
   Versioned tags and the default branch are never force-pushed. The local
   `latest` tag is synchronized after the remote update succeeds, and only then
   is publication marked complete. The command prints the Release URL.
   No confirmation prompt is required.

The `latest` tag starts an additional full GitHub CI run and selects the release
shown by the README's CI badge. No separate GitHub Release is created for this
tag. It first appears after a successful publication with this publisher; badges
may show no data until that CI run finishes. The badge URLs do not need to
change with each version. Repository tag
rules must allow creating and updating `latest` with the publication token.

Published releases are what installed applications update themselves from:
they read this repository's latest release, verify the package against the
uploaded `SHA256SUMS` and replace themselves in place. A release reaches
automatic checks 24 hours after publication and manual checks immediately, so
a mistaken release can be replaced within that day. See
[updates](updates.md).

No new commits since the previous version change means no new release. A Codex
failure, unsupported version or failed build stops the process; the command does
not substitute another model or silently skip checks.

## Retrying

`make publish` takes an exclusive local lock and saves its state in
`.host-build/publish/state.json`. After a failure, rerun the same command: it
continues the pending version instead of increasing it again. A failed build
leaves the local version commit in place, without pushing it. Until all archives
are prepared, a retry runs the full aggregate again. Once prepared, archives are
reused only if their saved hashes still match.

Keep the pending checkout and `.host-build/publish/` intact until publication
finishes. Independent changes to HEAD, the version files, prepared assets,
existing release notes or remote tags cause an error rather than being
overwritten. Restore the pending checkout before retrying; do not discard the
saved state just to bypass an error. Existing uploaded assets are verified and
never replaced automatically. A successfully published release is not bumped
again until additional source commits exist.

If the Release was published but updating `latest` failed, retrying completes
the tag update without rebuilding, uploading again, or increasing the version.
A push accepted before a connection failure is detected on retry. The saved
expected previous tag is retained, so retries cannot silently overwrite a
concurrent change. State files from before `latest` was introduced remain
supported. Rerunning a completed publication at its original commit can repair
the tag only while that version is still GitHub's Latest release; an older
publication cannot move the tag back from a newer release.

Publication tests are part of `make check` through `make test-publish`. They use
temporary repositories and mocked external services; they never create a real
GitHub Release or invoke a real Codex model.
