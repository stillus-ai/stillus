# Storage and security

[Back to README](../README.md)

To report a suspected vulnerability, follow the [security policy](../SECURITY.md).

## Files are the source of truth

The authoritative text and metadata of workspace notes live in UTF-8 Markdown
files under `<workspace>/notes/`. YAML front matter is compatible with Notable.
Unknown fields, unrelated files, and directories are preserved. Stillus does
not follow note symlinks or scan nested note directories, and opening or
scanning a workspace does not rewrite untouched notes.

Saves use atomic replacement, external conflict checks, and protection against
overwriting a colliding path. Storage and metadata operations are designed to
keep bounded memory use. External files stay at their original paths and are
not part of the workspace's note index.

```text
workspace/
├── notes/                         # authoritative note files
├── .stillus/
│   ├── settings.json              # layout and external-file references
│   ├── engines/rss/subscriptions.json
│   ├── search/                    # rebuildable local search index
│   ├── cache/rss/                 # feed content, validators, and read status
│   └── recovery/                  # potentially unsaved work
├── .stillus_security/
│   ├── master.age                 # password verifier and workspace identity
│   └── secrets/                   # encrypted engine secrets
└── .stillus_backups/
    └── secure/                    # encrypted rollback history
```

This shows the main storage locations, not an exhaustive list of files.
Preserve unknown files and operation journals as well.

## What to preserve

| Location | Role and consequences of removal |
|---|---|
| `notes/` | Authoritative notes, including soft-deleted notes. Preserve it. |
| `.stillus/settings.json` | Workspace layout and external-file references. Removing it loses those settings and references. |
| `.stillus/engines/rss/subscriptions.json` | Subscription URLs, local names, and organizational metadata. Preserve it to keep subscriptions. |
| `.stillus/search/` | Derived local search index. It can be rebuilt from notes. |
| `.stillus/cache/rss/` | Downloaded entries, HTTP validators, and read status. Content may be fetched again, but old entries may disappear from the source feed and read status is not recoverable from it. |
| `.stillus/recovery/` | Recovery records may contain edits not yet saved to a note, including work retained after a conflict. Removing them can lose work. |
| `.stillus_security/` | Authoritative password verifier, workspace identity, and encrypted engine secrets. Preserve it. |
| `.stillus_backups/` | Encrypted rollback versions and their supporting records. Preserve it if you need backup history. |
| `~/.stillus.cfg` | Global settings, including the interface language and last workspace's absolute path. Stored outside the workspace. |

Do not delete `.stillus/` wholesale as a cache cleanup. Resolve pending recovery
and conflicts in the application before considering removal of their records.
For a workspace backup, close Stillus and copy the entire workspace, including
hidden directories. External files require separate backups at their original
locations. Application-managed rollback history is not a replacement for a
separate backup.

## Protected notes and secrets

### Global AI credentials

AI settings are shared by all workspaces in `~/.stillus.cfg` (the usual profile
directory on Windows). The `ai` object contains a provider, an opaque credential
reference, the last verified model catalog, and an `aliases` map from names to
model/effort selections. The reserved `default` alias is created on connection.
The config never contains the API key. Reading settings does not rewrite them.
Catalog refresh failures retain the previous catalog and aliases; unavailable
selections remain visible but cannot be resolved for use by an engine.

Keys are stored through `stillus-platform` in macOS Keychain, Windows Credential
Manager, or Linux Secret Service under service `stillus/ai`. Linux requires an
unlocked Secret Service in the desktop session. There is no plaintext fallback.
This storage is independent of workspace master passwords and is not transferred
by copying a workspace or the global config to another computer.

Replacement first verifies and stores a new key, then atomically switches the
config reference. Old keys are deleted afterward; failed removals retain their
references in `pending_deletions` for the settings page's retry action. Config
conflicts reject the operation without replacing the previous connection.

Checking and refreshing use only the fixed HTTPS model catalog endpoints at
`api.openai.com` and `api.anthropic.com`. Redirects and environment proxies are
disabled; requests have time, page, model-count, and response-size limits.
These operations never read or transmit notes, protected bodies, or RSS articles.

The global request journal lives in `~/.stillus/ai/journal/`. Open it from
**Settings → AI → Request journal**, even without a connected provider. It
contains versioned JSON records for each HTTP request, including catalog pages,
with the provider, time, operation, safe parameters, response, status and duration.
Model and token fields are empty for catalog requests. Keys and authorization
headers are excluded; request and response contents are omitted for protected
contexts. Journal files are private to the user and independent of workspaces.

Before a request is sent, its initial record must be durably saved. A journal
failure blocks new AI requests without blocking note editing. If recording the
response fails, the result remains available in bounded process memory; use
**Retry saving** to persist it without repeating the network request. After a
crash, an unfinished record has an unknown outcome, not a successful result.

