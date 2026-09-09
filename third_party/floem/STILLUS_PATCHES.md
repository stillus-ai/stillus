# Floem 0.2.0

This directory contains the crates.io Floem 0.2.0 source, distributed under
its original MIT license (see LICENSE). Stillus keeps its existing dependency
versions and applies the following narrow layout fixes:

- `WindowConfig::min_size`: forward a logical minimum inner size to winit.
- `EditorView::layout`: constrain `WrapMethod::EditorWidth` to the parent viewport,
  preventing a stale intrinsic line width from creating a horizontal scrollbar.

Trailing whitespace in upstream documentation comments is normalized.

Original source: https://crates.io/crates/floem/0.2.0
Upstream repository: https://github.com/lapce/floem

Stillus modifications: Copyright 2026 Evgeniy Udodov, GPL-3.0-only.
Unmodified upstream files retain their original license.
