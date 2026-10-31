# Start your first Hoard catalog

Hoard helps you find, review and share models, datasets and Git projects already on your
machine. It indexes nominated folders in place; adding a source does not upload, move or
rename its files.

Watch the [narrated introduction](../media/introduction/hoard-introduction.mp4), or follow the steps below.
The [recording guide](introduction.md) includes the transcript and reshoot command.

## Start empty

From a directory without `hoard.toml`, start the binary:

```sh
hoard serve --bind 127.0.0.1:7099
```

For a source checkout, use `cargo run -- serve --bind 127.0.0.1:7099` instead. Open
`http://127.0.0.1:7099/`. Hoard creates its data directory and catalog; no example folders
are required. With no folders or resources, the bare URL opens Sources and its short setup
guide. Explicit Index searches and shared resource links keep their destinations.

The command above listens on this machine only. Without `--bind` or a configuration file,
the default listener is `0.0.0.0:7099`, which accepts connections on all IPv4 interfaces.
Copied local links use `127.0.0.1` until you configure a sharing address. A configured address
is not proof that another machine can reach it.

## Add a folder

In Sources, choose **ADD FOLDER**. Enter an absolute path on the machine running Hoard,
choose a recognizable name and leave the layout on **Auto-detect**. You can select **Git**
explicitly for a repository, **loose** for ordinary model folders or **hf-cache** for a
Hugging Face cache. Choose **ADD & SCAN**.

The folder remains in Sources while indexing runs in the background. Activity shows scan
progress or errors. **BROWSE** opens a source query in the normal Index. An empty folder
can finish successfully without producing resources; check the path and scan results, or
add another folder. An invalid path leaves the form available to correct and retry.

Paths belong to the server, even when your browser is on another machine. Hoard does not
browse or upload a folder from your browser's filesystem. Add only folders you intend to
make available through this node.

If the node requires owner access, open **SHARING & SETUP**, enter its `HOARD_NODE_TOKEN`
and choose **USE TOKEN**, then retry the change. The token remains in this tab's memory
only and is cleared on reload. Browsing and downloading do not require that write token.

## Find and review

Open Index. Models and Git projects share the same table, cards and inspector. Click a row
for facts; double-click or press Enter to open it. Search for a project by name, committed
README text or file path, or use `role:git` to show Git projects. A project does not need
model files to appear.

For a model or dataset, select its row and expand **REVIEW CLASSIFICATION** in the inspector.
Review the recorded evidence, choose a role and model family, and confirm the classification.
Blank family input keeps the current family. Owner access is required to save changes.
**CLEAR OVERRIDE** removes a saved decision; facts change when the files are re-inferred.
**REJECT AS NOISE**, followed by confirmation, skips the artifact's indexed paths on future
scans without deleting their files. A still-incomplete shard set can remain in triage after
its classification is confirmed.

Project details offer README, FILES and REFS. Follow relative documentation links, preview
text and choose a revision before copying a project link or clone command. Escape returns
from a file preview to the file list, then to Index. A search with no matches offers
**CLEAR SEARCH**; it does not restart setup or discard your folders.

## Share from this node

Open **SHARING & SETUP** in Sources. Review the active address and listener. Set a node name
and an HTTP or HTTPS address other machines can reach, for example
`http://workstation.lan:7099`, then choose **SAVE SETTINGS**. These values persist in
`<data_dir>/node.json`. Sharing-address changes apply immediately to generated links and
download commands. Only a changed node name remains pending until restart. Saving does not
restart the process, change its listener or change network settings.

If you started with a loopback listener, configure `[server] bind = "0.0.0.0:7099"` in
`hoard.toml`, or supply that bind address when restarting. Keep the same working directory
and data directory. The [example configuration](../../hoard.example.toml) documents the wider
options; remove its example source entries before using it for your own installation.
Managed node settings override the TOML node name and public URL; changing those fields
in TOML alone does not override an existing `node.json`.

Open the sharing address from another machine to check connectivity, host name resolution
and the listener before handing out links. **COPY PROJECT LINK** shares the selected view;
**COPY CLONE** gives a Git command. Models use the existing Hub-compatible download commands.

Anyone who can reach this node can browse and download its resources. `HOARD_NODE_TOKEN`,
set in the server environment or `.env`, protects changes to the catalog and settings;
it does not make resource reads private. With no token configured, changes are also open
to anyone who can reach the node. This is network resource sharing, not an account-based
repository hosting service.

## Keep it useful

Add more folders from Sources whenever you need them. **REMOVE**, followed by **CONFIRM**,
stops indexing and sharing a source and leaves its original files on disk. Removal persists
after restart. Add it again to restore it. Dismissing completed scans only clears Activity
history. After removing the last source and resource, the Sources setup guidance becomes
useful again; there is no permanent completed-tour flag to reset.
