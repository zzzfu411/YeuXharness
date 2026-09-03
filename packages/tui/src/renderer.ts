import type {
  CapabilityGrant,
  EventEnvelope,
  InvocationReconcileResult,
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
import {
  modelInkMotionAllowed,
  typewriterCaret,
  writeTypewriterInk,
} from "./typewriter.js";

export {
  HUMAN_KEYBOARD_DELAY_MS,
  TYPEWRITER_CARET,
  TYPEWRITER_CARET_ASCII,
  TYPEWRITER_CJK_DELAY_MS,
  TYPEWRITER_LATIN_DELAY_MS,
  TYPEWRITER_SEGMENT_CAP_MS,
  modelInkMotionAllowed,
  typewriterCaret,
  typewriterDelayMs,
  writeTypewriterInk,
} from "./typewriter.js";

export class EventRenderer {
  readonly #jsonl: boolean;
  readonly #typewriter: boolean | undefined;
  readonly #capabilities: TerminalCapabilities;
  readonly #theme: ThemeName;
  readonly #write: (text: string) => void;
  readonly #recentEvents: EventEnvelope[] = [];
  #chain: Promise<void> = Promise.resolve();
  #inFlight = 0;

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
    /** Force live typewriter on or off. Replay and JSONL always pass false. */
    readonly typewriter?: boolean;
  } = {}) {
    this.#jsonl = options.jsonl ?? false;
    this.#typewriter = options.typewriter;
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

  public render(event: EventEnvelope): Promise<void> {
    if (this.#jsonl) {
      this.#write(`${JSON.stringify(event)}\n`);
      return Promise.resolve();
    }

    this.rememberEvents([event]);

    const text = event.kind === "model/event" ? modelDeltaText(event.payload) : undefined;
    const motion = text !== undefined && this.#motionAllowed();
    if (!motion && this.#inFlight === 0) {
      this.#emitInstant(event, text);
      return Promise.resolve();
    }

    this.#inFlight += 1;
    const queued = this.#inFlight > 1;
    const run = queued ? this.#chain.then(() => this.#emit(event)) : this.#emit(event);
    this.#chain = run
      .catch(() => undefined)
      .finally(() => {
        this.#inFlight -= 1;
      });
    return run;
  }

  /** Wait until live model ink has finished walking onto the paper. */
  public async flush(): Promise<void> {
    await this.#chain;
  }

  #motionAllowed(): boolean {
    return modelInkMotionAllowed(this.#capabilities, {
      jsonl: this.#jsonl,
      ...(this.#typewriter === undefined ? {} : { typewriter: this.#typewriter }),
    });
  }

  async #emit(event: EventEnvelope): Promise<void> {
    const text = event.kind === "model/event" ? modelDeltaText(event.payload) : undefined;
    if (text !== undefined && this.#motionAllowed()) {
      await this.#emitTypewriter(event, text);
      return;
    }
    this.#emitInstant(event, text);
  }

  #emitInstant(event: EventEnvelope, text: string | undefined): void {
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

  async #emitTypewriter(event: EventEnvelope, text: string): Promise<void> {
    const ink = sanitizeTerminalText(text).replace(/[\r\n\t]+/g, " ");
    this.#write(formatAestheticModelEvent(event, "", {
      capabilities: this.#capabilities,
      theme: this.#theme,
    }));
    await writeTypewriterInk(ink, this.#write, {
      caret: paint(
        typewriterCaret(this.#capabilities.unicode),
        "text",
        this.#capabilities,
        this.#theme,
      ),
      paintInk: (chunk) => paint(chunk, "text", this.#capabilities, this.#theme),
      catchUp: () => this.#inFlight > 1,
    });
    this.#write("\n");
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

  /**
   * Render a control-plane result that is not itself an event envelope.
   * Keeping this shape explicit makes JSONL reconciliation output
   * distinguishable from replayed event records while preserving the same
   * stream for scripts and operators.
   */
  public renderReconciliationResult(result: InvocationReconcileResult): void {
    if (this.#jsonl) {
      this.#write(`${JSON.stringify({
        jsonrpc: "2.0",
        method: "runtime/reconciliation",
        params: result,
      })}\n`);
      return;
    }

    const summary = sanitizeTerminalLine(result.evidence.summary).replace(/[\r\n]+/g, " ");
    const text = `[reconciled] invocation ${result.invocationId} → ${result.state} · ${summary}`;
    this.#write(`${paintTerminalText(text, "success", this.#capabilities, this.#theme)}\n`);
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
  /** Non-empty write paths, or true. Required together with sandbox to display BUILD. */
  readonly writeGrant?: readonly string[] | boolean;
  /** Named OS sandbox is actually available. Required together with writeGrant to display BUILD. */
  readonly sandbox?: boolean;
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
    `MODE ${sessionBarModeLabel(state)}`,
    `MODEL ${singleLine(state.model)}`,
  ];
  if (state.trust !== undefined) required.push(`TRUST ${singleLine(state.trust).toUpperCase()}`);
  if (state.transport !== undefined) required.push(`TRANSPORT ${singleLine(state.transport)}`);
  const body = `${glyph("brandCompact", capabilities)}  YeuX / HARNESS   ${required.join("   ")}`;
  return paint(sanitizeTerminalLine(body), "text", capabilities, theme);
}

