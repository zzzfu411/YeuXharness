/**
 * The interactive command grammar is deliberately small and deterministic.
 * Slash commands are handled by the client control plane and never sent to a
 * model as accidental user prose. Everything else remains an ordinary turn
 * prompt.
 */

import type { RuntimeMode, WorkspaceTrust } from "@yeux/protocol";

export type PlanAction =
  | { readonly action: "show" }
  | { readonly action: "clear" }
  | { readonly action: "add"; readonly text: string };

export type InteractiveCommand =
  | { readonly kind: "prompt"; readonly text: string }
  | { readonly kind: "help" }
  | { readonly kind: "model"; readonly provider?: string }
  | { readonly kind: "doctor" }
  | { readonly kind: "context" }
  | { readonly kind: "plan"; readonly plan: PlanAction }
  | { readonly kind: "resume"; readonly threadId?: string }
  | { readonly kind: "compact" }
  | { readonly kind: "interrupt"; readonly reason?: string }
  | { readonly kind: "steer"; readonly message: string }
  | { readonly kind: "reconcile"; readonly invocationId: string; readonly outcome: "completed" | "failed"; readonly summary: string; readonly artifactUri?: string }
  | { readonly kind: "mode"; readonly mode: "observe" | "build" | "operate" }
  | { readonly kind: "threads" }
  | { readonly kind: "fork"; readonly title?: string }
  | { readonly kind: "exit" }
  | { readonly kind: "unknown"; readonly name: string };

export class InteractiveCommandError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = "InteractiveCommandError";
  }
}

const MAX_COMMAND_BYTES = 16 * 1024;

/** Mirror the shrinking authority intersection for client display. */
export function resolveEffectiveMode(input: {
  readonly requested: RuntimeMode;
  readonly hostCeiling: RuntimeMode;
  readonly workspaceTrust: WorkspaceTrust | undefined;
  readonly writeReady: boolean;
}): RuntimeMode {
  const projectCeiling: RuntimeMode = input.workspaceTrust === "trusted" ? "build" : "observe";
  const ceiling = minimumMode(
    minimumMode(input.requested, input.hostCeiling),
    projectCeiling,
  );
  return ceiling === "observe" || !input.writeReady ? "observe" : ceiling;
}

function minimumMode(left: RuntimeMode, right: RuntimeMode): RuntimeMode {
  const rank: Record<RuntimeMode, number> = { observe: 0, build: 1, operate: 2 };
  return rank[left] <= rank[right] ? left : right;
}

/** Parse one line from the interactive prompt. */
export function parseInteractiveCommand(input: string): InteractiveCommand {
  if (Buffer.byteLength(input, "utf8") > MAX_COMMAND_BYTES) {
    throw new InteractiveCommandError("interactive command exceeds 16 KiB");
  }
  const trimmed = input.trim();
  if (!trimmed.startsWith("/")) return { kind: "prompt", text: trimmed };

  const tokens = tokenize(trimmed.slice(1));
  const name = tokens.shift()?.toLowerCase() ?? "";
  switch (name) {
    case "":
      return { kind: "prompt", text: trimmed };
    case "exit":
    case "quit":
    case "q":
      ensureNoArguments(name, tokens);
      return { kind: "exit" };
    case "help":
    case "h":
      ensureNoArguments(name, tokens);
      return { kind: "help" };
    case "model":
    case "models":
      if (tokens.length > 1) throw new InteractiveCommandError("/model accepts at most one provider");
      return { kind: "model", ...(tokens[0] === undefined ? {} : { provider: tokens[0] }) };
    case "doctor":
      ensureNoArguments(name, tokens);
      return { kind: "doctor" };
    case "context":
      ensureNoArguments(name, tokens);
      return { kind: "context" };
    case "plan":
      return { kind: "plan", plan: parsePlan(tokens) };
    case "resume":
      if (tokens.length > 1) throw new InteractiveCommandError("/resume accepts an optional thread id");
      return { kind: "resume", ...(tokens[0] === undefined ? {} : { threadId: tokens[0] }) };
    case "compact":
      ensureNoArguments(name, tokens);
      return { kind: "compact" };
    case "interrupt":
      return {
        kind: "interrupt",
        ...(tokens.length === 0 ? {} : { reason: tokens.join(" ") }),
      };
    case "steer": {
      const message = tokens.join(" ").trim();
      if (message.length === 0) throw new InteractiveCommandError("/steer requires a message");
      return { kind: "steer", message };
    }
    case "reconcile":
      return parseReconcile(tokens);
    case "mode": {
      const mode = tokens.shift();
      if (mode !== "observe" && mode !== "build" && mode !== "operate") {
        throw new InteractiveCommandError("/mode requires observe, build, or operate");
      }
      ensureNoArguments(name, tokens);
      return { kind: "mode", mode };
    }
    case "threads":
    case "thread":
      ensureNoArguments(name, tokens);
      return { kind: "threads" };
    case "fork":
      return { kind: "fork", ...(tokens.length === 0 ? {} : { title: tokens.join(" ") }) };
    default:
      return { kind: "unknown", name };
  }
}

