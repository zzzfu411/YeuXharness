import { resolve } from "node:path";

import type { RuntimeMode } from "@yeux/protocol";

export type TuiCommand =
  | "interactive"
  | "run"
  | "replay"
  | "reconcile"
  | "help"
  | "version";

export type ReconciliationOutcome = "completed" | "failed";

export interface TuiOptions {
  readonly command: TuiCommand;
  readonly prompt?: string;
  readonly replayPath?: string;
  readonly jsonl: boolean;
  /** Force the copy-safe ASCII presentation for terminals with odd width rules. */
  readonly ascii: boolean;
  readonly cwd: string;
  readonly socketPath?: string;
  readonly daemonCommand: string;
  readonly threadId?: string;
  /** Invocation identifier used by the control-plane reconciliation command. */
  readonly invocationId?: string;
  readonly reconciliationOutcome?: ReconciliationOutcome;
  readonly reconciliationSummary?: string;
  readonly artifactUri?: string;
  readonly mode: RuntimeMode;
}

export function parseArgs(argv: readonly string[], processCwd = process.cwd()): TuiOptions {
  let command: TuiCommand = "interactive";
  let prompt: string | undefined;
  let jsonl = false;
  let ascii = false;
  let cwd = processCwd;
  let socketPath: string | undefined;
  let daemonCommand = process.env.YEUX_DAEMON ?? "yeuxd";
  let threadId: string | undefined;
  let invocationId: string | undefined;
  let reconciliationOutcome: ReconciliationOutcome | undefined;
  let reconciliationSummary: string | undefined;
  let artifactUri: string | undefined;
  // The bundled tool surface is read-only, so an unqualified turn is observe.
  let mode: RuntimeMode = "observe";
  const positionals: string[] = [];

  const readValue = (index: number, option: string): string => {
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) {
      throw new Error(`${option} requires a value`);
    }
    return value;
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === undefined) continue;

    if (index === 0 && (arg === "run" || arg === "replay" || arg === "reconcile" || arg === "help" || arg === "version")) {
      command = arg;
      continue;
    }
    if (arg === "--help" || arg === "-h") {
      command = "help";
      continue;
    }
    if (arg === "--version" || arg === "-V") {
      command = "version";
      continue;
    }
    if (arg === "--jsonl") {
      jsonl = true;
      if (command === "interactive") command = "run";
      continue;
    }
    if (arg === "--ascii") {
      ascii = true;
      continue;
    }
    if (arg === "--prompt" || arg === "-p") {
      prompt = readValue(index, arg);
      index += 1;
      continue;
    }
    if (arg === "--cwd") {
      cwd = readValue(index, arg);
      index += 1;
      continue;
    }
    if (arg === "--socket") {
      socketPath = readValue(index, arg);
      index += 1;
      continue;
    }
    if (arg === "--daemon") {
      daemonCommand = readValue(index, arg);
      index += 1;
      continue;
    }
    if (arg === "--thread") {
      threadId = readValue(index, arg);
      index += 1;
      continue;
    }
    if (arg === "--invocation") {
      invocationId = readValue(index, arg);
      index += 1;
      continue;
    }
    if (arg === "--outcome") {
      const value = readValue(index, arg);
      if (value !== "completed" && value !== "failed") {
        throw new Error("--outcome must be completed or failed");
      }
      reconciliationOutcome = value;
      index += 1;
      continue;
    }
    if (arg === "--summary") {
      reconciliationSummary = readValue(index, arg);
      index += 1;
      continue;
    }
    if (arg === "--artifact-uri") {
      artifactUri = readValue(index, arg);
      if (!artifactUri.startsWith("artifact://")) {
        throw new Error("--artifact-uri must use the artifact:// scheme");
      }
      index += 1;
      continue;
    }
    if (arg === "--mode") {
      const value = readValue(index, arg);
      if (value !== "observe" && value !== "build" && value !== "operate") {
        throw new Error("--mode must be observe, build, or operate");
      }
      mode = value;
      index += 1;
      continue;
    }
    if (arg === "--") {
      positionals.push(...argv.slice(index + 1));
      break;
    }
    if (arg.startsWith("-")) throw new Error(`Unknown option: ${arg}`);
    positionals.push(arg);
  }

  let replayPath: string | undefined;
  if (command === "replay") {
    if (positionals.length !== 1 || prompt !== undefined) {
      throw new Error("replay requires exactly one fixture path");
    }
    replayPath = resolve(processCwd, positionals[0] as string);
  } else if (command === "reconcile") {
    if (prompt !== undefined || positionals.length > 0) {
      throw new Error("reconcile does not accept a prompt or positional text");
    }
    if (threadId === undefined) throw new Error("reconcile requires --thread");
    if (invocationId === undefined) throw new Error("reconcile requires --invocation");
    if (reconciliationOutcome === undefined) {
      throw new Error("reconcile requires --outcome completed|failed");
    }
    if (reconciliationSummary === undefined || reconciliationSummary.trim().length === 0) {
      throw new Error("reconcile requires a non-empty --summary");
    }
  } else if (prompt === undefined && positionals.length > 0) {
    prompt = positionals.join(" ");
  }
  if (command === "run" && prompt === undefined) {
    throw new Error("run requires a prompt (use -p or positional text)");
  }

  return {
    command,
    ...(prompt === undefined ? {} : { prompt }),
    ...(replayPath === undefined ? {} : { replayPath }),
    jsonl,
    ascii,
    cwd: resolve(processCwd, cwd),
    ...(socketPath === undefined ? {} : { socketPath }),
    daemonCommand,
    ...(threadId === undefined ? {} : { threadId }),
    ...(invocationId === undefined ? {} : { invocationId }),
    ...(reconciliationOutcome === undefined ? {} : { reconciliationOutcome }),
    ...(reconciliationSummary === undefined ? {} : { reconciliationSummary }),
    ...(artifactUri === undefined ? {} : { artifactUri }),
    mode,
  };
}

export const USAGE = `YeuX Harness terminal client

Usage:
  yeux                         Start an interactive session
  yeux run -p <prompt>         Run one turn
  yeux run -p <prompt> --jsonl Stream event envelopes as JSONL
  yeux reconcile --thread <id> --invocation <id> --outcome <completed|failed> --summary <text>
                               Resolve an Unknown invocation using operator evidence
  yeux replay <fixture.jsonl> Replay an inert fixture through the presenters

Options:
  -p, --prompt <text>   Prompt for run mode
      --cwd <path>      Workspace root (default: current directory)
      --thread <id>     Resume an existing thread
      --invocation <id> Invocation to reconcile
      --outcome <state> completed or failed (reconcile only)
      --summary <text>  Bounded operator evidence summary (reconcile only)
      --artifact-uri <u> Verified artifact:// evidence URI (reconcile only)
      --mode <mode>     observe, build, or operate (default: observe; read-only tools)
      --socket <path>   Prefer this Unix socket, then fall back to stdio
      --daemon <path>   yeuxd executable (default: yeuxd)
      --ascii           Use copy-safe ASCII rails and status glyphs
      --jsonl           Machine-readable event stream
  -h, --help            Show this help
  -V, --version         Show the client version

Interactive commands:
  /help /model /context /plan /resume /compact /interrupt /steer
  /reconcile /mode /threads /fork /exit
  EOF also closes an interactive session cleanly.`;
