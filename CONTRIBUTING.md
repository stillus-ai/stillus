# Contributing to Stillus

Read the [README](README.md) and [development guide](docs/development.md) before
making changes. The project is in early development; discuss substantial new
features or architecture changes in an issue before implementing them.

## Bug reports

Include the application version or source revision, macOS version and hardware,
steps to reproduce, expected behavior, and actual behavior. A small synthetic
workspace is more useful than a personal notes directory. Remove private note
content, passwords, feed credentials, and personal paths from logs and images
before posting them.

For suspected vulnerabilities, follow the [security policy](SECURITY.md)
instead of posting technical details in a public issue.

## Changes and checks

- Keep each pull request focused on one change.
- Use the root Makefile and Docker toolchain. See the development guide for
  native build exceptions and the available commands.
- Add or update tests with behavior changes; never weaken tests to hide a bug.
- Preserve unknown workspace data, atomic/no-overwrite saves, conflict and
  recovery behavior, bounded memory use, and protected-body storage boundaries.
- Keep project-owned Rust safe-only. Do not introduce a database, browser or
  JavaScript runtime, or process execution. HTTP/HTTPS feed loading and explicit
  article opening remain restricted to the RSS engine.
- Use scoped UI styles, not global theme overrides for local changes.
- Update relevant English documentation when behavior or setup changes.

Run the appropriate [checks](docs/development.md#toolchain-and-commands)
before submitting. In the pull request, describe the problem, your change,
how you checked it, and any remaining failures or limitations.

## License

Contributions to Stillus's own code, documentation, and artwork are under
`GPL-3.0-only`, as described in [LICENSE](LICENSE). Submit only work you have
the right to contribute, and preserve third-party license and attribution
notices when including permitted third-party material.
