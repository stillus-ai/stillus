# Security policy

## Reporting a vulnerability

Please report suspected vulnerabilities privately. Do not include exploit
details, personal notes, passwords, or other secrets in public issues or pull
requests.

Use GitHub's [private vulnerability report form](https://github.com/stillus-ai/stillus/security/advisories/new)
when private reporting is enabled. You can also find it under
**Security → Advisories → Report a vulnerability** in the repository.
If the form or button is unavailable, [open an issue](https://github.com/stillus-ai/stillus/issues)
asking for a private reporting channel, without describing the vulnerability
or attaching sensitive files. Wait for that channel before sending technical
details.

Include the following in the private report:

- The source revision or application version, macOS version, and hardware.
- Steps to reproduce using a small synthetic workspace, where possible.
- The expected behavior, observed behavior, and potential impact.
- Any relevant logs or screenshots with private content removed.

Please allow time to investigate and coordinate a fix before publishing
technical details. Response and fix times are not guaranteed.

## Supported versions

Stillus is in early development. Security fixes target the latest development
revision; older revisions have no separate maintenance or backport commitment.
There is currently no Developer ID signed or notarized public release. Native
macOS release bundles use an ad-hoc integrity signature and require explicit
user approval through macOS Privacy & Security settings after download.

## Security boundaries and known issues

Protected notes encrypt the Markdown body. Filenames and YAML metadata,
including titles and tags, remain readable. Unprotected notes and their recovery
records are not encrypted. See the [storage and security guide](docs/storage.md)
for the file layout, backup behavior, and network boundaries.

Known dependency warnings and binary release limitations are tracked in the
[development guide](docs/development.md#known-dependency-warnings). A successful
audit command does not mean those warnings have been resolved.
