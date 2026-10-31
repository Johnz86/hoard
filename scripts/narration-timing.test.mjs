import assert from "node:assert/strict";
import test from "node:test";
import { chapterTimings } from "./narration-timing.mjs";

test("paragraph boundaries split pauses without discarding speech or counting edge silence", () => {
    const diagnostics = "silence_start: 0\nsilence_end: 1\nsilence_start: 4\nsilence_end: 5\nsilence_start: 9\nsilence_end: 10";
    assert.deepEqual(chapterTimings(diagnostics, 10, 2), [
        { start: 0, end: 4.5 }, { start: 4.5, end: 10 }
    ]);
});

test("ambiguous or missing paragraph pauses fail rather than guessing a chapter boundary", () => {
    assert.throws(() => chapterTimings("silence_start: 4\nsilence_end: 5", 10, 3), /Expected 2/);
    assert.throws(() => chapterTimings("silence_start: 4\nsilence_end: 5", 10, 1), /Expected 0/);
    assert.throws(() => chapterTimings("", Number.NaN, 1), /duration/);
});