function parsePlan(tokens: string[]): PlanAction {
  const action = tokens.shift()?.toLowerCase();
  if (action === undefined || action === "show" || action === "list") {
    ensureNoArguments("plan", tokens);
    return { action: "show" };
  }
  if (action === "clear" || action === "reset") {
    ensureNoArguments("plan", tokens);
    return { action: "clear" };
  }
  if (action === "add") {
    const text = tokens.join(" ").trim();
    if (text.length === 0) throw new InteractiveCommandError("/plan add requires a step");
    return { action: "add", text };
  }
  // `/plan inspect the parser` is a convenient shorthand for adding a step.
  const text = [action, ...tokens].join(" ").trim();
  if (text.length === 0) return { action: "show" };
  return { action: "add", text };
}

function parseReconcile(tokens: string[]): Extract<InteractiveCommand, { kind: "reconcile" }> {
  if (tokens.length >= 3 && !tokens[0]?.startsWith("--")) {
    const invocationId = tokens.shift() as string;
    const outcome = tokens.shift();
    if (outcome !== "completed" && outcome !== "failed") {
      throw new InteractiveCommandError("/reconcile outcome must be completed or failed");
    }
    const summary = tokens.join(" ").trim();
    if (summary.length === 0) throw new InteractiveCommandError("/reconcile requires evidence summary");
    return { kind: "reconcile", invocationId, outcome, summary };
  }

  const values = new Map<string, string>();
  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index] as string;
    if (!token.startsWith("--")) throw new InteractiveCommandError(`unexpected /reconcile argument: ${token}`);
    const key = token.slice(2);
    const value = tokens[index + 1];
    if (value === undefined || value.startsWith("--")) {
      throw new InteractiveCommandError(`--${key} requires a value`);
    }
    values.set(key, value);
    index += 1;
  }
  const invocationId = values.get("invocation");
  const outcome = values.get("outcome");
  const summary = values.get("summary")?.trim();
  if (invocationId === undefined || (outcome !== "completed" && outcome !== "failed")) {
    throw new InteractiveCommandError("/reconcile requires --invocation and --outcome completed|failed");
  }
  if (summary === undefined || summary.length === 0) {
    throw new InteractiveCommandError("/reconcile requires a non-empty --summary");
  }
  const artifactUri = values.get("artifact-uri");
  if (artifactUri !== undefined && !artifactUri.startsWith("artifact://")) {
    throw new InteractiveCommandError("--artifact-uri must use the artifact:// scheme");
  }
  return {
    kind: "reconcile",
    invocationId,
    outcome,
    summary,
    ...(artifactUri === undefined ? {} : { artifactUri }),
  };
}

function ensureNoArguments(command: string, tokens: readonly string[]): void {
  if (tokens.length > 0) throw new InteractiveCommandError(`/${command} does not accept arguments`);
}

/** Small shell-like tokenizer; malformed quotes fail closed. */
function tokenize(input: string): string[] {
  const tokens: string[] = [];
  let current = "";
  let quote: '"' | "'" | undefined;
  let escaped = false;
  for (const character of input) {
    if (escaped) {
      current += character;
      escaped = false;
      continue;
    }
    if (character === "\\" && quote !== "'") {
      escaped = true;
      continue;
    }
    if (quote !== undefined) {
      if (character === quote) quote = undefined;
      else current += character;
      continue;
    }
    if (character === '"' || character === "'") {
      quote = character;
    } else if (/\s/u.test(character)) {
      if (current.length > 0) {
        tokens.push(current);
        current = "";
      }
    } else {
      current += character;
    }
  }
  if (escaped || quote !== undefined) throw new InteractiveCommandError("unterminated quote or escape in command");
  if (current.length > 0) tokens.push(current);
  return tokens;
}

export const INTERACTIVE_COMMAND_HELP = [
  "/help                 show this command list",
  "/model [provider]     list configured models",
  "/doctor               show transport, sandbox and capability gates",
  "/context              show bounded recent ledger context",
  "/plan [step|clear]    inspect or edit the local plan scratchpad",
  "/resume [thread]      reload a thread and replay from its ledger",
  "/compact              request a durable checkpoint (if supported)",
  "/interrupt [reason]   cancel the active turn",
  "/steer <message>      steer the active turn at its next safe point",
  "/reconcile ...        record evidence for an Unknown invocation",
  "/mode <mode>          change requested observe/build/operate mode",
  "/threads              list resumable threads",
  "/fork [title]         fork the current thread at its latest sequence",
  "/exit                 close the session",
].join("\n");
