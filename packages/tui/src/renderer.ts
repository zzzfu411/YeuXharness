import type { EventEnvelope, RuntimeDiagnosticNotification } from "@yeux/protocol";

import {
  DEFAULT_THEME,
  type DetectTerminalCapabilitiesOptions,
  detectTerminalCapabilities,
  glyph,
  paint,
  sequenceLabel,
  statusGlyphName,
  statusLabel,
  colorRoleForStatus,
  type TerminalCapabilities,
  type ThemeName,
} from "./aesthetic.js";
import { sanitizeTerminalLine, sanitizeTerminalText } from "./terminal.js";

export class EventRenderer {
  readonly #jsonl: boolean;
  readonly #capabilities: TerminalCapabilities;
  readonly #theme: ThemeName;
  readonly #write: (text: string) => void;
  #streamingText = false;

  public constructor(options: {
    readonly jsonl?: boolean;
    readonly capabilities?: TerminalCapabilities;
    readonly color?: boolean | DetectTerminalCapabilitiesOptions["color"];
    readonly ascii?: boolean;
    readonly columns?: number;
    readonly plain?: boolean;
    readonly reducedMotion?: boolean;
    readonly isTTY?: boolean;
    readonly env?: Readonly<Record<string, string | undefined>>;
    readonly theme?: ThemeName;
    readonly write?: (text: string) => void;
  } = {}) {
    this.#jsonl = options.jsonl ?? false;
    this.#capabilities = options.capabilities ?? detectTerminalCapabilities(
      {
        ...(options.color === undefined ? {} : { color: options.color }),
        ...(options.ascii === undefined ? {} : { ascii: options.ascii }),
        ...(options.columns === undefined ? {} : { columns: options.columns }),
        ...(options.plain === undefined ? {} : { plain: options.plain }),
        ...(options.reducedMotion === undefined ? {} : { reducedMotion: options.reducedMotion }),
        ...(options.isTTY === undefined ? {} : { isTTY: options.isTTY }),
        ...(options.env === undefined ? {} : { env: options.env }),
      },
    );
    this.#theme = options.theme ?? DEFAULT_THEME;
    this.#write = options.write ?? ((text) => process.stdout.write(text));
  }

  public get capabilities(): TerminalCapabilities {
    return this.#capabilities;
  }

  public get theme(): ThemeName {
    return this.#theme;
  }

  public render(event: EventEnvelope): void {
    if (this.#jsonl) {
      this.#write(`${JSON.stringify(event)}\n`);
      return;
    }

    const text = event.kind === "model/event" ? modelDeltaText(event.payload) : undefined;
    if (text !== undefined) {
      this.#write(sanitizeTerminalText(text));
      this.#streamingText = true;
      return;
    }

    if (this.#streamingText) {
      this.#write("\n");
      this.#streamingText = false;
    }

    const formatted = formatAestheticEvent(event, {
      capabilities: this.#capabilities,
      theme: this.#theme,
    });
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
    const text = `[diagnostic:${diagnostic.code}] ${diagnostic.message}${sequence}`;
    this.#write(`${paintTerminalText(text, "warning", this.#capabilities, this.#theme)}\n`);
  }
}

/**
 * Backwards-compatible compact formatter retained for callers that used the
 * original v0.1 API.  New terminal output goes through
 * `formatAestheticEvent`, below, so the line renderer can evolve without
 * changing protocol payloads or JSONL output.
 */
export function formatEvent(event: EventEnvelope, color = false): string | undefined {
  const text = payloadText(event.payload);
  switch (event.kind) {
    case "turn/started":
      return paintTerminalText(
        `[start] turn ${event.turn_id ?? ""}`,
        "muted",
        legacyCapabilities(color),
      );
    case "turn/state_changed": {
      const state = event.payload["to"];
      if (state === "completed") return paintTerminalText("[ok] completed", "success", legacyCapabilities(color));
      if (state === "cancelled") return paintTerminalText("[cancelled]", "warning", legacyCapabilities(color));
      if (state === "failed") {
        return paintTerminalText(
          `[error] failed${text === undefined ? "" : `: ${text}`}`,
          "danger",
          legacyCapabilities(color),
        );
      }
      if (state === "waiting_for_approval") {
        return paintTerminalText("? waiting for approval", "warning", legacyCapabilities(color));
      }
      return paintTerminalText(`turn: ${String(state)}`, "muted", legacyCapabilities(color));
    }
    case "tool/proposed":
      return paintTerminalText(`[tool] ${text ?? "proposed"}`, "focus", legacyCapabilities(color));
    case "tool/state_changed":
      return paintTerminalText(
        `tool: ${String(event.payload["to"] ?? "updated")}`,
        "muted",
        legacyCapabilities(color),
      );
    case "tool/reconciled":
      return paintTerminalText(
        `tool: reconciled${text === undefined ? "" : `: ${text}`}`,
        "warning",
        legacyCapabilities(color),
      );
    case "runtime/diagnostic":
      return paintTerminalText(text ?? JSON.stringify(event.payload), "muted", legacyCapabilities(color));
    case "model/event":
    case "model/requested":
      return undefined;
    default:
      return paintTerminalText(
        `[${event.kind}]${text === undefined ? "" : ` ${text}`}`,
        "muted",
        legacyCapabilities(color),
      );
  }
}

