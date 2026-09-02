#!/usr/bin/env node
import { USAGE, parseArgs } from "./args.js";
import { replayFixture, runTui } from "./app.js";
import { sanitizeTerminalText } from "./terminal.js";

const VERSION = "0.1.0";

async function main(): Promise<number> {
  let options;
  try {
    options = parseArgs(process.argv.slice(2));
  } catch (error) {
    process.stderr.write(sanitizeTerminalText(`${formatError(error)}\n\n${USAGE}\n`));
    return 2;
  }

  if (options.command === "help") {
    process.stdout.write(`${USAGE}\n`);
    return 0;
  }
  if (options.command === "version") {
    process.stdout.write(`${VERSION}\n`);
    return 0;
  }
  try {
    if (options.command === "replay") {
      return await replayFixture(options.replayPath as string, {
        ascii: options.ascii,
        jsonl: options.jsonl,
      });
    }
    const result = await runTui(options);
    return result.exitCode;
  } catch (error) {
    process.stderr.write(sanitizeTerminalText(`yeux: ${formatError(error)}\n`));
    return 1;
  }
}

function formatError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

process.exitCode = await main();
