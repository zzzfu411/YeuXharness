import type { EventEnvelope, RuntimeDiagnosticNotification } from "@yeux/protocol";

const ANSI = Object.freeze({
  reset: "\u001b[0m",
  dim: "\u001b[2m",
  green: "\u001b[32m",
  yellow: "\u001b[33m",
  red: "\u001b[31m",
  cyan: "\u001b[36m",
});

export class EventRenderer {
  readonly #jsonl: boolean;
  readonly #color: boolean;
  readonly #write: (text: string) => void;
  #streamingText = false;

  public constructor(options: {
    readonly jsonl?: boolean;
    readonly color?: boolean;
    readonly write?: (text: string) => void;
  } = {}) {
    this.#jsonl = options.jsonl ?? false;
    this.#color = options.color ?? process.stdout.isTTY === true;
    this.#write = options.write ?? ((text) => process.stdout.write(text));
  }

  public render(event: EventEnvelope): void {
    if (this.#jsonl) {
      this.#write(`${JSON.stringify(event)}\n`);
      return;
    }

    const text = event.kind === "model/event" ? modelDeltaText(event.payload) : undefined;
    if (text !== undefined) {
      this.#write(text);
      this.#streamingText = true;
      return;
    }

    if (this.#streamingText) {
      this.#write("\n");
      this.#streamingText = false;
    }

    const formatted = formatEvent(event, this.#color);
    if (formatted !== undefined) this.#write(`${formatted}\n`);
  }

  public renderDiagnostic(diagnostic: RuntimeDiagnosticNotification): void {
    if (this.#streamingText) {
      this.#write("\n");
      this.#streamingText = false;
    }
    if (this.#jsonl) {
      this.#write(
        `${JSON.stringify({
          jsonrpc: "2.0",
          method: "runtime/diagnostic",
          params: diagnostic,
        })}\n`,
      );
      return;
    }

    const sequence = diagnostic.expected_seq === undefined
      ? ""
      : ` (expected seq ${diagnostic.expected_seq}, received ${diagnostic.actual_seq ?? "unknown"})`;
    this.#write(
      `${paint(`[diagnostic:${diagnostic.code}] ${diagnostic.message}${sequence}`, ANSI.yellow, this.#color)}\n`,
    );
  }
}

export function formatEvent(event: EventEnvelope, color = false): string | undefined {
  const text = payloadText(event.payload);
  switch (event.kind) {
    case "turn/started":
      return paint(`[start] turn ${event.turn_id ?? ""}`, ANSI.dim, color);
    case "turn/state_changed": {
      const state = event.payload["to"];
      if (state === "completed") return paint("[ok] completed", ANSI.green, color);
      if (state === "cancelled") return paint("[cancelled]", ANSI.yellow, color);
      if (state === "failed") {
        return paint(`[error] failed${text === undefined ? "" : `: ${text}`}`, ANSI.red, color);
      }
      if (state === "waiting_for_approval") return paint("? waiting for approval", ANSI.yellow, color);
      return paint(`turn: ${String(state)}`, ANSI.dim, color);
    }
    case "tool/proposed":
      return paint(`[tool] ${text ?? "proposed"}`, ANSI.cyan, color);
    case "tool/state_changed":
      return paint(`tool: ${String(event.payload["to"] ?? "updated")}`, ANSI.dim, color);
    case "runtime/diagnostic":
      return paint(text ?? JSON.stringify(event.payload), ANSI.dim, color);
    case "model/event":
    case "model/requested":
      return undefined;
    default:
      return paint(`[${event.kind}]${text === undefined ? "" : ` ${text}`}`, ANSI.dim, color);
  }
}

function modelDeltaText(payload: unknown): string | undefined {
  if (typeof payload !== "object" || payload === null) return undefined;
  const modelEvent = (payload as Record<string, unknown>)["model_event"];
  if (typeof modelEvent !== "object" || modelEvent === null) return undefined;
  const record = modelEvent as Record<string, unknown>;
  return record["type"] === "text_delta" && typeof record["text"] === "string"
    ? record["text"]
    : undefined;
}

function payloadText(payload: unknown): string | undefined {
  if (typeof payload !== "object" || payload === null) return undefined;
  for (const key of ["text", "message", "summary", "error"] as const) {
    const value = (payload as Record<string, unknown>)[key];
    if (typeof value === "string") return value;
  }
  return undefined;
}

function paint(text: string, colorCode: string, enabled: boolean): string {
  return enabled ? `${colorCode}${text}${ANSI.reset}` : text;
}
