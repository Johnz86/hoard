---
paths:
  - "web/**"
---

# Web UI rules for hoard

- Vanilla ES modules and Pico CSS only. No frameworks, no bundler, no npm.
- Use tokens from `web/css/tokens.css`; never hardcode an oklch value in `hoard.css` or JS.
- Match `docs/design/ui-spec.md` exactly: column widths, paddings, font sizes,
  letter-spacing, state colors, keyboard map. Follow the UI change workflow in
  `docs/contributing/ui-changes.md`. Later accepted changes recorded in the current specification
  supersede the imported prototypes; use its HTML for unspecified visual details.
- Extend the existing table/cards, inspector, detail tabs/right rail and inline row actions.
  A new resource type does not by itself justify a separate results section or page layout.
  Check both the new journey and unchanged resource types against before/after screenshots.
- Mono font for every measured value, sha, path, count, label; sans only for prose and names.
- Keyboard first: every action reachable by key; mouse targets account for scroll offsets.
- Light and dark from `prefers-color-scheme`, toggled via `data-theme` on `<html>`, remembered
  in `localStorage` inside try/catch.
- All data comes from `/api/v1`; the UI never parses the query grammar or Markdown itself.
- Fonts and CSS are served from the binary; no runtime requests to third-party hosts.

- Render the server's `display_name` and retain `entry_id` through source-sensitive actions
  and deep links. Canonical `name`, selected `relative_path` and client destination differ.
- Keep collection rows condensed, with native disclosures for details and edits. Verify
  capacity in short viewports, paired control heights and role/name column spacing.
- Patch mounted editors and preserve drafts/focus/scroll; asynchronous completion must check
  that the original editor still owns its controls after close/reopen.
