import { createInterface, type Interface } from "node:readline/promises";
import type { Readable, Writable } from "node:stream";

import {
  isRecord,
  type ApprovalRequestParams,
  type ApprovalRequestResult,
  type UserInputRequestParams,
  type UserInputRequestResult,
} from "@yeux/protocol";

import {
  DEFAULT_THEME,
  detectTerminalCapabilities,
  glyph,
  paint,
  type TerminalCapabilities,
  type ThemeName,
} from "./aesthetic.js";
import { sanitizeTerminalLine, sanitizeTerminalText } from "./terminal.js";

export interface TerminalPrompterOptions {
  readonly capabilities?: TerminalCapabilities;
  readonly ascii?: boolean;
  readonly columns?: number;
  readonly isTTY?: boolean;
  readonly theme?: ThemeName;
  readonly env?: Readonly<Record<string, string | undefined>>;
}

export class TerminalPrompter {
  readonly #readline: Interface;
  readonly #output: Writable;
  readonly #capabilities: TerminalCapabilities;
  readonly #theme: ThemeName;
  readonly #closedWaiters = new Set<() => void>();
  #queue: Promise<unknown> = Promise.resolve();
  #closed = false;
  #activeCommandAbort: AbortController | undefined;

  public constructor(
    input: Readable = process.stdin,
    output: Writable = process.stdout,
    options: TerminalPrompterOptions = {},
  ) {
    const terminal = (output as Writable & { readonly isTTY?: boolean }).isTTY === true;
    this.#readline = createInterface({ input, output, terminal });
    const markInputClosed = (): void => {
      this.#markClosed();
    };
    // `readline` does not consistently emit its own close event for a
    // non-TTY stream that reaches EOF before the first question. Observe the
    // source stream as well so pipes and scripted sessions can terminate
    // without a pending promise.
    input.once("end", markInputClosed);
    input.once("close", markInputClosed);
    this.#readline.once("close", () => {
      this.#markClosed();
    });
    this.#output = output;
    this.#capabilities = options.capabilities ?? detectTerminalCapabilities(
      {
        isTTY: options.isTTY ?? terminal,
        ...(options.ascii === undefined ? {} : { ascii: options.ascii }),
        ...(options.columns === undefined ? {} : { columns: options.columns }),
        ...(options.env === undefined ? {} : { env: options.env }),
      },
    );
    this.#theme = options.theme ?? DEFAULT_THEME;
  }

  public question(prompt: string): Promise<string> {
    this.#interruptCommandQuestion();
    return this.#enqueueQuestion(sanitizeTerminalText(prompt));
  }

  /** Renderer-owned interactive prompt; untrusted text never enters this path. */
  public async command(signal?: AbortSignal): Promise<string | undefined> {
    if (this.#closed) return undefined;
    const controller = new AbortController();
    const relayAbort = (): void => {
      controller.abort();
    };
    if (signal?.aborted === true) relayAbort();
    else signal?.addEventListener("abort", relayAbort, { once: true });
    this.#activeCommandAbort?.abort();
    this.#activeCommandAbort = controller;
    const prompt = paint(
      `yeux ${glyph("prompt", this.#capabilities)}`,
      "focus",
      this.#capabilities,
      this.#theme,
    );
    try {
      return await this.#questionUntilClose(`\n${prompt} `, controller.signal);
    } catch (error) {
      // readline rejects a pending question when stdin reaches EOF. EOF is a
      // normal terminal action, so callers can leave the session cleanly
      // without printing an internal error or waiting on an unsettled await.
      if (this.#closed || isReadlineClosedError(error)) return undefined;
      // Approval and user/input requests temporarily supersede the idle
      // command prompt. An empty line tells the active-turn input loop to
      // wait again after the higher-priority question is answered.
      if (controller.signal.aborted && isAbortError(error)) return "";
      throw error;
    } finally {
      signal?.removeEventListener("abort", relayAbort);
      if (this.#activeCommandAbort === controller) this.#activeCommandAbort = undefined;
    }
  }

  #enqueueQuestion(prompt: string, signal?: AbortSignal): Promise<string> {
    const operation = this.#queue.then(async () => signal === undefined
      ? await this.#readline.question(prompt)
      : await this.#readline.question(prompt, { signal }));
    this.#queue = operation.catch(() => undefined);
    return operation;
  }

  async #questionUntilClose(prompt: string, signal?: AbortSignal): Promise<string | undefined> {
    if (this.#closed) return undefined;
    let release: (() => void) | undefined;
    const closed = new Promise<void>((resolve) => {
      release = resolve;
      this.#closedWaiters.add(resolve);
    });
    try {
      return await Promise.race([
        this.#enqueueQuestion(prompt, signal),
        closed.then(() => undefined),
      ]);
    } finally {
      if (release !== undefined) this.#closedWaiters.delete(release);
    }
  }

  public async approval(params: ApprovalRequestParams): Promise<ApprovalRequestResult> {
    this.#interruptCommandQuestion();
    const safe = normalizeApprovalRequest(params);
    const safeArguments = sanitizeTerminalText(
      JSON.stringify(safe.invocation.normalized_arguments, null, 2),
    );
    this.#output.write(
      `\n${formatApprovalGate(safe, {
        capabilities: this.#capabilities,
        theme: this.#theme,
      })}\n`,
    );

    const rail = glyph("approvalRail", this.#capabilities);

    while (true) {
      const answer = await this.#questionUntilClose(
          paint(
            `${glyph("prompt", this.#capabilities)} approval: `,
            "approval",
            this.#capabilities,
            this.#theme,
          ),
        );
      // EOF is equivalent to the documented deny default. This keeps a
      // disconnected client from accidentally approving a side effect.
      const choice = parseApprovalChoice(answer ?? "");
      if (choice === "inspect") {
        const unified = unifiedDiffFromApproval(safe);
        if (unified !== undefined) {
          this.#output.write(
            `${paint("INSPECT · UNIFIED DIFF", "text", this.#capabilities, this.#theme)}\n` +
            `${framedLines(sanitizeTerminalText(unified), rail)}\n`,
          );
        } else {
          this.#output.write(
            `${paint("INSPECT · NORMALIZED ARGUMENTS", "text", this.#capabilities, this.#theme)}\n` +
            `${framedLines(safeArguments, rail)}\n`,
          );
        }
        continue;
      }
      return { approved: choice === "allow_once" };
    }
  }

  public async userInput(params: UserInputRequestParams): Promise<UserInputRequestResult> {
    if (!isRecord(params) || typeof params.prompt !== "string") {
      throw { code: -32602, message: "Invalid user/input request" };
    }
    this.#interruptCommandQuestion();
    const prompt = sanitizeTerminalLine(params.prompt);
    try {
      const answer = await this.#enqueueQuestion(`${prompt}: `);
      if (answer === undefined) throw { code: -32010, message: "Input closed before a response" };
      return { content: [{ type: "text", text: answer }] };
    } catch (error) {
      if (this.#closed || isReadlineClosedError(error)) {
        throw { code: -32010, message: "Input closed before a response" };
      }
      throw error;
    }
  }

  public close(): void {
    this.#markClosed();
  }

  #markClosed(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#interruptCommandQuestion();
    for (const resolve of this.#closedWaiters) resolve();
    this.#closedWaiters.clear();
    this.#readline.close();
  }

  #interruptCommandQuestion(): void {
    const controller = this.#activeCommandAbort;
    this.#activeCommandAbort = undefined;
    controller?.abort();
  }
}