Completed and interrupted records expire after 30 days or when the journal reaches
100 MB, oldest first. OS-held activity markers protect requests running in other
windows or processes as well. Active records are retained. **Clear history**
removes completed and interrupted records after confirmation. Corrupt records and unrelated files are
preserved; they do not hide valid history. The journal is not a workspace cache
and opening it does not rewrite notes or global settings.

RSS filtering is entirely local. No RSS preferences or article text are sent to
an AI provider, and filtering never reads credentials or fetches linked pages.
Each subscription's `preferences` holds `blacklist`, `whitelist`, and a version.
Each nonempty line is a case-insensitive Rust regular expression; input and
compiled-expression sizes are bounded. A blacklist match hides an article only
when the whitelist does not match.

RSS `state.json` retains read IDs, automatic decisions (including keep), the
preferences version used for each decision, a content fingerprint, and the
refresh `schedule`. New and changed entries are classified on refresh. Saving
rules alone preserves existing decisions; saving and applying recomputes every
cached entry without changing read marks. Content fingerprints prevent a rule
edit or application restart from silently reclassifying unchanged entries.
An HTTP 304 response also finishes local filtering if a previous cache write
succeeded before its updated decisions could be persisted.

These fields default in memory when absent; reading does not migrate files.
Unknown fields survive writes. Former reaction fields have no effect on
visibility. RSS mutations use a cross-process operation lock, revision checks,
and atomic file replacement. The worker's session gate prevents a closed
workspace session from applying results. Preferences and state are separate
files: a write failure is reported and can require reopening the form to retry.

### Workspace encryption

A protected note keeps its YAML front matter and title-derived filename in
plaintext. Its Markdown body is an authenticated, ASCII-armored age envelope.
Encryption therefore does not conceal titles, tags, filenames, or other YAML
metadata. Unprotected notes and their recovery data are not encrypted.

The protected body's plaintext is not written to persistent search indexes,
recovery records, caches, diagnostics, or application-managed temporary files.
This is an application storage boundary, not a claim that the entire computer
or workspace is encrypted.

`<workspace>/.stillus_security/master.age` contains an authenticated master
password verifier and a permanent random workspace ID. Files under
`secrets/<random-id>.age` contain immutable engine secrets. Their encrypted
payload binds them to the workspace, engine, owner, and field key. The security
directory uses private permissions. Engine configurations refer to secrets
through `SecretRef` rather than storing their plaintext.

## Password changes and rollback history

Changing the master password re-encrypts the verifier, referenced engine
secrets, all protected notes including deleted notes, and active encrypted
recovery files. It also works when only the verifier remains in the workspace.

The operation uses a recoverable filesystem journal. If interrupted, Stillus
finishes installing the complete new set or restores the previous ciphertext
set before scanning the workspace. Passwords and plaintext are not written to
the journal.

Before replacing an already protected note, Stillus backs up the previous
confirmed ciphertext version. After atomic replacement, it verifies the
SHA-256 of the complete stored file against the generated bytes. The latest
ten versions are retained for each logical note. Secure rollback history also
covers engine secrets.

Each note backup contains the whole protected file: its YAML remains readable
and its body stays encrypted. Manifests and incident records contain internal
IDs, relative paths, version numbers, timestamps, and SHA-256 values, not the
body plaintext or master password.

Password changes create backups under the same retention policy, but do not
re-encrypt existing backup history. Keep the previous master password to read
those older versions.

## Network boundary and known limitations

RSS uses a restricted HTTP/HTTPS client for direct feed URLs, without cookies or
authentication and without host or port restrictions. HTTP traffic is unencrypted.
Explicit article opening uses the RSS engine's dedicated HTTP/HTTPS opener.
There is no WebView or HTML execution, and feed cards do not
fetch images. RSS never creates note files in `notes/`.

Project-owned Rust forbids unsafe code. This does not mean all transitive
dependencies are safe-only or free of defects. See the dated dependency
warnings and native release limitations in the [development guide](development.md).

## Concurrent local windows

A workspace root (or an external file's containing directory) can contain an
empty `.stillus-operation.lock`. It coordinates short filesystem operations;
it contains no paths, note text or requests. Do not delete it while Stillus is
running: the OS releases its lock on process exit, and removing the marker
would create two independent locks. It is not an authoritative note or cache.

Atomic publication and version checks run within that lock. Password rotation
and startup transaction recovery share it, so another opening window cannot
roll back a live transaction. Recovery writes reject a competing record and
preserve it; automatic cleanup cannot rely on another process's revision number.
Conflicting settings changes are reported rather than silently replacing the
other window's settings.
