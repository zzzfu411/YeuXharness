import { once } from "node:events";
import type { Readable, Writable } from "node:stream";

import { JsonRpcClient, isRecord } from "@yeux/protocol";

import { loadPluginManifest } from "./manifest.js";
import { PluginHost } from "./plugin-host.js";

export interface ServePluginHostOptions {
  readonly manifestPath: string;
  readonly grants?: readonly string[];
  readonly input?: Readable;
  readonly output?: Writable;
  readonly errorOutput?: Writable;
}

export async function servePluginHost(options: ServePluginHostOptions): Promise<void> {
  const loaded = await loadPluginManifest(options.manifestPath);
  const input = options.input ?? process.stdin;
  const output = options.output ?? process.stdout;
  const errorOutput = options.errorOutput ?? process.stderr;
  const runtime = new JsonRpcClient(input, output, { requestTimeoutMs: 30_000 });
  const host = new PluginHost({
    manifest: loaded.manifest,
    root: loaded.root,
    grantedCapabilities: options.grants ?? [],
    onStderr: (text) => errorOutput.write(`[${loaded.manifest.id}] ${text}`),
    onNotification: (method, params) => {
      void runtime.notify("plugin/event", {
        plugin_id: loaded.manifest.id,
        method,
        params: params ?? null,
      });
    },
  });
  const descriptor = await host.start();

  runtime.handleRequest("plugin/describe", async () => descriptor);
  runtime.handleRequest("plugin/invoke", async (params) => {
    if (!isRecord(params) || typeof params.tool_id !== "string" || !("input" in params)) {
      throw { code: -32602, message: "Invalid plugin/invoke params" };
    }
    return await host.invoke({
      ...(typeof params.invocation_id === "string"
        ? { invocation_id: params.invocation_id }
        : {}),
      tool_id: params.tool_id,
      input: params.input as never,
    });
  });
  runtime.handleRequest("plugin/shutdown", async () => {
    await host.close();
    return { stopped: true };
  });

  try {
    if (!runtime.closed) await once(runtime, "close");
  } finally {
    await host.close();
    runtime.close();
  }
}
