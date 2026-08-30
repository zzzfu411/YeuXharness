import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import { extname } from "node:path";

import {
  JsonRpcClient,
  JsonRpcConnectionClosedError,
  PROTOCOL_VERSION,
  isRecord,
  type JsonObject,
  type JsonValue,
} from "@yeux/protocol";

import {
  resolveGrantedCapabilities,
  verifyPluginExecutable,
  type PluginManifest,
} from "./manifest.js";

const TOOL_ID = /^[a-z][a-z0-9_.-]{0,127}$/;

export interface PluginToolDescriptor {
  readonly id: string;
  readonly description: string;
  readonly input_schema: JsonObject;
  readonly output_schema?: JsonObject;
  readonly effect_template?: JsonObject;
}

export interface PluginDescriptor {
  readonly id: string;
  readonly version: string;
  readonly protocol: string;
  readonly granted_capabilities: readonly string[];
  readonly tools: readonly PluginToolDescriptor[];
}

export interface PluginInvocation {
  readonly invocation_id?: string;
  readonly tool_id: string;
  readonly input: JsonValue;
}

export interface PluginHostOptions {
  readonly manifest: PluginManifest;
  readonly root: string;
  readonly grantedCapabilities?: readonly string[];
  readonly onStderr?: (text: string) => void;
  readonly onNotification?: (method: string, params: unknown) => void;
  readonly startupTimeoutMs?: number;
  readonly invocationTimeoutMs?: number;
}

export class PluginHost {
  readonly #options: PluginHostOptions;
  readonly #grantedCapabilities: readonly string[];
  #child: ChildProcessWithoutNullStreams | undefined;
  #client: JsonRpcClient | undefined;
  #descriptor: PluginDescriptor | undefined;
  #toolNames = new Map<string, string>();

  public constructor(options: PluginHostOptions) {
    this.#options = options;
    this.#grantedCapabilities = resolveGrantedCapabilities(
      options.manifest,
      options.grantedCapabilities,
    );
  }

  public get descriptor(): PluginDescriptor {
    if (this.#descriptor === undefined) throw new Error("Plugin host has not started");
    return this.#descriptor;
  }

  public async start(): Promise<PluginDescriptor> {
    if (this.#client !== undefined) return this.descriptor;
    const executable = await verifyPluginExecutable(this.#options.root, this.#options.manifest);
    const isNodeModule = [".js", ".cjs", ".mjs"].includes(extname(executable).toLowerCase());
    const command = isNodeModule ? process.execPath : executable;
    const args = isNodeModule
      ? [executable, ...this.#options.manifest.args]
      : [...this.#options.manifest.args];

    const child = spawn(command, args, {
      cwd: this.#options.root,
      env: {
        LANG: process.env.LANG ?? "C.UTF-8",
        PATH: process.env.PATH ?? "",
        YEUX_PLUGIN_ID: this.#options.manifest.id,
        YEUX_PLUGIN_CAPABILITIES: JSON.stringify(this.#grantedCapabilities),
      },
      stdio: ["pipe", "pipe", "pipe"],
      shell: false,
      windowsHide: true,
    });
    await waitForSpawn(child);
    this.#child = child;
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (text: string) => this.#options.onStderr?.(text));

    const client = new JsonRpcClient(child.stdout, child.stdin, {
      requestTimeoutMs: this.#options.invocationTimeoutMs ?? 30_000,
    });
    this.#client = client;
    client.on("notification", (message: unknown) => {
      if (isRecord(message) && typeof message.method === "string") {
        this.#options.onNotification?.(message.method, message.params);
      }
    });
    child.once("exit", (code, signal) => {
      client.close(
        new JsonRpcConnectionClosedError(
          `Plugin ${this.#options.manifest.id} exited ${signal ?? code ?? "unknown"}`,
        ),
      );
    });

    try {
      const raw = await client.request(
        "initialize",
        {
          protocol_version: PROTOCOL_VERSION,
          plugin: {
            id: this.#options.manifest.id,
            version: this.#options.manifest.version,
          },
          granted_capabilities: this.#grantedCapabilities,
        },
        { timeoutMs: this.#options.startupTimeoutMs ?? 5_000 },
      );
      this.#descriptor = validateDescriptor(
        raw,
        this.#options.manifest,
        this.#grantedCapabilities,
        this.#toolNames,
      );
      return this.#descriptor;
    } catch (error) {
      await this.close();
      throw error;
    }
  }

  public async invoke(invocation: PluginInvocation): Promise<unknown> {
    const client = this.#client;
    if (client === undefined) throw new Error("Plugin host has not started");
    const localToolId = this.#toolNames.get(invocation.tool_id);
    if (localToolId === undefined) {
      throw { code: -32601, message: `Unknown plugin tool ${invocation.tool_id}` };
    }
    return await client.request(
      "tool/invoke",
      {
        invocation_id: invocation.invocation_id ?? randomUUID(),
        tool_id: localToolId,
        input: invocation.input,
      },
      { timeoutMs: this.#options.invocationTimeoutMs ?? 30_000 },
    );
  }

  public async close(): Promise<void> {
    const child = this.#child;
    const client = this.#client;
    this.#child = undefined;
    this.#client = undefined;
    this.#descriptor = undefined;
    this.#toolNames.clear();
    if (child === undefined) return;

    if (client !== undefined && !client.closed) {
      try {
        await client.request("shutdown", {}, { timeoutMs: 750 });
      } catch {
        // A crashed or unresponsive plugin is terminated below.
      }
      client.close();
    }
    child.stdin.end();
    await terminateChild(child);
  }
}

function validateDescriptor(
  raw: unknown,
  manifest: PluginManifest,
  grantedCapabilities: readonly string[],
  toolNames: Map<string, string>,
): PluginDescriptor {
  if (!isRecord(raw) || !isRecord(raw.contributions)) {
    throw new Error("Plugin initialize response is missing contributions");
  }
  const rawTools = raw.contributions.tools;
  if (rawTools !== undefined && !Array.isArray(rawTools)) {
    throw new Error("Plugin tools contribution must be an array");
  }
  const tools = (rawTools ?? []).map((tool): PluginToolDescriptor => {
    if (
      !isRecord(tool) ||
      typeof tool.id !== "string" ||
      !TOOL_ID.test(tool.id) ||
      typeof tool.description !== "string" ||
      !isRecord(tool.input_schema)
    ) {
      throw new Error("Plugin returned an invalid tool descriptor");
    }
    const namespacedId = `${manifest.id}/${tool.id}`;
    if (toolNames.has(namespacedId)) throw new Error(`Duplicate plugin tool ${tool.id}`);
    toolNames.set(namespacedId, tool.id);
    return Object.freeze({
      id: namespacedId,
      description: tool.description,
      input_schema: tool.input_schema as JsonObject,
      ...(isRecord(tool.output_schema) ? { output_schema: tool.output_schema as JsonObject } : {}),
      ...(isRecord(tool.effect_template)
        ? { effect_template: tool.effect_template as JsonObject }
        : {}),
    });
  });

  return Object.freeze({
    id: manifest.id,
    version: manifest.version,
    protocol: manifest.protocol,
    granted_capabilities: grantedCapabilities,
    tools: Object.freeze(tools),
  });
}

async function waitForSpawn(child: ChildProcessWithoutNullStreams): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", reject);
  });
}

async function terminateChild(child: ChildProcessWithoutNullStreams): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.kill("SIGTERM");
  await new Promise<void>((resolve) => {
    const timer = setTimeout(() => {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      resolve();
    }, 1_000);
    timer.unref();
    child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}
