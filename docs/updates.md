# Updates

Stillus updates itself from the GitHub Releases of this repository. Open
**Settings → Updates** to see the installed version, look for a new release and
turn the startup check on or off.

## What the application does

- **At every start**, if the startup check is on, Stillus asks GitHub for the
  release currently marked as the latest one. The check runs in the background
  a moment after the window opens.
- **A release younger than 24 hours is held back.** The startup check offers a
  version only a full day after it was published, so a release that turns out
  to be broken can be replaced before it reaches everyone automatically. The
  Updates page still names the held release, and updating from that page
  installs it immediately.
- **Nothing is installed without a decision.** When the startup check finds a
  release, a card in the corner of the window offers **Update now** or
  **Later**. The window stays usable behind it. Declining is remembered: that
  version is not offered again at the next start, while a newer one is.
- **Checking from the Updates page ignores the 24-hour hold**, so a release can
  be installed on the day it is published.

## What an update does to the installation

1. The release metadata is read from
   `https://api.github.com/repos/stillus-ai/stillus/releases/latest`.
2. The `SHA256SUMS` asset and the package for the running platform are
   downloaded. The package is rejected unless its bytes match the published
   checksum.
3. The package is extracted into a hidden staging directory beside the
   installation, on the same filesystem. Entries with absolute paths, `..`
   segments, symbolic links or other special files are refused, and the
   extracted tree must match the `build.json` manifest the packager wrote: the
   right platform, the right source revision and the recorded SHA-256 of every
   file. On macOS the bundle must also carry the offered version and the
   `org.stillus.Stillus` identifier.
4. The installed application is replaced by renaming: the previous bundle or
   files move aside, the new ones take their place, and a failure at any step
   restores what was there before.
5. A notice offers **Restart** or **Later**, including after a manual update.
   Restart waits for saves and background operations to finish, preserves the
   current workspace and open-file selection, and launches the updated Stillus
   executable directly. The new window opens after the old window shuts down.
   Save conflicts, security operations and launch errors leave Stillus open.
   **Later** keeps the current session running; **Settings → Updates** retains
   the Restart button. Files left behind by replacement are removed at the
   next start.

Only the application's own installation directory is written. Workspaces,
notes and `.stillus/` are never touched by an update.

## Where updates are unavailable

- **Development builds.** A binary that is not a packaged installation, for
  example `cargo run` output, cannot replace itself; the Updates page says so.
- **An installation you cannot write to.** If Stillus was installed by another
  user or an administrator, the update stops before anything is downloaded and
  the page suggests installing the release manually. **Open the release page**
  leads to the download.
- **A platform without a package in that release.** Publication builds macOS
  arm64, Windows x64 and one Linux architecture; a release without a package
  for the running platform is reported rather than installed.

## Network and privacy

The update client talks only to GitHub, over HTTPS, and only to the hosts that
serve release metadata and release assets. Every redirect hop is checked
against the same list, response sizes are capped, and no note content, no
identifier and no credential is ever sent. Anonymous GitHub requests are rate
limited per address; when the limit is reached the startup check is skipped
silently and a check started from the Updates page says so.

The checksum list is published next to the packages and served by GitHub over
the same connection: it protects against a corrupted or truncated download,
not against a compromised publisher account. The macOS bundle carries an
ad-hoc signature only, exactly like the bundle in the release archive, and an
update preserves that property rather than improving it.

## Turning the startup check off

**Settings → Updates → Startup check** stores the choice in `~/.stillus.cfg`
under `updates`. With the check off, Stillus never contacts GitHub on its own;
the button on the Updates page still works.

See [publishing](publishing.md) for how a release reaches the endpoint this
page describes.
