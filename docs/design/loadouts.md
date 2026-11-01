# Model collections and client downloads

Loadouts are named collections of indexed models, assembled across repositories and source
folders. They describe files to acquire on the client. One Hoard server is sufficient.
Membership does not copy files, launch inference, configure a tool, or write server links.

## User journey

1. Search the existing Index. Loadouts participate in the same table/cards, ordering, cursor,
   pagination and result totals as artifacts and Git projects. Their role reads LOADOUT;
   `role:loadout` lists collections. Name searches also match member filenames. Artifact-only
   filters continue to return artifacts. Facet and shelf counts include matching collections.
2. Open a loadout row to inspect its drawer without replacing the Index underneath it. A
   direct `#/loadout/<id>` link opens the same collection after reload. A member model's
   inspector and model detail list RELATED LOADOUTS as buttons opening that drawer.
3. The toolbar LOADOUTS button opens the collection chooser. Create a collection with NEW;
   duplicate/blank names produce an inline error and retain the entered name. Select a
   current collection. ADD TO LOADOUT or L opens a chooser for the current model or selected
   models; confirm ADD TO THIS LOADOUT, or create a named collection to add them to. Adding a
   second VAE, checkpoint or encoder appends it; exact duplicate members are idempotent.
4. The drawer has a named collection selector, a name field with explicit SAVE, and a
   vertically scrolling member list. Each member shows its source, size/availability, an
   editable download path relative to the client's chosen root, explicit SAVE PATH, and a
   labeled REMOVE button. Removing membership never removes model files. All model roles
   are supported. Existing legacy loadouts remain readable and editable.
5. DOWNLOAD ON YOUR COMPUTER replaces server-side Apply. Show the exact relative directory
   tree, total selected bytes and unavailable members. The client root defaults to `./models`
   (run from ComfyUI), and is editable; Windows users may enter an absolute path. Choose
   PowerShell or POSIX shell, then COPY DOWNLOAD SCRIPT or SAVE SCRIPT. The generated script
   uses the existing `hf` CLI against this server and requests only the collection's pinned
   files. Destination paths are explicit, not inferred again at download time. No background
   server apply job or second node is involved.
6. Scripts stage each file beneath the chosen root, stop on download failure, and move a
   completed file into its previewed destination. Existing destination files are preserved
   and reported as skipped; no silent overwrite or claim of checksum validation. Rerunning
   can resume incomplete staged downloads through the HF CLI. Missing members, unsafe paths
   and case-insensitive destination collisions block script generation with a clear error.
   Unhashed files and multi-shard artifacts also block generation with an explanation;
   scripts currently acquire individual model files. Unsaved path edits disable copying
   and saving the script until the path is saved or restored.
7. DELETE LOADOUT uses an inline confirmation naming the collection. CANCEL restores focus;
   confirmation removes only the collection, its search result and related-membership links.
   The next collection or an empty chooser appears. Original files remain available.

## Interaction and visual contract

Reuse the existing 520px drawer, Pico native form controls, typefaces, spacing, tokens, shared
Index renderers, inspector facts/actions and keyboard hints. Keep all deep work in Rust.
The list body owns scrolling; the download footer remains reachable on short viewports.
Use native buttons for collection selection and member actions, associated labels for form
fields, visible focus and polite inline status. Opening focuses CLOSE; closing restores the
invoking control and prior text selection. Escape blurs an input before closing the drawer.

The drawer shell is mounted once per opening. Background Index/inspector updates and CRUD
responses must not recreate it, replay its opening animation, replace focused inputs, lose
unsaved name/path/root drafts, reset selection or reset list scroll. Updating membership
patches keyed member rows. An explicit collection switch changes the editor's document and
drafts; stale asynchronous responses must never overwrite a newer selection. Controls for a
pending mutation are disabled against duplicate submissions. Failure preserves drafts and
allows retry; no rename is sent just because a field loses focus. Changing destination paths
or client roots never changes the model catalog or any server filesystem path.
Reuse the existing owner-access form inside the drawer so rejected writes can be retried
without losing edits. Collapse new-collection creation when a collection is already selected.
Folder-layout and script previews expand on demand; saved PowerShell scripts use a UTF-8 BOM
for Windows PowerShell compatibility.

## Implementation sequence and boundaries

Implement the collection document and portable path validation first, retaining legacy TOML
read support. Add resolved views, member editing, related membership and client script
generation. Extend the shared resource search/merge and facet/shelf arithmetic, then replace
the drawer and wire Index/inspector discovery. Finally exercise browser journeys and actual
downloads with an isolated node before adding the requested real collection.

Keep generic server projection APIs available to existing callers, but remove them from the
collection UI. Exclude inference workflows, automatic upstream model acquisition, additional
nodes, authentication redesign, archive streaming and network configuration changes.

## Observable acceptance criteria

- Create Qwen Image 2.1 Text to Video containing exactly the four requested files, including
  both encoders. Paths are `diffusion_models/qwen_image_2.1_int8_convrot.safetensors`,
  `text_encoders/qwen3vl_8b_int8_convrot.safetensors`,
  `text_encoders/qwen3.5_9b_qwen_image_2.1_pe_t2i.int8_convrot.safetensors`, and
  `vae/qwen_image_2.1_vae_bf16.safetensors` beneath the client root. This name is supplied by
  the user; membership alone does not certify an inference workflow or video capability.
- Find the collection by name and member filename, filter to loadouts, open it through a
  member's inspector, and reload its deep link. Sort and page mixed resources without
  omissions/duplicates. Counts agree with displayed results and saved shelves.
- Add several files of the same role; remove one; rename; change a download path; confirm
  delete; cancel delete; retry rejected writes; reload and restart. No original file changes.
- Execute the generated shell script against real isolated HF endpoints with four synthetic
  files spread across repositories/nested paths. Verify exact final paths and bytes, no
  unrelated models, repeat-download preservation and failure on unavailable members. Review
  PowerShell quoting, path safety and matching command targets.
- Observe no drawer remount/animation restart, lost drafts/focus/selection, or scroll resets
  during edits, Index refreshes, delayed requests and failures. Exercise keyboard and mouse.
- Review 1180/1600px, both themes/densities, table/cards and inspector on/off. Existing model
  classification, Git project acquisition, sources and sharing journeys continue to work.
