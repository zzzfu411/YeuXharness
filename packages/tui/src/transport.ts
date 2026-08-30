import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync, type Stats } from "node:fs";
import { lstat } from "node:fs/promises";
import { createConnection, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

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
  return join(tmpdir(), `yeux-${currentUid() ?? "user"}`, "yeuxd.sock");
}

/**
 * Verifies the filesystem boundary used to authenticate a local Unix socket.
 * The daemon creates both components with owner-only permissions; accepting a
 * looser path would let another local account replace or impersonate it.
 */
export async function validateSocketPath(socketPath: string): Promise<void> {
  await inspectSocketPath(socketPath, currentUid());
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
  const uid = currentUid();
  const before = await inspectSocketPath(path, uid);
  const socket = await new Promise<Socket>((resolve, reject) => {
    const candidate = createConnection(path);
    const cleanup = (): void => {
      clearTimeout(timer);
      candidate.off("error", onError);
    };
    const onError = (error: Error): void => {
      cleanup();
      candidate.destroy();
      reject(error);
    };
    const timer = setTimeout(() => {
      cleanup();
      candidate.destroy();
      reject(new Error(`Socket connection timed out after ${timeoutMs} ms`));
    }, timeoutMs);
    timer.unref();
    candidate.once("connect", () => {
      cleanup();
      resolve(candidate);
    });
    candidate.once("error", onError);
  });
  let postConnectError: Error | undefined;
  const capturePostConnectError = (error: Error): void => {
    postConnectError = error;
  };
  socket.once("error", capturePostConnectError);
  try {
    const after = await inspectSocketPath(path, uid);
    if (!sameSocketPath(before, after)) {
      throw new Error(`Socket path changed while connecting: ${path}`);
    }
    if (postConnectError !== undefined) throw postConnectError;
  } catch (error) {
    socket.off("error", capturePostConnectError);
    socket.destroy();
    throw error;
  }
  const client = new JsonRpcClient(socket, socket);
  socket.off("error", capturePostConnectError);
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

interface SocketPathInspection {
  readonly parent: PathIdentity;
  readonly socket: PathIdentity;
}

interface PathIdentity {
  readonly device: number;
  readonly inode: number;
}

async function inspectSocketPath(
  socketPath: string,
  expectedUid: number | undefined,
): Promise<SocketPathInspection> {
  const parentPath = dirname(socketPath);
  const parent = await lstat(parentPath);
  assertPathSecurity("Socket parent", parentPath, parent, "directory", expectedUid);

  const socket = await lstat(socketPath);
  assertPathSecurity("Socket", socketPath, socket, "socket", expectedUid);

  return {
    parent: { device: parent.dev, inode: parent.ino },
    socket: { device: socket.dev, inode: socket.ino },
  };
}

function assertPathSecurity(
  label: string,
  path: string,
  metadata: Stats,
  expectedType: "directory" | "socket",
  expectedUid: number | undefined,
): void {
  if (metadata.isSymbolicLink()) throw new Error(`${label} must not be a symlink: ${path}`);
  if (expectedType === "directory" && !metadata.isDirectory()) {
    throw new Error(`${label} is not a directory: ${path}`);
  }
  if (expectedType === "socket" && !metadata.isSocket()) {
    throw new Error(`${label} is not a Unix socket: ${path}`);
  }
  if (expectedUid !== undefined && metadata.uid !== expectedUid) {
    throw new Error(`${label} must be owned by uid ${expectedUid}: ${path}`);
  }

  const mode = metadata.mode & 0o777;
  if ((mode & 0o077) !== 0) {
    throw new Error(
      `${label} must not be accessible by group or other users (mode ${mode.toString(8)}): ${path}`,
    );
  }
}

function sameSocketPath(left: SocketPathInspection, right: SocketPathInspection): boolean {
  return (
    left.parent.device === right.parent.device &&
    left.parent.inode === right.parent.inode &&
    left.socket.device === right.socket.device &&
    left.socket.inode === right.socket.inode
  );
}

function currentUid(): number | undefined {
  if (typeof process.geteuid === "function") return process.geteuid();
  if (typeof process.getuid === "function") return process.getuid();
  return undefined;
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
