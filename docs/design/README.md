# Product and interface design

Hoard is a resource catalog and sharing interface. It helps people find, review and acquire
resources across disks and machines while keeping original files authoritative.

- [Product scope](product-scope.md) defines supported workflows and limits.
- [UI specification](ui-spec.md) defines current surfaces, spacing, typography and interactions.
- [UI change workflow](../contributing/ui-changes.md) defines baseline and acceptance checks.
- [Prototype references](prototypes/README.md) retain the original visual source files.

Current accepted behavior takes precedence over historical prototypes. Add new resource types
to the shared Index, inspector and detail patterns. Sources owns folder management, Activity
and node sharing configuration; it does not contain a second resource browser.
