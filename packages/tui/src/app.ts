import {
  PROTOCOL_VERSION,
  isRuntimeDiagnosticNotification,
  type ApprovalRequestResult,
  type EventEnvelope,
  type JsonRpcClient,
  type RuntimeMode,
  type UserInputRequestResult,
} from "@yeux/protocol";

import type { TuiOptions } from "./args.js";
import { detectTerminalCapabilities } from "./aesthetic.js";
import { isReadOnlyEffects, TerminalPrompter } from "./prompter.js";
import { EventRenderer } from "./renderer.js";
import { sanitizeTerminalText } from "./terminal.js";
import { connectRuntime, type RuntimeConnection } from "./transport.js";

const CLIENT_VERSION = "0.1.0";
const TERMINAL_STATES = new Set(["completed", "failed", "cancelled"]);

export interface TuiRunResult {
  readonly exitCode: number;
  readonly threadId: string;
  readonly turnId?: string;
}

export async function runTui(options: TuiOptions): Promise<TuiRunResult> {
  const connection = await connectRuntime({
    ...(options.socketPath === undefined ? {} : { socketPath: options.socketPath }),
    daemonCommand: options.daemonCommand,
    onDaemonStderr: (text) => process.stderr.write(sanitizeTerminalText(text)),
  });
  const capabilities = detectTerminalCapabilities({ ascii: options.ascii });
  const renderer = new EventRenderer({ jsonl: options.jsonl, capabilities });
  const prompter = options.jsonl
    ? undefined
    : new TerminalPrompter(process.stdin, process.stdout, { capabilities });

  try {
    const session = new RuntimeSession(connection.client, renderer, prompter);
    await session.initialize();
    const threadId = await session.openThread({
      cwd: options.cwd,
      ...(options.threadId === undefined ? {} : { threadId: options.threadId }),
    });
    const models = await session.listModels();
    const model = models[0] === undefined
      ? "unconfigured"
      : `${models[0].provider}/${models[0].model}`;
    renderer.renderSessionBar({
      cwd: session.workspaceRoot ?? options.cwd,
      thread: threadId,
      mode: options.mode,
      model,
      ...(session.workspaceTrust === undefined ? {} : { trust: session.workspaceTrust }),
      transport: connection.kind,
    });
    // Only claim a write-none grant when this client actually sends the
    // observe override. `--mode build|operate` still reaches the daemon's
    // own grant, so the Inspector must not invent empty write scopes.
    const presenterPolicy = options.mode === "observe"
      ? {
          mode: "observe" as const,
          filesystem_read: [session.workspaceRoot ?? options.cwd],
          filesystem_write: [],
          filesystem_delete: [],
          process: false,
          network: [],
          secrets: [],
          external_write: [],
        }
      : undefined;
    renderer.renderInspector(presenterPolicy);

    // A provider-less daemon is a valid local runtime, but it is not an
    // interactive prompt. Keep the state visible and fail before `yeux ›`.
    if (models.length === 0) {
      renderer.renderDiagnostic({
        code: "provider_unconfigured",
        message: "no model provider is configured; configure a provider before starting a turn",
        recoverable: false,
      });
      return { exitCode: 1, threadId };
    }

    let signalCount = 0;
    let forcedClose = false;
    const forceClose = (): void => {
      forcedClose = true;
      prompter?.close();
      void connection.close();
    };
    const onSigint = (): void => {
      signalCount += 1;
      if (signalCount === 1 && session.hasActiveTurn) {
        void session.interruptActiveTurn("SIGINT").catch((error: unknown) => {
          process.stderr.write(
            sanitizeTerminalText(`Failed to interrupt turn: ${formatError(error)}\n`),
          );
          forceClose();
        });
        return;
      }
      forceClose();
    };
    process.on("SIGINT", onSigint);

    try {
      if (options.command === "run") {
        const result = await session.runTurn(threadId, options.prompt ?? "", options.mode);
        renderer.renderInspector(presenterPolicy);
        return {
          exitCode: exitCodeFor(result),
          threadId,
          ...(result.turn_id === undefined ? {} : { turnId: result.turn_id }),
        };
      }

      let exitCode = 0;
      while (true) {
        const prompt = (await prompter?.command())?.trim() ?? "";
        if (prompt === "/exit" || prompt === "/quit") break;
        if (prompt.length === 0) continue;
        const result = await session.runTurn(threadId, prompt, options.mode);
        renderer.renderInspector(presenterPolicy);
        exitCode = exitCodeFor(result);
      }
      return { exitCode, threadId };
    } catch (error) {
      if (forcedClose) return { exitCode: 130, threadId };
      throw error;
    } finally {
      process.off("SIGINT", onSigint);
    }
  } finally {
    prompter?.close();
    await connection.close();
  }
}

class RuntimeSession {
  readonly #client: JsonRpcClient;
  readonly #renderer: EventRenderer;
  readonly #prompter: TerminalPrompter | undefined;
  readonly #terminalEvents = new Map<string, EventEnvelope>();
  readonly #terminalWaiters = new Map<
    string,
    { readonly resolve: (event: EventEnvelope) => void; readonly reject: (error: Error) => void }
  >();
  #workspaceRoot: string | undefined;
  #workspaceTrust: string | undefined;
  #activeTurn: { readonly threadId: string; readonly turnId: string } | undefined;
  #interruptPromise: Promise<boolean> | undefined;

