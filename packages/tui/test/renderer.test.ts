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

    expect(output).toBe("answerdone\n[diagnostic:gap] retry\n");
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
});