function isReadlineClosedError(error: unknown): boolean {
  return error instanceof Error && (
    error.message.includes("readline was closed") ||
    error.message.includes("ERR_USE_AFTER_CLOSE")
  );
}

function isAbortError(error: unknown): boolean {
  return error instanceof Error && error.name === "AbortError";
}

export interface ApprovalGateFormatOptions {
  readonly capabilities?: TerminalCapabilities;
  readonly theme?: ThemeName;
}

/**
 * The gate is a presenter-only boundary. It is deliberately legible without
 * colour: a double-line marker, a risk glyph, explicit effects, and a deny
 * default all survive ASCII and NO_COLOR output.
 */
export function formatApprovalGate(
  params: ApprovalRequestParams,
  options: ApprovalGateFormatOptions = {},
): string {
  const safe = normalizeApprovalRequest(params);
  const capabilities = options.capabilities ?? detectTerminalCapabilities();
  const theme = options.theme ?? DEFAULT_THEME;
  const tool = sanitizeTerminalLine(`${safe.invocation.tool_id}@${safe.invocation.tool_version}`);
  const explanation = sanitizeTerminalText(safe.explanation);
  const invocationId = sanitizeTerminalLine(safe.invocation.invocation_id);
  const digest = sanitizeTerminalLine(safe.invocation.effect_digest);
  const safeEffects = sanitizeTerminalText(JSON.stringify(safe.invocation.effects, null, 2));
  const border = glyph("approvalStart", capabilities);
  const horizontal = glyph("approvalHorizontal", capabilities);
  const topRight = glyph("approvalTopRight", capabilities);
  const rail = glyph("approvalRail", capabilities);
  const end = glyph("approvalEnd", capabilities);
  const bottomRight = glyph("approvalBottomRight", capabilities);
  const headerContent = `${glyph("approval", capabilities)} APPROVAL REQUIRED · ${tool}`;
  const bodyContent = [
    ...explanation.split("\n").map((line) => `   ${line}`),
    ` binding ${digest} · invocation ${invocationId}`,
    " effects",
    ...safeEffects.split("\n").map((line) => `   ${line}`),
  ];
  const footerContent = "[a] ALLOW ONCE   [d] DENY (default)   [i] INSPECT";
  // Do not re-trim framed lines: sanitizeTerminalLine would eat trailing
  // padding and can drop the right rail that closes ╔╗ / ╚╝ (ASCII +-+).
  const topBaseWidth = displayWidth(border) + 1 + displayWidth(headerContent) + 1 + displayWidth(topRight);
  const footerBaseWidth = displayWidth(end) + 1 + displayWidth(footerContent) + 1 + displayWidth(bottomRight);
  const bodyBaseWidths = bodyContent.map((line) => displayWidth(rail) + displayWidth(line) + displayWidth(rail));
  const frameWidth = Math.max(
    topBaseWidth + 1,
    footerBaseWidth + 1,
    ...bodyBaseWidths.map((width) => width + 1),
  );
  const topPadding = frameWidth - topBaseWidth;
  const bottomPadding = frameWidth - footerBaseWidth;
  const framedBody = bodyContent.map((line) => {
    const padding = " ".repeat(frameWidth - displayWidth(rail) - displayWidth(line) - displayWidth(rail));
    return `${rail}${line}${padding}${rail}`;
  });
  const lines = [
    `${border} ${headerContent} ${horizontal.repeat(topPadding)}${topRight}`,
    ...framedBody,
    `${end} ${footerContent} ${horizontal.repeat(bottomPadding)}${bottomRight}`,
  ];
  return lines
    .map((line) => paint(line, "approval", capabilities, theme))
    .join("\n");
}

