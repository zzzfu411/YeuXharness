import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync } from "node:fs";
import { createConnection, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { JsonRpcClient } from "@yeux/protocol";

export interface RuntimeConnection {
  readonly client: JsonRpcClient;
  readonly kind: "socket" | "stdio";
  readonly description: string;
  close(): Promise<void>;
}

export interface ConnectRuntimeOptions {
  readonly socketPath?: string;
  readonly daemonCommand?: string;
  readonly socketTimeoutMs?: number;
  readonly onDaemonStderr?: (text: string) => void;
}

export function defaultSocketPath(environment = process.env): string {
  if (environment.YEUX_SOCKET !== undefined && environment.YEUX_SOCKET.length > 0) {
    return environment.YEUX_SOCKET;
  }
  if (environment.XDG_RUNTIME_DIR !== undefined && environment.XDG_RUNTIME_DIR.length > 0) {
    return join(environment.XDG_RUNTIME_DIR, "yeux", "yeuxd.sock");
  }
  return join(tmpdir(), `yeux-${process.getuid?.() ?? "user"}.sock`);
}

export async function connectRuntime(
  options: ConnectRuntimeOptions = {},
): Promise<RuntimeConnection> {
  const configuredSocket = options.socketPath ?? defaultSocketPath();
  const shouldTrySocket = options.socketPath !== undefined || existsSync(configuredSocket);
  if (shouldTrySocket) {
    try {
      return await connectSocket(configuredSocket, options.socketTimeoutMs ?? 750);
    } catch (error) {
      options.onDaemonStderr?.(
        `Could not connect to ${configuredSocket}; starting a private runtime (${formatError(error)}).\n`,
      );
    }
  }
  return await spawnRuntime(options.daemonCommand ?? "yeuxd", options.onDaemonStderr);
}

async function connectSocket(path: string, timeoutMs: number): Promise<RuntimeConnection> {
  const socket = await new Promise<Socket>((resolve, reject) => {
    const candidate = createConnection(path);
    const timer = setTimeout(() => {
      candidate.destroy();
      reject(new Error(`Socket connection timed out after ${timeoutMs} ms`));
    }, timeoutMs);
    timer.unref();
    candidate.once("connect", () => {
      clearTimeout(timer);
      candidate.off("error", reject);
      resolve(candidate);
    });
    candidate.once("error", reject);
  });
  const client = new JsonRpcClient(socket, socket);
  let closePromise: Promise<void> | undefined;
  return {
    client,
    kind: "socket",
    description: path,
    close: () => {
      closePromise ??= Promise.resolve().then(() => {
        client.close();
        if (!socket.destroyed) socket.end();
      });
      return closePromise;
    },
  };
}

async function spawnRuntime(
  daemonCommand: string,
  onStderr: ((text: string) => void) | undefined,
): Promise<RuntimeConnection> {
  const child = spawn(daemonCommand, ["--stdio"], {
    stdio: ["pipe", "pipe", "pipe"],
    shell: false,
    windowsHide: true,
  });
  await waitForSpawn(child);
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk: string) => onStderr?.(chunk));
  const client = new JsonRpcClient(child.stdout, child.stdin);
  const terminateOnParentExit = (): void => {
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
  };
  process.once("exit", terminateOnParentExit);
  child.once("exit", (code, signal) => {
    process.off("exit", terminateOnParentExit);
    client.close(
      new Error(
        `yeuxd exited ${signal === null ? `with code ${code ?? "unknown"}` : `from ${signal}`}`,
      ),
    );
  });
  let closePromise: Promise<void> | undefined;

  return {
    client,
    kind: "stdio",
    description: `${daemonCommand} --stdio`,
    close: () => {
      closePromise ??= (async () => {
        process.off("exit", terminateOnParentExit);
        client.close();
        child.stdin.end();
        await terminateChild(child);
      })();
      return closePromise;
    },
  };
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
    }, 2_000);
    timer.unref();
    child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

function formatError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
