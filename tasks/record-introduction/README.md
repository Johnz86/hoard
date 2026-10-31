# Record the introduction

Reproduce the [published walkthrough](../../docs/user-guide/introduction.md) using the real
application, synthetic resources and the supplied narration. The task verifies find, review,
configuration/restart and actual Git cloning before publishing media.

## Task files

| Path | Purpose |
| --- | --- |
| [record.mjs](record.mjs) | Browser capture, verification, chapter synchronization and rendering |
| [storyboard.json](storyboard.json) | Ordered scenes, headings, on-screen summaries and narration |
| [inputs/narration.txt](inputs/narration.txt) | Spoken text, one paragraph per scene |
| [inputs/narration.wav](inputs/narration.wav) | Owner-supplied continuous voice recording |
| [narration-timing.mjs](narration-timing.mjs) | Paragraph pause validation and chapter boundaries |
| [narration-timing.test.mjs](narration-timing.test.mjs) | Focused timing regression checks |

## Requirements

Use Node.js 24+, Git, FFmpeg with libx264/drawtext, FFprobe and a Chromium-based browser.
The default browser is `/usr/bin/brave-browser`; pass `--browser` or set `HOARD_BROWSER` to
select another compatible binary. The built application is `target/debug/hoard` by default.
Vendored product fonts are reused. There are no npm packages or runtime downloads.

The recorder reads the WAV from disk and never calls an audio generation service. No speech
is synthesized or substituted; the original recording is preserved at its original speed.

## Run and review

```sh
nice -n 10 cargo build --workspace -j 2
node --test tasks/record-introduction/narration-timing.test.mjs
node tasks/record-introduction/record.mjs --verify
node tasks/record-introduction/record.mjs --output /tmp/hoard-introduction-candidate.mp4 --keep-work
```

`--verify` runs the UI/acquisition assertions without audio processing or encoding. Review a
candidate's picture, sound, transcript, captions and verification report. After acceptance:

```sh
node tasks/record-introduction/record.mjs --overwrite
```

The default output is `docs/media/introduction/hoard-introduction.mp4`, accompanied by
`hoard-introduction.vtt`, `-transcript.md`, `-poster.jpg` and `-verification.json`. Existing
outputs are protected unless `--overwrite` is explicit. `--binary`, `--browser`, `--storyboard`,
`--narration` and `--narration-text` override the corresponding inputs. `--help` lists options.

## Narration and pacing

The 11 text paragraphs correspond to the 11 storyboard scenes. Generate or record new speech
separately, with clear paragraph pauses of at least 0.8 seconds and shorter sentence pauses.
Keep text and storyboard synchronized. The recorder detects ten internal pauses at -35 dB
and places chapter boundaries at their midpoints. Ambiguous boundaries fail rather than
silently cutting speech.

The continuous audio receives volume normalization, a 0.45-second lead-in and a 1.25-second
ending hold. Captured actions and settled frames fit the chapter durations; audio is never
sped up. A 1920x1080 frame contains the native 1600x900 UI above a separate summary band.
VTT captions contain the spoken narration. Actual waits are verified and edited out; the
walkthrough is not a performance benchmark.

## Verification and isolation

The task creates its own temporary catalog, Git repository, tiny synthetic models and browser
profile. It disables upstream access and discovery, clears inherited tokens, binds an available
loopback port and stops only its own process groups, including on interruption. FFmpeg uses
limited threads and lowered priority. Failures retain scratch evidence; `--keep-work` retains
successful evidence too.

The report records executed journey checks, full audio/video decode and durations, audible
unclipped levels, source fingerprints, input WAV hash, paragraph boundaries and working-tree
status. Source changes during capture fail the run. These records describe the capture, not
an ongoing application status or a clean-commit claim when the checkout was dirty.

Run [first-run verification](../verify-first-run/README.md) and the applicable checks in
[CONTRIBUTING.md](../../CONTRIBUTING.md) when application behavior changes. The promotional
journey alone does not establish broader UX acceptance.