/**
 * Return terminal cells rather than JavaScript code points. This intentionally
 * covers the common CJK, emoji and combining-mark cases without depending on
 * a native module; ambiguous characters stay one cell for stable ASCII-like
 * framing across locales.
 */
export function displayWidth(text: string): number {
  let width = 0;
  const normalized = text.normalize("NFC");
  // Segmenting grapheme clusters keeps ZWJ emoji and regional-indicator flags
  // at their rendered width instead of counting every code point separately.
  const segmenter = typeof Intl.Segmenter === "function"
    ? new Intl.Segmenter(undefined, { granularity: "grapheme" })
    : undefined;
  const clusters = segmenter === undefined
    ? [...normalized]
    : [...segmenter.segment(normalized)].map(({ segment }) => segment);
  for (const cluster of clusters) {
    const codePoints = [...cluster].map((character) => character.codePointAt(0) ?? 0);
    if (codePoints.length === 0 || codePoints.every(isZeroWidth)) continue;
    width += codePoints.some(isWide) || codePoints.includes(0xfe0f) || codePoints.includes(0x20e3)
      ? 2
      : 1;
  }
  return width;
}

function isZeroWidth(codePoint: number): boolean {
  return (
    (codePoint >= 0x0300 && codePoint <= 0x036f) ||
    (codePoint >= 0x1ab0 && codePoint <= 0x1aff) ||
    (codePoint >= 0x1dc0 && codePoint <= 0x1dff) ||
    (codePoint >= 0x20d0 && codePoint <= 0x20ff) ||
    (codePoint >= 0xfe00 && codePoint <= 0xfe0f) ||
    (codePoint >= 0x200b && codePoint <= 0x200f) ||
    (codePoint >= 0x2060 && codePoint <= 0x2064) ||
    (codePoint >= 0xfeff && codePoint <= 0xfeff)
  );
}

