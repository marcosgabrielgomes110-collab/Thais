# ADR-0012: Wide tables in the TUI

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP TUI rendering, README TUI requirements

## Context

Terminal width is limited. Wrapping table cells breaks column alignment and makes dense tables hard to scan. Plain text loses structure.

## Decision

- Render tables at their natural width and use horizontal scroll when they exceed the viewport.
- Below a minimum width (40 columns), fall back to plain text: one field per line or a key-value layout.
- Do not wrap cells by default.
- Preserve alignment and use display width, not bytes or runes, for column sizing.
- Show a scroll indicator in the status bar.
- Test narrow terminals, wide cells, and borders.

## Consequences

- Tables remain readable and structurally stable.
- Users navigate horizontally for wide tables.
- The renderer needs a viewport and a plain-text fallback path.

## Alternatives considered

- Wrap cells: fits width but destroys table readability.
- Always plain text: simple, but loses structure.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
