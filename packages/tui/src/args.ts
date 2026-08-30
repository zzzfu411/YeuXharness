import { resolve } from "node:path";

import type { RuntimeMode } from "@yeux/protocol";

export type TuiCommand = "interactive" | "run" | "help" | "version";

export interface TuiOptions {
  readonly command: TuiCommand;
  readonly prompt?: string;
  readonly jsonl: boolean;
  /** Force the copy-safe ASCII presentation for terminals with odd width rules. */
  readonly ascii: boolean;
  readonly cwd: string;
  readonly socketPath?: string;
  readonly daemonCommand: string;
  readonly threadId?: string;
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
  let mode: RuntimeMode = "build";
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

    if (index === 0 && (arg === "run" || arg === "help" || arg === "version")) {
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
      command = "run";
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

  if (prompt === undefined && positionals.length > 0) prompt = positionals.join(" ");
  if (command === "run" && prompt === undefined) {
    throw new Error("run requires a prompt (use -p or positional text)");
  }

  return {
    command,
    ...(prompt === undefined ? {} : { prompt }),
    jsonl,
    ascii,
    cwd: resolve(processCwd, cwd),
    ...(socketPath === undefined ? {} : { socketPath }),
    daemonCommand,
    ...(threadId === undefined ? {} : { threadId }),
    mode,
  };
}

export const USAGE = `YeuX Harness terminal client

Usage:
  yeux                         Start an interactive session
  yeux run -p <prompt>         Run one turn
  yeux run -p <prompt> --jsonl Stream event envelopes as JSONL

Options:
  -p, --prompt <text>   Prompt for run mode
      --cwd <path>      Workspace root (default: current directory)
      --thread <id>     Resume an existing thread
      --mode <mode>     observe, build, or operate (default: build)
      --socket <path>   Prefer this Unix socket, then fall back to stdio
      --daemon <path>   yeuxd executable (default: yeuxd)
      --ascii           Use copy-safe ASCII rails and status glyphs
      --jsonl           Machine-readable event stream
  -h, --help            Show this help
  -V, --version         Show the client version`;
