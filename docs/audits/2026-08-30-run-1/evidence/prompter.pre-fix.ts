import { createInterface, type Interface } from "node:readline/promises";
import type { Readable, Writable } from "node:stream";

import {
  isRecord,
  type ApprovalRequestParams,
  type ApprovalRequestResult,
  type UserInputRequestParams,
  type UserInputRequestResult,
} from "@yeux/protocol";

export class TerminalPrompter {
  readonly #readline: Interface;
  readonly #output: Writable;
  #queue: Promise<unknown> = Promise.resolve();

  public constructor(input: Readable = process.stdin, output: Writable = process.stdout) {
    const terminal = (output as Writable & { readonly isTTY?: boolean }).isTTY === true;
    this.#readline = createInterface({ input, output, terminal });
    this.#output = output;
  }

  public question(prompt: string): Promise<string> {
    const operation = this.#queue.then(async () => await this.#readline.question(prompt));
    this.#queue = operation.catch(() => undefined);
    return operation;
  }

  public async approval(params: ApprovalRequestParams): Promise<ApprovalRequestResult> {
    const safe = normalizeApprovalRequest(params);
    this.#output.write(
      `\nApproval required: ${safe.invocation.tool_id}@${safe.invocation.tool_version}\n${safe.explanation}\n`,
    );
    this.#output.write(`${JSON.stringify(safe.invocation.effects, null, 2)}\n`);
    const answer = await this.question(
      "[1] allow once  [2] deny (default): ",
    );
    return { approved: parseApprovalChoice(answer) === "allow_once" };
  }

  public async userInput(params: UserInputRequestParams): Promise<UserInputRequestResult> {
    if (!isRecord(params) || typeof params.prompt !== "string") {
      throw { code: -32602, message: "Invalid user/input request" };
    }
    return { content: [{ type: "text", text: await this.question(`${params.prompt}: `) }] };
  }

  public close(): void {
    this.#readline.close();
  }
}

export type ApprovalChoice = "allow_once" | "deny";

export function parseApprovalChoice(input: string): ApprovalChoice {
  switch (input.trim().toLowerCase()) {
    case "1":
    case "once":
    case "allow_once":
    case "y":
    case "yes":
      return "allow_once";
    default:
      return "deny";
  }
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
    typeof params.explanation !== "string"
  ) {
    throw { code: -32602, message: "Invalid approval/request payload" };
  }
  return params;
}
