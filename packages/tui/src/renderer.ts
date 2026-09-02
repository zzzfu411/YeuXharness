import type {
  CapabilityGrant,
  EventEnvelope,
  RuntimeDiagnosticNotification,
  RuntimeMode,
} from "@yeux/protocol";

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
  readonly #recentEvents: EventEnvelope[] = [];

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

    this.rememberEvents([event]);

    const text = event.kind === "model/event" ? modelDeltaText(event.payload) : undefined;
    if (text !== undefined) {
      this.#write(`${formatAestheticModelEvent(event, text, {
        capabilities: this.#capabilities,
        theme: this.#theme,
      })}\n`);
      return;
    }

    const formatted = formatAestheticEvent(event, {
      capabilities: this.#capabilities,
      theme: this.#theme,
    });
    if (formatted !== undefined) this.#write(`${formatted}\n`);
  }

  public renderDiagnostic(diagnostic: RuntimeDiagnosticNotification): void {
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

  /** Emit the identity block once a thread and provider have been resolved. */
  public renderSessionBar(state: SessionBarState): void {
    if (this.#jsonl) return;
    this.#write(`${formatSessionBar(state, {
      capabilities: this.#capabilities,
      theme: this.#theme,
    })}\n`);
  }

  /** Seed the Inspector from a resumed thread without replaying its timeline. */
  public rememberEvents(events: readonly EventEnvelope[]): void {
    for (const event of events) {
      this.#recentEvents.push(event);
      if (this.#recentEvents.length > 12) this.#recentEvents.shift();
    }
  }

  /** Emit a compact policy/event readout for an operator or test fixture. */
  public renderInspector(policy: CapabilityGrant | undefined = undefined): void {
    if (this.#jsonl) return;
    this.#write(`${formatInspector({
      ...(policy === undefined ? {} : { policy }),
      events: this.#recentEvents,
    }, {
      capabilities: this.#capabilities,
      theme: this.#theme,
    })}\n`);
  }
}

export interface SessionBarState {
  readonly cwd: string;
  readonly thread: string;
  readonly mode: RuntimeMode | string;
  readonly model: string;
  readonly trust?: string;
  readonly transport?: string;
}

export interface PresenterFormatOptions {
  readonly capabilities?: TerminalCapabilities;
  readonly theme?: ThemeName;
}

/** The identity bar is deliberately explicit: it is never replaced by a prompt. */
export function formatSessionBar(
  state: SessionBarState,
  options: PresenterFormatOptions = {},
): string {
  const capabilities = options.capabilities ?? detectTerminalCapabilities();
  const theme = options.theme ?? DEFAULT_THEME;
  const required = [
    `CWD ${singleLine(state.cwd)}`,
    `THREAD ${singleLine(state.thread)}`,
    `MODE ${singleLine(state.mode).toUpperCase()}`,
    `MODEL ${singleLine(state.model)}`,
  ];
  if (state.trust !== undefined) required.push(`TRUST ${singleLine(state.trust).toUpperCase()}`);
  if (state.transport !== undefined) required.push(`TRANSPORT ${singleLine(state.transport)}`);
  const body = `${glyph("brandCompact", capabilities)}  YeuX / HARNESS   ${required.join("   ")}`;
  return paint(sanitizeTerminalLine(body), "text", capabilities, theme);
}

export interface InspectorState {
  readonly policy?: CapabilityGrant | Record<string, unknown>;
  readonly events: readonly EventEnvelope[];
}

/** Render the current capability policy and a bounded recent-event ledger tail. */
export function formatInspector(
  state: InspectorState,
  options: PresenterFormatOptions = {},
): string {
  const capabilities = options.capabilities ?? detectTerminalCapabilities();
  const theme = options.theme ?? DEFAULT_THEME;
  const lines = [
    "INSPECTOR",
    `POLICY · ${state.policy === undefined ? "unresolved" : formatPolicy(state.policy)}`,
    "RECENT EVENTS",
  ];
  if (state.events.length === 0) {
    lines.push("  none");
  } else {
    for (const event of state.events.slice(-12)) {
      const summary = payloadText(event.payload);
      lines.push(`  ${sequenceLabel(event.seq)} ${glyph("rail", capabilities)} ${singleLine(event.kind)}${summary === undefined ? "" : ` · ${singleLine(summary)}`}`);
    }
  }
  return lines
    .map((line, index) => paint(sanitizeTerminalLine(line), index === 0 ? "focus" : "muted", capabilities, theme))
    .join("\n");
}

function formatPolicy(policy: CapabilityGrant | Record<string, unknown>): string {
  const record = policy as Record<string, unknown>;
  const mode = typeof record.mode === "string" ? record.mode.toUpperCase() : "UNKNOWN";
  const read = formatPolicyValue(record.filesystem_read);
  const write = formatPolicyValue(record.filesystem_write);
  const remove = formatPolicyValue(record.filesystem_delete);
  const process = record.process === true ? "yes" : "none";
  const network = formatPolicyValue(record.network);
  const secrets = formatPolicyValue(record.secrets);
  const external = formatPolicyValue(record.external_write ?? record.external_writes);
  return `MODE ${mode} · filesystem_read ${read} · filesystem_write ${write} · filesystem_delete ${remove} · process ${process} · network ${network} · secrets ${secrets} · external_write ${external}`;
}

// Names used by screen-mode callers; keeping aliases avoids a second presenter contract.
export const renderSessionBar = formatSessionBar;
export const formatInspectorBlock = formatInspector;
export const renderInspector = formatInspector;

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
      if (event.payload["to"] === "unknown") {
        return paintTerminalText(
          "[unknown] reconciliation required",
          "danger",
          legacyCapabilities(color),
        );
      }
      return paintTerminalText(
        `tool: ${String(event.payload["to"] ?? "updated")}`,
        "muted",
        legacyCapabilities(color),
      );
    case "tool/reconciled":
      return paintTerminalText(
        `[unknown] reconciliation required${text === undefined ? "" : `: ${text}`}`,
        "danger",
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

  if (event.kind === "model/event") {
    const delta = modelDeltaText(event.payload);
    if (delta !== undefined) {
      return formatAestheticModelEvent(event, delta, { capabilities, theme });
    }
    return timelineLine(
      seq,
      event,
      glyph("model", capabilities),
      modelEventSummary(event.payload),
      "focus",
      capabilities,
      theme,
    );
  }

  if (event.kind === "model/requested") {
    return timelineLine(
      seq,
      event,
      glyph("model", capabilities),
      "MODEL REQUESTED",
      "focus",
      capabilities,
      theme,
    );
  }

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
    if (state === "UNKNOWN") {
      return timelineLine(
        seq,
        event,
        glyph("unknown", capabilities),
        `UNKNOWN · RECONCILIATION REQUIRED${text === undefined ? "" : ` · ${singleLine(text)}`}`,
        "danger",
        capabilities,
        theme,
        true,
      );
    }
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
      `UNKNOWN · RECONCILIATION REQUIRED${summary}`,
      "danger",
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

export function modelDeltaText(payload: unknown): string | undefined {
  if (typeof payload !== "object" || payload === null) return undefined;
  const modelEvent = (payload as Record<string, unknown>)["model_event"];
  if (typeof modelEvent !== "object" || modelEvent === null) return undefined;
  const record = modelEvent as Record<string, unknown>;
  return record["type"] === "text_delta" && typeof record["text"] === "string"
    ? record["text"]
    : undefined;
}

function modelEventSummary(payload: unknown): string {
  if (typeof payload !== "object" || payload === null) return "MODEL EVENT";
  const modelEvent = (payload as Record<string, unknown>)["model_event"];
  if (typeof modelEvent !== "object" || modelEvent === null) return "MODEL EVENT";
  const type = (modelEvent as Record<string, unknown>)["type"];
  return typeof type === "string" ? `MODEL ${type.replaceAll("_", " ").toUpperCase()}` : "MODEL EVENT";
}

export function formatAestheticModelEvent(
  event: EventEnvelope,
  text: string,
  options: AestheticFormatOptions = {},
): string {
  const capabilities = options.capabilities ?? detectTerminalCapabilities();
  const theme = options.theme ?? DEFAULT_THEME;
  const body = `STREAMING · ${text}`;
  return timelineLine(
    sequenceLabel(event.seq),
    event,
    glyph("streaming", capabilities),
    body,
    "focus",
    capabilities,
    theme,
  );
}

export const renderTimelineEvent = formatAestheticEvent;
export const formatTimelineEvent = formatAestheticEvent;

function payloadText(payload: unknown): string | undefined {
  if (typeof payload !== "object" || payload === null) return undefined;
  for (const key of ["text", "message", "summary", "error"] as const) {
    const value = (payload as Record<string, unknown>)[key];
    if (typeof value === "string") return value;
  }
  return undefined;
}

function formatPolicyValue(value: unknown): string {
  if (Array.isArray(value)) {
    if (value.length === 0) return "none";
    return value.map((entry) => singleLine(String(entry))).join(", ");
  }
  if (typeof value === "string") return singleLine(value);
  return value === true ? "yes" : "none";
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