function isWide(codePoint: number): boolean {
  return (
    (codePoint >= 0x1100 && codePoint <= 0x115f) ||
    (codePoint >= 0x2329 && codePoint <= 0x232a) ||
    (codePoint >= 0x2e80 && codePoint <= 0xa4cf) ||
    (codePoint >= 0xac00 && codePoint <= 0xd7a3) ||
    (codePoint >= 0xf900 && codePoint <= 0xfaff) ||
    (codePoint >= 0xfe10 && codePoint <= 0xfe19) ||
    (codePoint >= 0xfe30 && codePoint <= 0xfe6f) ||
    (codePoint >= 0xff00 && codePoint <= 0xff60) ||
    (codePoint >= 0xffe0 && codePoint <= 0xffe6) ||
    (codePoint >= 0x1f1e6 && codePoint <= 0x1f1ff) ||
    (codePoint >= 0x1f300 && codePoint <= 0x1faff) ||
    (codePoint >= 0x20000 && codePoint <= 0x3fffd)
  );
}

export const renderApprovalGate = formatApprovalGate;

export type ApprovalChoice = "allow_once" | "deny" | "inspect";

const KNOWN_EFFECT_KEYS = new Set([
  "filesystem_read",
  "filesystem_write",
  "filesystem_delete",
  "network",
  "secrets",
  "external_write",
  "external_writes",
  "process",
  "processes",
  "idempotency",
  "reversibility",
]);

const SIDE_EFFECT_KEYS = [
  "filesystem_write",
  "filesystem_delete",
  "network",
  "secrets",
  "external_write",
  "external_writes",
  "process",
  "processes",
] as const;

/**
 * Auto-approve only when every known write channel is empty and no unknown
 * effect key is present. Unknown keys fail closed so a new side-effect field
 * cannot skip the gate.
 */
export function isReadOnlyEffects(effects: unknown): boolean {
  if (!isRecord(effects)) return false;
  const record = effects as Record<string, unknown>;
  for (const key of Object.keys(record)) {
    if (!KNOWN_EFFECT_KEYS.has(key)) return false;
  }
  return SIDE_EFFECT_KEYS.every((key) => !hasEffect(record[key]));
}

function hasEffect(value: unknown): boolean {
  if (Array.isArray(value)) return value.length > 0;
  if (typeof value === "boolean") return value;
  return value !== undefined && value !== null && value !== "";
}

export function parseApprovalChoice(input: string): ApprovalChoice {
  switch (input.trim().toLowerCase()) {
    case "1":
    case "a":
    case "allow":
    case "once":
    case "allow_once":
    case "y":
    case "yes":
      return "allow_once";
    case "2":
    case "d":
    case "deny":
      return "deny";
    case "i":
    case "inspect":
      return "inspect";
    default:
      return "deny";
  }
}

function framedLines(text: string, rail: string): string {
  return text
    .split("\n")
    .map((line) => `${rail}   ${line}`)
    .join("\n");
}

function readUnifiedDiffField(value: unknown): string | undefined {
  if (!isRecord(value)) return undefined;
  for (const key of ["unifiedDiff", "unified_diff"] as const) {
    const candidate = value[key];
    if (typeof candidate === "string" && candidate.trim() !== "") return candidate;
  }
  return undefined;
}

function unifiedDiffFromApproval(params: ApprovalRequestParams): string | undefined {
  const direct = readUnifiedDiffField(params);
  if (direct !== undefined) return direct;
  return readUnifiedDiffField(params.invocation);
}

function normalizeApprovalRequest(params: ApprovalRequestParams): ApprovalRequestParams {
  if (
    !isRecord(params) ||
    !isRecord(params.invocation) ||
    typeof params.invocation.invocation_id !== "string" ||
    typeof params.invocation.tool_id !== "string" ||
    typeof params.invocation.tool_version !== "string" ||
    !isRecord(params.invocation.effects) ||
    typeof params.invocation.effect_digest !== "string" ||
    params.invocation.normalized_arguments === undefined ||
    typeof params.explanation !== "string"
  ) {
    throw { code: -32602, message: "Invalid approval/request payload" };
  }
  return params;
}
