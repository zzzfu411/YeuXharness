#!/usr/bin/env node
import { servePluginHost } from "./service.js";

interface CliOptions {
  readonly manifestPath: string;
  readonly grants: readonly string[];
}

async function main(): Promise<number> {
  try {
    const options = parseArgs(process.argv.slice(2));
    await servePluginHost(options);
    return 0;
  } catch (error) {
    process.stderr.write(`yeux-plugin-host: ${formatError(error)}\n`);
    return 1;
  }
}

function parseArgs(argv: readonly string[]): CliOptions {
  let manifestPath: string | undefined;
  const grants: string[] = [];
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--manifest") {
      manifestPath = argv[index + 1];
      index += 1;
      continue;
    }
    if (arg === "--grant") {
      const grant = argv[index + 1];
      if (grant === undefined) throw new Error("--grant requires a capability");
      grants.push(grant);
      index += 1;
      continue;
    }
    throw new Error(`Unknown option: ${arg ?? ""}`);
  }
  if (manifestPath === undefined) throw new Error("--manifest <path> is required");
  return { manifestPath, grants };
}

function formatError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

process.exitCode = await main();
