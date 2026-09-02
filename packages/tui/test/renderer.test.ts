import { describe, expect, it } from "vitest";

import { EventRenderer, formatEvent } from "../src/renderer.js";

const event = {
  schema_version: { major: 1, minor: 0 },
  event_id: "evt-1",
  thread_id: "thread-1",
  turn_id: "turn-1",
  agent_id: "agent-root",
  seq: 1,
  time: "2026-08-30T12:00:00Z",
  kind: "turn/state_changed",
  payload: { turn_id: "turn-1", from: "streaming", to: "completed" },
} as const;

describe("EventRenderer", () => {
  it("emits one event envelope per line in JSONL mode", () => {
    let output = "";
    const renderer = new EventRenderer({
      jsonl: true,
      color: false,
      write: (text) => {
        output += text;
      },
    });
    renderer.render(event);
    expect(output).toBe(`${JSON.stringify(event)}\n`);
  });

  it("formats terminal events without ANSI when color is off", () => {
    expect(formatEvent(event, false)).toBe("[ok] completed");
  });

  it("uses the Paper timeline tokens by default in a rich terminal", () => {
    let output = "";
    const renderer = new EventRenderer({
      env: {
        TERM: "xterm-256color",
        COLORTERM: "truecolor",
        LANG: "en_US.UTF-8",
      },
      isTTY: true,
      columns: 120,
      write: (text) => {
        output += text;
      },
    });

    renderer.render(event);

    expect(renderer.theme).toBe("paper");
    expect(output).toBe(
      "\u001b[38;2;53;95;67m\u001b[48;2;216;211;204m0001 │ ✓ COMPLETED\u001b[0m\n",
    );
  });

  it("falls back to an uncoloured ASCII timeline for TERM=dumb", () => {
    let output = "";
    const renderer = new EventRenderer({
      env: { TERM: "dumb", LANG: "en_US.UTF-8" },
      isTTY: true,
      color: "truecolor",
      write: (text) => {
        output += text;
      },
    });

    renderer.render(event);

    expect(renderer.capabilities).toMatchObject({
      colorDepth: "none",
      unicode: false,
      plain: true,
    });
    expect(output).toBe("0001 | OK COMPLETED\n");
  });

  it("preserves transport diagnostics in JSONL mode", () => {
    let output = "";
    const renderer = new EventRenderer({
      jsonl: true,
      color: false,
      write: (text) => {
        output += text;
      },
    });
    renderer.renderDiagnostic({
      code: "event_sequence_gap",
      message: "reconnect\u001b[2J",
      recoverable: true,
      expected_seq: 4,
      actual_seq: 6,
    });
    expect(JSON.parse(output)).toEqual({
      jsonrpc: "2.0",
      method: "runtime/diagnostic",
      params: {
        code: "event_sequence_gap",
        message: "reconnect\u001b[2J",
        recoverable: true,
        expected_seq: 4,
        actual_seq: 6,
      },
    });
  });

  it("sanitizes model and diagnostic text in terminal mode", () => {
    let output = "";
    const renderer = new EventRenderer({
      jsonl: false,
      color: false,
      ascii: true,
      write: (text) => {
        output += text;
      },
    });
    renderer.render({
      ...event,
      kind: "model/event",
      payload: {
        model_event: {
          type: "text_delta",
          text: "answer\u001b[2J\u001b]0;owned\u0007done",
        },
      },
    });
    renderer.renderDiagnostic({
      code: "gap\u001b[31m",
      message: "retry\u001b[0m",
      recoverable: true,
    });

    expect(output).toBe("0001 | ~ STREAMING · answerdone\n[diagnostic:gap] retry\n");
    expect(output).not.toContain("\u001b");
    expect(output).not.toContain("\u0007");
  });

  it("preserves untrusted payload text verbatim in JSONL mode", () => {
    let output = "";
    const renderer = new EventRenderer({
      jsonl: true,
      color: false,
      write: (text) => {
        output += text;
      },
    });
    const rawText = "answer\u001b[2J\u001b]0;owned\u0007done";
    const rawEvent = {
      ...event,
      kind: "model/event",
      payload: { model_event: { type: "text_delta", text: rawText } },
    } as const;
    renderer.render(rawEvent);

    expect(JSON.parse(output)).toEqual(rawEvent);
    expect(JSON.parse(output).payload.model_event.text).toBe(rawText);
  });

  it("keeps JSONL bytes independent of theme, colour and glyph settings", () => {
    const outputs: string[] = [];
    for (const options of [
      { theme: "nocturne" as const, color: "truecolor" as const, ascii: false },
      { theme: "paper" as const, color: "ansi256" as const, ascii: true },
      { theme: "mono" as const, color: false, ascii: true },
    ]) {
      let output = "";
      const renderer = new EventRenderer({
        jsonl: true,
        ...options,
        isTTY: true,
        env: { TERM: "xterm-256color", LANG: "en_US.UTF-8" },
        write: (text) => {
          output += text;
        },
      });
      renderer.render(event);
      outputs.push(output);
    }

    expect(new Set(outputs)).toEqual(new Set([`${JSON.stringify(event)}\n`]));
  });
});
