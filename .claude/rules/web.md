---
paths:
  - "web/**"
---

# Web UI rules for hoard

- Vanilla ES modules and Pico CSS only. No frameworks, no bundler, no npm.
- Use tokens from `web/css/tokens.css`; never hardcode an oklch value in `hoard.css` or JS.
- Match `docs/design/ui-spec.md` exactly: column widths, paddings, font sizes,
  letter-spacing, state colors, keyboard map. Open the handoff HTML when a detail is
  missing from the spec.
- Mono font for every measured value, sha, path, count, label; sans only for prose and names.
- Keyboard first: every action reachable by key; mouse targets account for scroll offsets.
- Light and dark from `prefers-color-scheme`, toggled via `data-theme` on `<html>`, remembered
  in `localStorage` inside try/catch.
- All data comes from `/api/v1`; the UI never parses the query grammar or Markdown itself.
- Fonts and CSS are served from the binary; no runtime requests to third-party hosts.
