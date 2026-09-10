# User guide

## AI settings

Open Settings → AI settings. Paste an OpenAI or Anthropic API key; the provider
is detected locally. Connect fetches the model catalog and stores the key in the
system credential store. This checks catalog access, not generation permissions
or billing. Use Change key to replace or remove it, or Refresh models to update
the available models. Verification never sends notes.

After connecting, Model aliases lets you name model/effort combinations for use
throughout the product. Changing an alias updates the model used by every task
that refers to that name. There is no fixed number of aliases. Use Add alias to
create one, or click an existing alias to change its name, model, or effort.
Names are case-sensitive, trimmed on save, and must be unique and nonempty.

The built-in `default` starts with GPT-5.6 Luna for OpenAI or Claude Sonnet 5 for
Anthropic, both with `high` effort. These reviewed defaults are explicit because
the catalogs do not expose a consistent model-tier field. An available dated
snapshot is used if the canonical model ID is absent. If neither is listed,
`default` is shown as unavailable; select an accessible model before using it.
Refreshing the catalog or replacing a key for the same provider never changes a
saved alias automatically.

You can change the model and effort of `default`, but cannot delete or rename
it. Deleted, renamed, or unknown alias names resolve to `default`. An existing
alias with an unavailable model or effort returns an error instead of silently
switching models; the same applies if `default` itself is unavailable. New
aliases start with the model and effort from `default`. Changing the model keeps
a compatible effort, otherwise selecting `high` or the first supported level.
The model dropdown only includes models with a supported effort parameter. Choose effort from the second
dropdown; there is no separate model search field.

Settings apply across workspaces. Switching providers clears the aliases and
creates a new provider-specific `default`. Network errors retain the previous
connection. On Linux, unlock the desktop Secret Service if key storage fails.
No key is written to a settings file. The retired Small/Medium/Large profiles
are not migrated to aliases.

Assistant commands and text generation will be added separately.

[Back to README](../README.md)

The interface defaults to English, independently of your operating system.

## Language

Choose **Settings → General → Language**. Language names are displayed in their
own language. The choice applies immediately and is remembered for all workspaces
in `~/.stillus.cfg`. Existing installations without a saved choice use English.

Available languages are English, Spanish, Russian, Simplified and Traditional
Chinese, Brazilian and European Portuguese, Hindi, Arabic, French, Bengali,
Indonesian, Urdu, German, Japanese, Turkish, and Korean. Arabic and Urdu place
the navigation sidebar on the right. Notes, tags, filenames, and RSS articles
retain their original content. New notes receive a title in the selected language.

Translations are included in the application and work offline. System-owned
file dialog controls follow the operating system's language. Technical diagnostic
details remain in English.

## Updates

Open **Settings → Updates** to see the installed version, look for a new
release and install it. Stillus also looks in the background at every start and
offers a release in a card with **Update now** and **Later**; a release is
offered automatically only a day after it was published, while checking from
the settings page installs it immediately. After an update Stillus asks you to
restart it: the application never restarts itself. The
[updates guide](updates.md) describes the checks, the platforms and the
network requests involved.

## Workspaces and settings

On first launch without an explicit or saved workspace, Stillus offers to
create `~/Downloads/Notes` or choose another folder. Filesystem changes happen
only after confirmation. The selected folder is the workspace root: its notes
live in `notes/`, for example `~/Downloads/Notes/notes/`.
An unavailable saved workspace brings you back to the folder selection screen.

After successfully opening a workspace, Stillus remembers its absolute path in
`~/.stillus.cfg`. Workspace layout and the list of external files are stored in
`<workspace>/.stillus/settings.json`. Use the settings screen to change the
workspace. See [Storage and security](storage.md) for what to preserve when
moving or backing up a workspace.

## Notes and organization

Notes use UTF-8 Markdown with YAML front matter compatible with Notable.
Categories come from YAML tags. Notes can be favorited, pinned, and soft-deleted.
Categories and Favorites support manual order and automatic sorting.
Editing uses autosave; recovery and conflict handling help preserve work after
a crash or an external edit. Local search indexes workspace notes.

Stillus preserves unknown front matter fields and unrelated files. Opening a
workspace does not rewrite notes. It does not scan nested note directories or
follow note symlinks.

## External files and desktop opening

External `.md`, `.markdown`, and `.txt` files open as complete UTF-8 text without
parsing YAML front matter. They remain at their original locations and are not
copied into `notes/` or added to the workspace search index. Their ordered list
is saved separately for each workspace.

The close control in External removes only the sidebar reference;
it never deletes the external file.

The macOS bundle declares support for these extensions, but does not replace
your existing default editor automatically. In Finder, select a file, open
Get Info (`⌘I`), choose Stillus under Open with, and select Change All if you
want it to become the default. Double-clicking a file or choosing Open with
then delivers it to the running application or launches Stillus. The file
appears in the active workspace's External group.

On Linux, run the package's `python3 Register.py` to add Stillus to Open With;
`python3 Register.py --remove` removes that registration. Python 3 is needed only
for registration. On Windows, run `powershell -NoProfile -File .\Register.ps1`;
add `-Remove` to unregister. Neither registration changes your default editor.
Register again after moving the portable package.

You can also pass files directly: `stillus --open first.md second.txt`, optionally
with `--workspace /path/to/workspace`. A directory argument alone keeps its
workspace-selection meaning. Requests wait for workspace selection on first
launch. Linux and Windows can open a new window; Finder normally reuses the
running macOS application.

Different windows keep independent editor state. A conflicting save or recovery
write is reported instead of overwriting another window's work. Independent
settings changes are merged; conflicting changes to the same setting require
closing and reopening the workspace. Resolve conflicts before closing a window
with unsaved edits.

