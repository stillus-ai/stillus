<!-- Copyright 2026 Evgeniy Udodov -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->
# Windows builds and acceptance

Supported target: Windows 10/11 x64, local NTFS, portable application.
An installer, code signing, ARM64, network shares,
and other filesystems are outside this target. The dependency audit includes
`x86_64-pc-windows-gnu`; the compiler remains Rust 1.88.0.

## Build and automated tests

On macOS or Linux with Docker running:

```sh
make build-windows
make test-windows-build
```

Copy the complete `dist/windows/x86_64` directory to Windows, retaining any DLLs
beside their executable. In Windows PowerShell 5.1 or later:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tests\Run-Tests.ps1
```

No Rust installation is required. The runner requires a local NTFS temporary
directory, creates isolated Unicode paths with spaces, redirects global settings
to that test profile, creates an NTFS junction fixture, runs every compiled test
executable, checks native startup and normal window closing, opens multiple external files,
and tests registration in an isolated registry subtree. Tests and logs
are retained under the printed temporary directory for diagnosis. Existing
workspaces and the user's real `.stillus.cfg` are not used.

`tests/windows-results.json` records the Windows build, filesystem, individual
test exit codes, and native launch result. A cross-compilation log and this
Windows result are separate evidence. Neither a successful `make check` on
Linux nor compilation of the test EXEs means that Windows execution passed.
There is no committed passing native Windows acceptance result yet.
The [GitHub CI workflow](ci.md) runs this same kit on `windows-2025` after
verifying the Linux job's package SHA and checksums. Its `-CI` mode exports a
sanitized report without temporary workspaces; normal local execution is unchanged.

## Native UI acceptance

Run the kit with `-Interactive` to open its isolated workspace after automated
tests. Record Windows version, GPU/driver, scale settings, and each outcome.
The script deliberately leaves manual acceptance unconfirmed.

- Create, edit, save, rename (including a change of case), trash and restore
  notes. Verify Unicode names, name collisions, and preservation of unknown YAML.
- Search notes; open and save an external Markdown file with spaces and Unicode
  in its absolute path. Restart and switch workspaces to verify global settings.
- Exercise Ctrl+A/C/X/V/Z/Y/S/F, selection, clipboard, undo, native file/folder
  dialogs, and the localized crash dialog using a synthetic failure only.
- Refresh HTTP and HTTPS RSS feeds, navigate articles, and open the original through
  the existing hardened RSS opener. Check certificate/network error messages.
- Switch all 17 languages, including Arabic and Urdu, with an unsaved editor
  and open menus. Check mixed text and scaling at 100%, 150%, and 200%, including
  moving the window between monitors and the minimum window size.
- Protect a synthetic note, edit it, change the master password, restore a
  backup, and recover unsaved work. Kill the test application during a password
  transition and repeat recovery after restart. Retain incomplete journals on
  any failure; never remove `.stillus/` wholesale.
- Test read-only and externally locked notes, hard links, junctions/reparse
  points, file substitution, and failure injection before/after publication.
  Confirm that errors leave a conflict or recovery, without claiming a save.
- Search persistent service files for the synthetic protected-body sentinel.
  YAML and filenames remain readable; protected bodies must remain age data.
- Measure large-file load, metadata polling, editing, and search performance.
  Windows version snapshots currently read the body in bounded chunks to detect
  writes that restore size and modification time.

## Persistence details

The automated Windows kit checks native startup and external-file launch by
waiting for the same responsive process window and saved selection to remain
ready for 500 ms, then closing it normally. Readiness has a 60-second deadline
and closing has a shared 30-second deadline. Only rejected close requests are
retried, for at most five seconds. Reports distinguish rejection, exit timeout,
and nonzero exit codes, and retain the window state before forced cleanup.
Opt-in application lifecycle diagnostics identify the last reached shutdown
stage without logging note contents or paths. The helper regression tests are
included in the kit; see [CI diagnostics](ci.md) for the record names.
This does not replace the interactive checklist above.

The shared filesystem layer obtains volume/file IDs from open handles, rejects
reparse-point ancestors and non-local Windows prefixes, and retains Windows
ACL entries when replacing a file. A new private file is opened exclusively,
receives a protected DACL for the current user, Administrators, and SYSTEM,
and only then becomes available to the writer. ACL failures are returned.
Metadata fingerprints preserve the open handle's read/write position. Stillus
closes consumed target readers before conflict checks and publication. Temporary
guards are bound before their writers so error unwinding closes the writer before
attempting deletion. Another program's
incompatible open handle still causes a reported failure with recovery retained.

Workspace-note recognition and restored selection compare canonical paths on both
sides, including Windows extended-length paths. Missing external files remain
visible as unavailable. Case-only rename tests inspect the actual directory entry
instead of assuming the old spelling stops resolving on case-insensitive NTFS.

Age armor written by Stillus uses LF on every OS; readers also accept CRLF armor
from Windows tools. Original plaintext line endings remain unchanged. Search
generation publication completes Tantivy merges and releases its file handles
before renaming the directory, and persisted search paths use `/` on every OS.

Replacement uses the checked `MoveFileExW` write-through operation supplied by
`atomicwrites`. Namespace synchronization uses an empty private marker, flushes
it, and moves it with write-through. Successful cleanup leaves no marker; a
crash may leave an empty `.stillus-sync-*` file containing no note data. The
marker retains a handle with `DELETE` access and `FILE_SHARE_DELETE` from its
creation through rename and removal. Windows rejects intervening readers that
would deny deletion, eliminating that specific close/reopen race. Holding the
handle does not exclude all Windows sharing conflicts; the source of remaining
`NATIVE_DIRECTORY_SYNC stage=Publish os_error=32` failures is not established.
Only marker rename/removal retries error 32, using a shared six-wait budget of
10, 20, 40, 80, 160 and 320 ms (630 ms total). Before retrying, including after
each wait, the pathname must still identify the same empty regular file with
one link. Missing, substituted or uninspectable markers stop the operation;
an error after a possible rename remains an error, never inferred success.
Other errors and exhausted retries propagate as `PostReplaceSync` when the note
was already committed; note writes and user operations are never replayed.
Cleanup removes only a verified owned source; an uncertain publication can
leave an empty destination marker. Test-kit retry diagnostics contain only the
stage, attempt, delay, OS code and ownership classification (plus thread ID).
This sharing mode applies only to empty synchronization markers;
private note and recovery writers retain exclusive handles. Native regression
tests reproduce the old sharing violation, attempt blocking opens at both
publication and removal, and verify collisions and error propagation. The
sharing contract is documented in [Microsoft's CreateFile reference](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew).
Native interruption tests must validate this protocol on the supported NTFS
systems. File synchronization errors are propagated instead of discarded.
Unix continues using file/parent-directory synchronization and mode bits.

Password transaction journals include an explicit platform marker. Windows
entries retain ACLs and content fingerprints; Unix retains inode/change-time
information. Legacy journals without a marker are Unix journals. A journal from
a different platform blocks transaction recovery and is preserved for diagnosis.
Internal relative paths continue to use `/`; absolute external paths and the
Windows home/profile path use native path handling.

## Optional Open With registration

Run `powershell -NoProfile -File .\Register.ps1` from the portable package.
Use `-Remove` to unregister that copy. The script changes only current-user
Stillus registration and OpenWithProgids for `.md`, `.markdown`, and `.txt`.
It never writes UserChoice or replaces the default application. Re-register
after moving the package. Explorer launches the executable with the requested
file; a new process/window is allowed.
