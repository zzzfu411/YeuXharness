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
        this.#output.write(
          `${paint("INSPECT · NORMALIZED ARGUMENTS", "text", this.#capabilities, this.#theme)}\n` +
          `${framedLines(safeArguments, rail)}\n`,
        );
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
  const rail = glyph("approvalRail", capabilities);
  const end = glyph("approvalEnd", capabilities);
  const header = `${border} ${glyph("approval", capabilities)} APPROVAL REQUIRED · ${tool}`;
  const footer = `${end} [a] ALLOW ONCE   [d] DENY (default)   [i] INSPECT`;
  const lines = [
    header,
    framedLines(explanation, rail),
    `${rail} binding ${digest} · invocation ${invocationId}`,
    `${rail} effects`,
    framedLines(safeEffects, rail),
    footer,
  ];
  return lines
    .flatMap((line) => line.split("\n"))
    .map((line) => paint(sanitizeTerminalLine(line), "approval", capabilities, theme))
    .join("\n");
}

export const renderApprovalGate = formatApprovalGate;

export type ApprovalChoice = "allow_once" | "deny" | "inspect";

/** Read-only invocations never need to stop the model stream for a human vote. */
export function isReadOnlyEffects(effects: unknown): boolean {
  if (!isRecord(effects) || !("filesystem_read" in effects)) return false;
  const record = effects as Record<string, unknown>;
  const risky = [
    "filesystem_write",
    "filesystem_delete",
    "network",
    "secrets",
    "external_write",
    "external_writes",
    "process",
    "processes",
  ];
  if (risky.some((key) => hasEffect(record[key]))) return false;
  return record.process !== true && !hasEffect(record.processes);
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
