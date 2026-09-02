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
  #queue: Promise<unknown> = Promise.resolve();

  public constructor(
    input: Readable = process.stdin,
    output: Writable = process.stdout,
    options: TerminalPrompterOptions = {},
  ) {
    const terminal = (output as Writable & { readonly isTTY?: boolean }).isTTY === true;
    this.#readline = createInterface({ input, output, terminal });
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
    return this.#enqueueQuestion(sanitizeTerminalText(prompt));
  }

  /** Renderer-owned interactive prompt; untrusted text never enters this path. */
  public command(): Promise<string> {
    const prompt = paint(
      `yeux ${glyph("prompt", this.#capabilities)}`,
      "focus",
      this.#capabilities,
      this.#theme,
    );
    return this.#enqueueQuestion(`\n${prompt} `);
  }

  #enqueueQuestion(prompt: string): Promise<string> {
    const operation = this.#queue.then(async () => await this.#readline.question(prompt));
    this.#queue = operation.catch(() => undefined);
    return operation;
  }

  public async approval(params: ApprovalRequestParams): Promise<ApprovalRequestResult> {
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
      const choice = parseApprovalChoice(
        await this.#enqueueQuestion(
          paint(
            `${glyph("prompt", this.#capabilities)} approval: `,
            "approval",
            this.#capabilities,
            this.#theme,
          ),
        ),
      );
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
    const prompt = sanitizeTerminalLine(params.prompt);
    return { content: [{ type: "text", text: await this.#enqueueQuestion(`${prompt}: `) }] };
  }

  public close(): void {
    this.#readline.close();
  }
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

function displayWidth(text: string): number {
  return [...text].length;
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