Search and Find hints show Cmd on macOS and Ctrl on Windows/Linux. The latter
also support Ctrl+Home/End, Ctrl+Shift+Home/End and Ctrl+Y. AltGr text input stays
available.

## RSS and Atom

Choose `+` → RSS feed and enter a direct HTTP or HTTPS feed URL. RSS 1.0, RSS 2.0,
and Atom are supported. Subscriptions appear alongside notes; entries open in
a native, read-only feed view. A feed refreshes when opened and through its
toolbar button, and automatically while its workspace is open.

The first ten entries in the first successful response are marked unread.
Scrolling alone does not change read status: open a card by clicking it or
using `J`/`K`. Selecting a card scrolls it to the top of the feed. Read cards
are dimmed, with additional contrast for the selected card.

Cards show a bold sans-serif title, the author and local date, then a serif
Markdown excerpt. They neither execute HTML nor load remote images. Clicking
a linked title opens the original article in the system browser and marks the
card read. If an entry has no original link, a suitable HTTP or HTTPS link from its
excerpt can be used instead. Without a suitable link, the title is plain text.
Other excerpt links are displayed as text.

Feeds are fetched over HTTP or HTTPS without cookies or authentication, with no
host or port allowlist; local network feeds are supported. HTTP traffic is
unencrypted. Opening an article explicitly hands its HTTP or HTTPS URL to the
system browser. Subscriptions, cached entries, and read status have different
storage roles; see the
[storage guide](storage.md).

### RSS filters

Open **Filters** in the RSS toolbar. **Blacklist** and **Whitelist** accept one
regular expression per nonempty line (16 KiB per field), without enclosing `/`.
Search covers the title, a newline, and all cached RSS article text; the linked
page is never downloaded. Matching ignores case by default. Use `(?-i)` for
case-sensitive matching, or `(?m)` for line anchors. Expressions use Rust's
`regex` syntax; look-around and backreferences are not supported. Invalid or
excessively complex expressions block saving and identify the list and line.

An article is hidden only when a blacklist expression matches and no whitelist
expression matches. For example, blacklist `promotion|sponsored` and whitelist
`rust` hide promotions except those mentioning Rust. An empty blacklist keeps
all articles visible. Blank lines are ignored; spaces in nonempty expressions
are significant.

**Cancel** and Escape discard the draft. **Save** saves rules for new or changed
articles and closes the popup, preserving existing hiding decisions even after
restart. **Save and Apply** also recalculates every cached article, including
read articles, and closes on success. It can both hide and reveal articles;
clearing the blacklist and choosing **Save and Apply** removes filtering.
Read marks are preserved. Filtering runs locally and needs no AI provider or key.

Click a collapsed title to expand and read it inside Stillus; it remains hidden
from J/K navigation. Hidden unread entries do not contribute to the visible badge.

Every refresh attempt, including errors and HTTP 304, advances a persisted
backoff counter. After cycle `i`, the delay is `min(86400, 60 + 2^i)` seconds
when any unread entries remain, or `min(86400, 60 + 1.5^i)` otherwise. Hidden
entries count here. A visit or manual refresh resets the counter and requests
an immediate cycle; an existing download is reused. An overdue schedule runs
once after restart. No updates run while the application is closed.

At 99 unread entries automatic refresh pauses until a visit. Each visit grants
one forced refresh. Two RSS downloads can run concurrently. **Save and Apply**
works on cached articles even while automatic refresh is paused, without
requesting a download. There is no classification budget.

## Protected notes

Protected notes encrypt their Markdown body, while the filename and YAML
metadata stay readable. They require the workspace master password.
The password can be changed under Settings → Encryption.

Changing the password does not re-encrypt existing backup history. Keep the
previous password if you need to read older encrypted backups. See
[Storage and security](storage.md) for the recovery and backup details.

## AI chats

Choose **+ → AI Chat** to create a conversation in the current category or
Favorites. You can create and read chats before connecting a provider. Use the
chat's **Open AI settings** button to connect OpenAI; Anthropic model catalogs
remain available, with chat generation planned for a later adapter.

Choose a model alias, type a message and press Enter (Shift+Enter inserts a line
break). Drafts are saved locally. Each chat keeps its own alias and linear history;
sending captures the current model, so editing aliases affects later tasks. You
can edit the next draft while a reply is running. Stop prevents further requests
and actions; Continue resumes tasks paused at their request/tool limit.

Chats share the note/RSS sidebar controls for renaming, categories, pinning,
Favorites, sorting, dragging, Trash and restoration. The first message supplies
a title of up to 64 characters, unless you renamed the chat. A running indicator
and unread badge help track background replies. Opening and viewing the latest
reply marks it read. Up to two tasks run concurrently, with six waiting.

Your messages appear in shaded bubbles on the right. Replies render Markdown and
copyable code blocks; long code lines scroll inside their block. Only the history
scrolls, keeping the toolbar and composer visible. Scrolling near the top loads
earlier messages automatically while preserving the message you are reading.
The toolbar's Refresh button reloads the latest messages and chat state from
disk without resending a request or clearing your draft. The visible history
uses a bounded window; Refresh also returns to the latest messages after reading
older pages. New replies scroll into view only while you are at the bottom.
Open the request journal from **Settings → AI settings →
Request journal**. Hover over a toolbar icon to see its label.

Expand an action card to see its arguments and result. The assistant
can use permitted application actions; existing confirmations and protected-note
restrictions still apply. A save operation is awaited before its result is given
to the model. Unknown outcomes after a crash require your decision and are never
repeated automatically.

Chats are stored unencrypted with the workspace; the global journal has independent
retention. See [storage documentation](storage.md). If journal persistence fails
after a reply, retry saving from the journal and then Continue. This does not
resend the completed request. Full conversation remains on disk when older
context is summarized for a long task.
