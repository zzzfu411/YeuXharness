import { afterEach, describe, expect, it, vi } from "vitest";

import type { EventEnvelope } from "@yeux/protocol";
import type { TerminalCapabilities } from "../src/aesthetic.js";
import {
  EventRenderer,
  HUMAN_KEYBOARD_DELAY_MS,
  TYPEWRITER_CARET,
  TYPEWRITER_CJK_DELAY_MS,
  TYPEWRITER_LATIN_DELAY_MS,
  TYPEWRITER_SEGMENT_CAP_MS,
  modelInkMotionAllowed,
  typewriterDelayMs,
} from "../src/renderer.js";

const LIVE: TerminalCapabilities = {
  isTTY: true,
  columns: 80,
  colorDepth: "truecolor",
  unicode: true,
  plain: false,
  reducedMotion: false,
};

const PREFIX = "0001 │ ≈ STREAMING · ";

const event: EventEnvelope = {
  schema_version: { major: 1, minor: 0 },
  event_id: "evt-ink",
  thread_id: "thread-1",
  turn_id: "turn-1",
  agent_id: "agent-root",
  seq: 1,
  time: "2026-09-02T12:00:00Z",
  kind: "model/event",
  payload: { model_event: { type: "text_delta", text: "Hey" } },
};

function modelEvent(text: string, seq = 1): EventEnvelope {
  return {
    ...event,
    event_id: `evt-${seq}`,
    seq,
    payload: { model_event: { type: "text_delta", text } },
  };
}

/** Apply backspaces so typewriter caret replacements become the visible line. */
function visible(text: string): string {
  const chars: string[] = [];
  for (const char of text) {
    if (char === "\b") chars.pop();
    else chars.push(char);
  }
  return chars.join("");
}

