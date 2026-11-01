# Verify model collections

Run the collection journey against an isolated Hoard node with four tiny synthetic models
using the requested Qwen filenames. The fixture spans separate model folders and includes a
nested source path, two text encoders, and an unrelated file that must not be downloaded.
The model bytes are test fixtures, not working inference weights.

Use Node.js 24+, Git, the Hugging Face `hf` CLI, and a Chromium-based browser. Brave is the
default; `HOARD_BROWSER` selects another compatible binary. No npm installation is needed.

```sh
nice -n 10 cargo build --workspace -j 2
node tasks/verify-loadouts/verify.mjs
```

The entry point [verify.mjs](verify.mjs) creates a temporary catalog, model files, configuration,
client cache and browser profile. It uses a random loopback port, disables upstream/discovery,
uses a synthetic owner token, and stops only its own processes. Production sources, services
and network settings are untouched.

The journey creates the named collection through the UI; verifies both encoders survive;
edits the name and paths without replacing the drawer or losing drafts/focus/selection/scroll;
checks invalid-path recovery; runs the generated shell script through the real `hf` CLI;
compares all final paths and bytes; reruns to verify existing client files are preserved;
checks name/member search, role filtering, related membership and deep-link reload; and
cancels then confirms deletion while preserving model files. It checks related links in model
detail, inline owner-access recovery, unsaved-path download blocking and saved PowerShell
encoding. It captures both themes at 1180/1600px, including the shared Index in both densities,
table/cards and inspector on/off, and checks overflow and download-button visibility.

Successful runs print the evidence directory containing screenshots, a verification JSON,
download logs and server logs. Failed runs retain evidence for diagnosis. Run the broader
[first-run journey](../verify-first-run/README.md), project acquisition verification and
[Rust/release checks](../../CONTRIBUTING.md#verification) before handing off a UI change.
