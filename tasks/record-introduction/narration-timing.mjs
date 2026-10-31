import assert from "node:assert/strict";

export function chapterTimings(diagnostics, duration, chapters) {
    assert(Number.isFinite(duration) && duration > 0, "Narration duration must be positive");
    assert(Number.isInteger(chapters) && chapters > 0, "Narration needs at least one chapter");
    const pauses = [];
    let start = null;
    for (const match of diagnostics.matchAll(/silence_(start|end):\s*([\d.]+)/g)) {
        const time = Number(match[2]);
        if (match[1] === "start") start = time;
        else if (start !== null) {
            if (start > 0.25 && time < duration - 0.25 && time - start >= 0.8) {
                pauses.push({ start, end: time });
            }
            start = null;
        }
    }
    assert.equal(pauses.length, chapters - 1,
        `Expected ${chapters - 1} paragraph pauses of at least 0.8 seconds; found ${pauses.length}. Supply audio with a clear pause between each paragraph.`);
    const boundaries = [0, ...pauses.map(pause => (pause.start + pause.end) / 2), duration];
    return boundaries.slice(0, -1).map((start, index) => {
        const end = boundaries[index + 1];
        assert(end > start, "Narration chapter boundaries must increase");
        return { start, end };
    });
}