describe("live model typewriter", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("uses the locked Latin, CJK and segment-cap constants", () => {
    expect(TYPEWRITER_LATIN_DELAY_MS).toBe(18);
    expect(TYPEWRITER_CJK_DELAY_MS).toBe(24);
    expect(TYPEWRITER_SEGMENT_CAP_MS).toBe(600);
    expect(HUMAN_KEYBOARD_DELAY_MS).toBe(0);
    expect(typewriterDelayMs("A")).toBe(TYPEWRITER_LATIN_DELAY_MS);
    expect(typewriterDelayMs("你")).toBe(TYPEWRITER_CJK_DELAY_MS);
    expect(typewriterDelayMs("あ")).toBe(TYPEWRITER_CJK_DELAY_MS);
    expect(typewriterDelayMs("한")).toBe(TYPEWRITER_CJK_DELAY_MS);
  });

  it("walks Latin ink at 18ms with a static graphite caret", async () => {
    vi.useFakeTimers();
    let output = "";
    const renderer = new EventRenderer({
      capabilities: LIVE,
      theme: "mono",
      write: (text) => {
        output += text;
      },
    });

    const done = renderer.render(modelEvent("Hey"));
    expect(visible(output)).toBe(`${PREFIX}H${TYPEWRITER_CARET}`);
    expect(visible(output)).not.toContain("Hey");
    expect(output).not.toContain("█");
    expect(output).not.toContain("\u0007");

    await vi.advanceTimersByTimeAsync(TYPEWRITER_LATIN_DELAY_MS - 1);
    expect(visible(output)).toBe(`${PREFIX}H${TYPEWRITER_CARET}`);

    await vi.advanceTimersByTimeAsync(1);
    expect(visible(output)).toBe(`${PREFIX}He${TYPEWRITER_CARET}`);

    await vi.advanceTimersByTimeAsync(TYPEWRITER_LATIN_DELAY_MS);
    await done;
    expect(visible(output)).toBe(`${PREFIX}Hey\n`);
    expect(visible(output).endsWith(`${TYPEWRITER_CARET}\n`)).toBe(false);
  });

  it("walks CJK ink at 24ms, not the Latin 18ms", async () => {
    vi.useFakeTimers();
    let output = "";
    const renderer = new EventRenderer({
      capabilities: LIVE,
      theme: "mono",
      write: (text) => {
        output += text;
      },
    });

    const done = renderer.render(modelEvent("汉字"));
    expect(visible(output)).toBe(`${PREFIX}汉${TYPEWRITER_CARET}`);

    await vi.advanceTimersByTimeAsync(TYPEWRITER_LATIN_DELAY_MS);
    expect(visible(output)).toBe(`${PREFIX}汉${TYPEWRITER_CARET}`);
    expect(visible(output)).not.toContain("汉字");

    await vi.advanceTimersByTimeAsync(TYPEWRITER_CJK_DELAY_MS - TYPEWRITER_LATIN_DELAY_MS);
    await done;
    expect(visible(output)).toBe(`${PREFIX}汉字\n`);
  });

  it("drops leftover ink instantly after the 600ms segment cap", async () => {
    vi.useFakeTimers();
    let output = "";
    const renderer = new EventRenderer({
      capabilities: LIVE,
      theme: "mono",
      write: (text) => {
        output += text;
      },
    });
    const ink = "L".repeat(50);
    const delayedChars = Math.floor(TYPEWRITER_SEGMENT_CAP_MS / TYPEWRITER_LATIN_DELAY_MS) + 1;

    const done = renderer.render(modelEvent(ink));
    expect(visible(output)).toBe(`${PREFIX}L${TYPEWRITER_CARET}`);
    expect(visible(output)).not.toContain(ink);

    await vi.advanceTimersByTimeAsync(TYPEWRITER_LATIN_DELAY_MS);
    expect(visible(output).includes(ink)).toBe(false);

    await vi.advanceTimersByTimeAsync(TYPEWRITER_SEGMENT_CAP_MS);
    await done;
    expect(visible(output)).toBe(`${PREFIX}${ink}\n`);
    expect(visible(output)).not.toContain(`${TYPEWRITER_CARET}\n`);
    expect(delayedChars).toBe(34);
  });

  it("emits the finished line once for reducedMotion, jsonl, plain and NO_COLOR", async () => {
    const cases = [
      {
        name: "reducedMotion",
        options: { capabilities: { ...LIVE, reducedMotion: true }, theme: "mono" as const },
        expectText: `${PREFIX}Hey\n`,
      },
      {
        name: "plain",
        options: { capabilities: { ...LIVE, plain: true, reducedMotion: true }, theme: "mono" as const },
        expectText: `${PREFIX}Hey\n`,
      },
      {
        name: "NO_COLOR",
        options: { capabilities: { ...LIVE, colorDepth: "none" as const }, theme: "mono" as const },
        expectText: `${PREFIX}Hey\n`,
      },
      {
        name: "jsonl",
        options: { jsonl: true, capabilities: LIVE },
        expectText: `${JSON.stringify(modelEvent("Hey"))}\n`,
      },
    ];

    for (const testCase of cases) {
      const writes: string[] = [];
      const renderer = new EventRenderer({
        ...testCase.options,
        write: (text) => {
          writes.push(text);
        },
      });
      await renderer.render(modelEvent("Hey"));
      expect(writes, testCase.name).toEqual([testCase.expectText]);
      expect(writes.join(""), testCase.name).not.toContain("\b");
    }
  });

  it("paints walking ink and caret in graphite while STREAMING stays focus", async () => {
    let output = "";
    const renderer = new EventRenderer({
      capabilities: LIVE,
      write: (text) => {
        output += text;
      },
    });
    await renderer.render(modelEvent("H"));
    expect(output).toContain("STREAMING");
    expect(output).toContain("38;2;49;86;107");
    expect(output).toContain("38;2;27;24;21");
    expect(output).toContain(TYPEWRITER_CARET);
    expect(output).not.toContain("█");
    expect(output).not.toContain("\u0007");
  });

  it("does not enable motion for NO_COLOR, pipes or forced-off replay", () => {
    expect(modelInkMotionAllowed({ ...LIVE, colorDepth: "none" })).toBe(false);
    expect(modelInkMotionAllowed({ ...LIVE, isTTY: false, plain: true, reducedMotion: true })).toBe(false);
    expect(modelInkMotionAllowed(LIVE, { typewriter: false })).toBe(false);
    expect(modelInkMotionAllowed(LIVE, { jsonl: true })).toBe(false);
    expect(modelInkMotionAllowed(LIVE)).toBe(true);
  });
});