export interface AestheticFormatOptions {
  readonly capabilities?: TerminalCapabilities;
  readonly theme?: ThemeName;
}

/** Render an event as the YeuX Paper Signal timeline line. */
export function formatAestheticEvent(
  event: EventEnvelope,
  options: AestheticFormatOptions = {},
): string | undefined {
  const capabilities = options.capabilities ?? detectTerminalCapabilities();
  const theme = options.theme ?? DEFAULT_THEME;
  const text = payloadText(event.payload);
  const seq = sequenceLabel(event.seq);

  if (event.kind === "model/event" || event.kind === "model/requested") return undefined;

  if (event.kind === "turn/started") {
    return timelineLine(
      seq,
      event,
      glyph("accepted", capabilities),
      `START TURN${event.turn_id === undefined ? "" : ` ${singleLine(event.turn_id)}`}`,
      "muted",
      capabilities,
      theme,
    );
  }

  if (event.kind === "turn/state_changed") {
    const state = typeof event.payload["to"] === "string" ? event.payload["to"] : "unknown";
    const summary = text === undefined ? "" : ` · ${singleLine(text)}`;
    return timelineLine(
      seq,
      event,
      glyph(statusGlyphName(state), capabilities),
      `${statusLabel(state)}${summary}`,
      colorRoleForStatus(state),
      capabilities,
      theme,
    );
  }

  if (event.kind === "tool/proposed") {
    const summary = text === undefined ? "" : ` · ${singleLine(text)}`;
    return timelineLine(
      seq,
      event,
      glyph("toolProposed", capabilities),
      `TOOL PROPOSED${summary}`,
      "focus",
      capabilities,
      theme,
      true,
    );
  }

  if (event.kind === "tool/state_changed") {
    const state = event.payload["to"] === undefined ? "UPDATED" : String(event.payload["to"]).replaceAll("_", " ").toUpperCase();
    return timelineLine(
      seq,
      event,
      glyph("rail", capabilities),
      `TOOL ${singleLine(state)}`,
      "muted",
      capabilities,
      theme,
      true,
    );
  }

  if (event.kind === "tool/reconciled") {
    const summary = text === undefined ? "" : ` · ${singleLine(text)}`;
    return timelineLine(
      seq,
      event,
      glyph("unknown", capabilities),
      `TOOL RECONCILED${summary}`,
      "warning",
      capabilities,
      theme,
      true,
    );
  }

  if (event.kind === "runtime/diagnostic") {
    const diagnostic = text === undefined ? JSON.stringify(event.payload) : text;
    return timelineLine(
      seq,
      event,
      glyph("unknown", capabilities),
      `DIAGNOSTIC · ${singleLine(diagnostic)}`,
      "warning",
      capabilities,
      theme,
    );
  }

  const kind = singleLine(event.kind);
  return timelineLine(
    seq,
    event,
    glyph("beat", capabilities),
    `[${kind}]${text === undefined ? "" : ` ${singleLine(text)}`}`,
    "muted",
    capabilities,
    theme,
  );
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

function legacyCapabilities(color: boolean): Pick<TerminalCapabilities, "colorDepth"> {
  return { colorDepth: color ? "ansi16" : "none" };
}

function paintTerminalText(
  text: string,
  role: Parameters<typeof paint>[1],
  capabilities: Pick<TerminalCapabilities, "colorDepth">,
  theme: ThemeName = DEFAULT_THEME,
): string {
  return paint(sanitizeTerminalLine(text), role, capabilities, theme);
}

function timelineLine(
  seq: string,
  event: EventEnvelope,
  statusGlyph: string,
  body: string,
  role: Parameters<typeof paint>[1],
  capabilities: TerminalCapabilities,
  theme: ThemeName,
  branch = false,
): string {
  const prefix = capabilities.columns < 40
    ? seq
    : `${seq} ${glyph(branch ? "branch" : "rail", capabilities)}`;
  const safeBody = sanitizeTerminalText(body).replace(/[\r\n\t]+/g, " ");
  return paint(`${prefix} ${statusGlyph} ${safeBody}`, role, capabilities, theme);
}

function singleLine(value: string): string {
  return sanitizeTerminalLine(value);
}
