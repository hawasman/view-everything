---
status: accepted
---

# GPUI with gpui-kit for the GUI

Supersedes ADR-0002. The GUI moves from egui to GPUI with gpui-kit. The reason is polish: gpui-kit's 75+ styled components (sidebar, cards, data tables, dialogs, theming) give the Dashboard-style look out of the box, which egui could only approach with a lot of custom widget code. ve-core and ve-scan are unaffected; ve-app is rewritten.

## Consequences

- GPUI ships only as pre-1.0 snapshots. gpui-kit 0.8.1 pins `gpui-pre =0.3.10` exactly, so GPUI upgrades move in lockstep with gpui-kit releases, and API breaks are expected.
- The requirements that drove ADR-0002 still apply: the Treemap remains custom drawing, and lists of up to 10M Entries must be virtualised in-process. gpui-kit's tables are advertised for hundreds of thousands of rows, so the flat list may need GPUI's own virtual list rather than the stock table.