  public constructor(
    client: JsonRpcClient,
    renderer: EventRenderer,
    prompter: TerminalPrompter | undefined,
  ) {
    this.#client = client;
    this.#renderer = renderer;
    this.#client.onEvent((event) => {
      this.#renderer.render(event);
      if (event.turn_id !== undefined && isTerminalTurnEvent(event)) {
        const waiter = this.#terminalWaiters.get(event.turn_id);
        if (waiter === undefined) this.#terminalEvents.set(event.turn_id, event);
        else {
          this.#terminalWaiters.delete(event.turn_id);
          waiter.resolve(event);
        }
      }
    });
    this.#client.onNotification("runtime/diagnostic", (params) => {
      if (isRuntimeDiagnosticNotification(params)) {
        this.#renderer.renderDiagnostic(params);
      }
    });
    this.#client.once("close", (error: Error) => {
      for (const waiter of this.#terminalWaiters.values()) waiter.reject(error);
      this.#terminalWaiters.clear();
    });
    this.#client.handleRequest("approval/request", async (params): Promise<ApprovalRequestResult> => {
      if (isReadOnlyEffects(params.invocation.effects)) {
        return { approved: true };
      }
      if (this.#prompter === undefined) {
        return { approved: false };
      }
      return await this.#prompter.approval(params);
    });
    this.#client.handleRequest("user/input", async (params): Promise<UserInputRequestResult> => {
      if (this.#prompter === undefined) {
        throw { code: -32010, message: "The JSONL client cannot answer interactive input" };
      }
      return await this.#prompter.userInput(params);
    });
  }

  public async initialize(): Promise<void> {
    const result = await this.#client.command("initialize", {
      protocolVersion: PROTOCOL_VERSION,
      clientInfo: { name: "yeux-tui", version: CLIENT_VERSION },
      capabilities: {
        event_replay: true,
        server_requests: true,
        rich_content: false,
      },
    });
    if (result.protocolVersion.major !== PROTOCOL_VERSION.major) {
      throw new Error(
        `Protocol mismatch: server ${result.protocolVersion.major}.${result.protocolVersion.minor}, client ${PROTOCOL_VERSION.major}.${PROTOCOL_VERSION.minor}`,
      );
    }
  }

  public async listModels(): Promise<readonly { readonly provider: string; readonly model: string }[]> {
    const result = await this.#client.command("model/list", {});
    return result.models;
  }

  public async openThread(options: {
    readonly cwd: string;
    readonly threadId?: string;
  }): Promise<string> {
    if (options.threadId !== undefined) {
      const resumed = await this.#client.command("thread/resume", {
        threadId: options.threadId,
      });
      const workspace = await this.#client.command("workspace/status", {
        workspaceId: resumed.thread.workspace_id,
      });
      this.#workspaceRoot = workspace.workspace.root;
      this.#workspaceTrust = workspace.workspace.trust;
      this.#renderer.rememberEvents(resumed.events);
      await this.#client.command("thread/subscribe", {
        threadId: resumed.thread.id,
        afterSeq: resumed.thread.last_seq,
      });
      return resumed.thread.id;
    }

    const workspace = await this.#client.command("workspace/open", { path: options.cwd });
    this.#workspaceRoot = workspace.workspace.root;
    this.#workspaceTrust = workspace.workspace.trust;
    const created = await this.#client.command("thread/start", {
      workspaceId: workspace.workspace.id,
    });
    await this.#client.command("thread/subscribe", {
      threadId: created.thread.id,
      afterSeq: created.thread.last_seq,
    });
    return created.thread.id;
  }

  public get hasActiveTurn(): boolean {
    return this.#activeTurn !== undefined;
  }

  public get workspaceTrust(): string | undefined {
    return this.#workspaceTrust;
  }

  public get workspaceRoot(): string | undefined {
    return this.#workspaceRoot;
  }

  public async interruptActiveTurn(reason?: string): Promise<boolean> {
    const active = this.#activeTurn;
    if (active === undefined) return false;
    if (this.#interruptPromise !== undefined) return await this.#interruptPromise;

    const operation = this.#client
      .command("turn/interrupt", {
        threadId: active.threadId,
        turnId: active.turnId,
        ...(reason === undefined ? {} : { reason }),
      })
      .then((result) => result.accepted)
      .finally(() => {
        this.#interruptPromise = undefined;
      });
    this.#interruptPromise = operation;
    return await operation;
  }

  public async runTurn(
    threadId: string,
    prompt: string,
    mode: RuntimeMode,
  ): Promise<EventEnvelope> {
    if (mode === "observe" && this.#workspaceRoot === undefined) {
      throw new Error("Cannot enter observe mode without a resolved workspace root");
    }
    const started = await this.#client.command("turn/start", {
      threadId,
      content: [{ type: "text", text: prompt }],
      ...(mode === "observe"
        ? {
            capabilityOverride: {
              mode: "observe" as const,
              filesystem_read: [this.#workspaceRoot as string],
              filesystem_write: [],
              filesystem_delete: [],
              process: false,
              network: [],
              secrets: [],
              external_write: [],
            },
          }
        : {}),
    });
    this.#activeTurn = { threadId, turnId: started.turn.id };
    try {
      return await this.#waitForTerminal(started.turn.id);
    } finally {
      if (this.#activeTurn?.turnId === started.turn.id) this.#activeTurn = undefined;
    }
  }

  async #waitForTerminal(turnId: string): Promise<EventEnvelope> {
    const existing = this.#terminalEvents.get(turnId);
    if (existing !== undefined) {
      this.#terminalEvents.delete(turnId);
      return existing;
    }
    return await new Promise<EventEnvelope>((resolve, reject) => {
      this.#terminalWaiters.set(turnId, { resolve, reject });
    });
  }
}

function exitCodeFor(event: EventEnvelope): number {
  const state = event.payload["to"];
  if (state === "completed") return 0;
  if (state === "cancelled") return 130;
  return 1;
}

function isTerminalTurnEvent(event: EventEnvelope): boolean {
  return event.kind === "turn/state_changed" && TERMINAL_STATES.has(String(event.payload["to"]));
}

function formatError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