/**
 * Fail closed: MODE BUILD/OPERATE is only shown when a write grant and a
 * sandbox are both present. `--mode build` may still be requested; the Bar
 * must not claim BUILD when the client only has list/read/search.
 */
export function sessionBarModeLabel(
  state: Pick<SessionBarState, "mode" | "writeGrant" | "sandbox">,
): string {
  const requested = singleLine(String(state.mode)).toUpperCase();
  if (requested === "BUILD" || requested === "OPERATE") {
    if (!hasWriteGrant(state.writeGrant) || state.sandbox !== true) {
      return "OBSERVE";
    }
  }
  return requested;
}

function hasWriteGrant(writeGrant: SessionBarState["writeGrant"]): boolean {
  if (writeGrant === true) return true;
  return Array.isArray(writeGrant) && writeGrant.length > 0;
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
  const diffStart = lines.length;
  const hunk = latestUnifiedDiff(state.events);
  if (hunk !== undefined) {
    lines.push("UNIFIED DIFF");
    lines.push(...splitDiffLines(hunk).map((line) => `  ${line}`));
  }
  return lines
    .map((line, index) => paint(
      index > diffStart ? sanitizeTerminalText(line) : sanitizeTerminalLine(line),
      index === 0 ? "focus" : "muted",
      capabilities,
      theme,
    ))
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
  const writeTools = record.write_tools_available === true ? "yes" : "none";
  const processTools = record.process_tools_available === true ? "yes" : "none";
  const sandbox = typeof record.sandbox === "string" ? record.sandbox : "unavailable";
  return `MODE ${mode} · filesystem_read ${read} · filesystem_write ${write} · filesystem_delete ${remove} · process ${process} · network ${network} · secrets ${secrets} · external_write ${external} · write_tools ${writeTools} · process_tools ${processTools} · sandbox ${sandbox}`;
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
    const completed = timelineLine(
      seq,
      event,
      glyph("rail", capabilities),
      `TOOL ${singleLine(state)}`,
      "muted",
      capabilities,
      theme,
      true,
    );
    if (state === "COMPLETED") {
      const hunk = extractUnifiedDiff(event.payload);
      if (hunk !== undefined) {
        return `${completed}\n${formatDiffBlock(hunk, capabilities, theme)}`;
      }
    }
    return completed;
  }

  if (event.kind === "item/added") {
    const hunk = extractUnifiedDiff(event.payload);
    if (hunk !== undefined) {
      const header = timelineLine(
        seq,
        event,
        glyph("rail", capabilities),
        "TOOL RESULT",
        "muted",
        capabilities,
        theme,
        true,
      );
      return `${header}\n${formatDiffBlock(hunk, capabilities, theme)}`;
    }
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

export function extractUnifiedDiff(
  value: unknown,
  seen: Set<object> = new Set(),
  depth = 0,
): string | undefined {
  if (depth > 12) return undefined;
  if (Array.isArray(value)) {
    for (const item of value.slice(0, 32)) {
      const found = extractUnifiedDiff(item, seen, depth + 1);
      if (found !== undefined) return found;
    }
    return undefined;
  }
  if (typeof value !== "object" || value === null) return undefined;
  if (seen.has(value)) return undefined;
  seen.add(value);
  const record = value as Record<string, unknown>;
  for (const key of ["unified_diff", "unifiedDiff"] as const) {
    const candidate = record[key];
    if (typeof candidate === "string" && candidate.trim() !== "") {
      return candidate.length > 256 * 1024 ? candidate.slice(0, 256 * 1024) : candidate;
    }
  }
  let scanned = 0;
  for (const [, nested] of Object.entries(record)) {
    if (typeof nested === "string" && nested.length > 64 * 1024) continue;
    scanned += 1;
    if (scanned > 32) break;
    const found = extractUnifiedDiff(nested, seen, depth + 1);
    if (found !== undefined) return found;
  }
  return undefined;
}

function latestUnifiedDiff(events: readonly EventEnvelope[]): string | undefined {
  let found: string | undefined;
  for (const event of events) {
    const hunk = extractUnifiedDiff(event.payload);
    if (hunk !== undefined) found = hunk;
  }
  return found;
}

function splitDiffLines(hunk: string): string[] {
  const lines = hunk.split("\n");
  if (lines.at(-1) === "") lines.pop();
  return lines;
}

function formatDiffBlock(
  hunk: string,
  capabilities: TerminalCapabilities,
  theme: ThemeName,
): string {
  const rail = glyph("rail", capabilities);
  return splitDiffLines(hunk)
    .map((line) => paint(`${rail} ${sanitizeTerminalText(line)}`, "muted", capabilities, theme))
    .join("\n");
}

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
