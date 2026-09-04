import { readFileSync } from "node:fs";
import type { Readable } from "node:stream";
import { Writable } from "node:stream";

import {
  PROTOCOL_VERSION,
  isRecord,
  isRuntimeDiagnosticNotification,
  type InvocationReconcileResult,
  type ApprovalRequestResult,
  type EventEnvelope,
  type Thread,
  type ThreadCompactResult,
  type ThreadReadResult,
  type TurnSteerResult,
  type ModelDescriptor,
  type CapabilityGrant,
  type JsonRpcClient,
  type RuntimeMode,
  type UserInputRequestResult,
  type WorkspaceTrust,
} from "@yeux/protocol";

import type { TuiOptions } from "./args.js";
import { detectTerminalCapabilities } from "./aesthetic.js";
import {
  INTERACTIVE_COMMAND_HELP,
  parseInteractiveCommand,
  resolveEffectiveMode,
  type InteractiveCommand,
} from "./commands.js";
import { formatApprovalGate, isReadOnlyEffects, TerminalPrompter } from "./prompter.js";
import { EventRenderer } from "./renderer.js";
import { sanitizeTerminalText } from "./terminal.js";
import { connectRuntime, type RuntimeConnection } from "./transport.js";

const CLIENT_VERSION = "0.1.0-alpha.1";
const TERMINAL_STATES = new Set(["completed", "failed", "cancelled"]);

export interface TuiRunResult {
  readonly exitCode: number;
  readonly threadId: string;
  readonly turnId?: string;
}

export interface ReplayOptions {
  readonly ascii?: boolean;
  readonly jsonl?: boolean;
  readonly write?: (text: string) => void;
  /** Fixture stdin for `Prompter.approval()`; defaults to process.stdin. */
  readonly input?: Readable;
}

function writableFromWrite(write: (text: string) => void): Writable {
  return new Writable({
    decodeStrings: false,
    write(chunk, _encoding, callback) {
      const text = typeof chunk === "string" ? chunk : Buffer.from(chunk).toString("utf8");
      write(text);
      callback();
    },
  });
}

/** Replay inert JSONL events without opening a daemon or touching the workspace. */
export async function replayFixture(path: string, options: ReplayOptions = {}): Promise<number> {
  const jsonl = options.jsonl ?? false;
  const write = options.write ?? ((text: string) => {
    process.stdout.write(text);
  });
  const capabilities = detectTerminalCapabilities(
    options.ascii === undefined ? {} : { ascii: options.ascii },
  );
  const renderer = new EventRenderer({ jsonl, capabilities, write, typewriter: false });
  const output = writableFromWrite(write);
  const prompter = jsonl
    ? undefined
    : new TerminalPrompter(options.input ?? process.stdin, output, {
        capabilities,
        ...(options.ascii === undefined ? {} : { ascii: options.ascii }),
      });
  const lines = readFileSync(path, "utf8").split(/\r?\n/);
  try {
    for (const line of lines) {
      if (line.trim().length === 0) continue;
      const event = JSON.parse(line) as EventEnvelope;
      await renderer.render(event);
      if (prompter !== undefined) {
        const approval = approvalRequestForFixtureEvent(event);
        if (approval !== undefined) await prompter.approval(approval);
      }
    }
    await renderer.flush();
    return 0;
  } finally {
    prompter?.close();
  }
}

function approvalRequestForFixtureEvent(
  event: EventEnvelope,
): Parameters<typeof formatApprovalGate>[0] | undefined {
  if (event.kind !== "tool/proposed" || !isRecord(event.payload)) return undefined;
  const payload = event.payload;
  const effects = payload.effects;
  if (!isRecord(effects) || isReadOnlyEffects(effects)) return undefined;
  const unifiedDiff = typeof payload.unifiedDiff === "string"
    ? payload.unifiedDiff
    : typeof payload.unified_diff === "string"
      ? payload.unified_diff
      : undefined;
  return {
    invocation: {
      invocation_id: typeof payload.invocation_id === "string" ? payload.invocation_id : event.event_id,
      tool_id: typeof payload.tool_id === "string" ? payload.tool_id : "fixture.approval_boundary",
      tool_version: typeof payload.tool_version === "string" ? payload.tool_version : "fixture",
      effects,
      effect_digest: typeof payload.effect_digest === "string" ? payload.effect_digest : "fixture-effects",
      normalized_arguments: payload.normalized_arguments ?? {},
    },
    explanation: typeof payload.summary === "string" ? payload.summary : "side-effecting tool requires approval",
    ...(unifiedDiff !== undefined && unifiedDiff.trim() !== "" ? { unifiedDiff } : {}),
  };
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
    let threadId = await session.openThread({
      cwd: options.cwd,
      ...(options.threadId === undefined ? {} : { threadId: options.threadId }),
    });

    // Reconciliation is a control-plane operation: it must remain usable
    // when no model provider or process sandbox is configured.  Keep it
    // before model discovery and write-mode checks, while still subscribing
    // to the thread so the durable events are streamed to the caller.
    if (options.command === "reconcile") {
      if (
        options.invocationId === undefined ||
        options.reconciliationOutcome === undefined ||
        options.reconciliationSummary === undefined
      ) {
        throw new Error("reconcile requires --invocation, --outcome, and --summary");
      }
      renderer.renderSessionBar({
        cwd: session.workspaceRoot ?? options.cwd,
        thread: threadId,
        mode: "reconcile",
        model: "control-plane",
        ...(session.workspaceTrust === undefined ? {} : { trust: session.workspaceTrust }),
        transport: connection.kind,
        sandbox: false,
      });
      const result = await session.reconcileInvocation(threadId, {
        invocationId: options.invocationId,
        outcome: options.reconciliationOutcome,
        summary: options.reconciliationSummary,
        ...(options.artifactUri === undefined ? {} : { artifactUri: options.artifactUri }),
      });
      renderer.renderReconciliationResult(result);
      await renderer.flush();
      return { exitCode: 0, threadId };
    }

    const models = await session.listModels();
    const model = models[0] === undefined
      ? "unconfigured"
      : `${models[0].provider}/${models[0].model}`;
    const sandboxNamed = session.sandbox.trim().length > 0 && session.sandbox !== "unavailable";
    const writeReady = session.writeToolsAvailable && sandboxNamed;
    let requestedMode: RuntimeMode = options.mode;
    const effectiveModeFor = (requested: RuntimeMode): RuntimeMode => resolveEffectiveMode({
      requested,
      hostCeiling: session.hostCeiling,
      workspaceTrust: session.workspaceTrust,
      writeReady,
    });
    const effectiveMode = effectiveModeFor(requestedMode);
    const writeEnabled = effectiveMode !== "observe" && writeReady;
    renderer.renderSessionBar({
      cwd: session.workspaceRoot ?? options.cwd,
      thread: threadId,
      mode: effectiveMode,
      model,
      ...(session.workspaceTrust === undefined ? {} : { trust: session.workspaceTrust }),
      transport: connection.kind,
      writeGrant: writeEnabled ? [session.workspaceRoot ?? options.cwd] : [],
      sandbox: sandboxNamed,
    });

    const modeNarrowed = options.mode !== effectiveMode;
    const writeUnavailableReason = !session.writeToolsAvailable
      ? session.writeToolsReason ?? "the daemon did not advertise workspace mutation tools"
      : !sandboxNamed
        ? "a named filesystem sandbox is unavailable"
        : session.workspaceTrust !== "trusted"
          ? "the workspace is untrusted"
          : `the effective capability ceiling is ${effectiveMode}`;
    if (options.mode !== "observe" && effectiveMode === "observe" && options.command === "run") {
      renderer.renderDiagnostic({
        code: "write_pipeline_unavailable",
        message: `requested ${options.mode} mode is unavailable: ${writeUnavailableReason}`,
        recoverable: false,
      });
      return { exitCode: 1, threadId };
    }
    if (modeNarrowed && options.command === "interactive") {
      renderer.renderDiagnostic({
        code: "mode_narrowed",
        message: `requested ${options.mode} mode is effective as ${effectiveMode}: ${writeUnavailableReason}`,
        recoverable: true,
      });
    } else if (modeNarrowed) {
      renderer.renderDiagnostic({
        code: "mode_narrowed",
        message: `requested ${options.mode} mode is effective as ${effectiveMode}`,
        recoverable: true,
      });
    }

    // A provider-less daemon is a valid local runtime, but it is not an
    // interactive prompt. Keep the state visible and fail before `yeux ›`.
    if (models.length === 0 && options.command === "run") {
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
        const result = await session.runTurn(threadId, options.prompt ?? "", effectiveMode);
        await renderer.flush();
        return {
          exitCode: exitCodeFor(result),
          threadId,
          ...(result.turn_id === undefined ? {} : { turnId: result.turn_id }),
        };
      }

      if (prompter === undefined) {
        throw new Error("interactive mode requires a terminal input channel");
      }
      const interactivePrompter = prompter;
      let exitCode = 0;
      const plan: string[] = [];
      while (true) {
        await renderer.flush();
        const line = await interactivePrompter.command();
        // EOF is a successful, explicit end of an interactive session.
        if (line === undefined) break;
        let command: InteractiveCommand;
        try {
          command = parseInteractiveCommand(line);
        } catch (error) {
          renderer.renderDiagnostic({
            code: "invalid_interactive_command",
            message: formatError(error),
            recoverable: true,
          });
          continue;
        }
        if (command.kind === "exit") break;
        if (command.kind === "prompt" && command.text.length === 0) continue;

        if (command.kind !== "prompt") {
          try {
            const handled = await executeInteractiveCommand(
              session,
              renderer,
              command,
              threadId,
              plan,
              effectiveModeFor,
              connection.kind,
            );
            if (handled.threadId !== undefined) threadId = handled.threadId;
            if (handled.requestedMode !== undefined) requestedMode = handled.requestedMode;
            if (handled.exitCode !== undefined) exitCode = handled.exitCode;
          } catch (error) {
            renderer.renderDiagnostic({
              code: "interactive_command_failed",
              message: formatError(error),
              recoverable: true,
            });
          }
          continue;
        }

        if (models.length === 0) {
          renderer.renderDiagnostic({
            code: "provider_unconfigured",
            message: "no model provider is configured; use /doctor or configure a provider before starting a turn",
            recoverable: true,
          });
          exitCode = 1;
          continue;
        }
        const active = await runInteractiveTurn({
          session,
          renderer,
          prompter: interactivePrompter,
          threadId,
          prompt: command.text,
          plan,
          requestedMode,
          effectiveModeFor,
          transport: connection.kind,
        });
        requestedMode = active.requestedMode;
        await renderer.flush();
        exitCode = exitCodeFor(active.event);
        if (active.exitRequested) break;
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
  #workspaceId: string | undefined;
  #threadId: string | undefined;
  #workspaceTrust: WorkspaceTrust | undefined;
  #activeTurn: { readonly threadId: string; readonly turnId: string } | undefined;
  #interruptPromise: Promise<boolean> | undefined;
  #writeToolsAvailable = false;
  #processToolsAvailable = false;
  #writeToolsReason: string | undefined;
  #processToolsReason: string | undefined;
  #sandbox = "unavailable";
  #hostCeiling: RuntimeMode = "observe";

  public constructor(
    client: JsonRpcClient,
    renderer: EventRenderer,
    prompter: TerminalPrompter | undefined,
  ) {
    this.#client = client;
    this.#renderer = renderer;
    this.#client.onEvent((event) => {
      void this.#renderer.render(event);
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
      await this.#renderer.flush();
      if (isReadOnlyEffects(params.invocation.effects)) {
        return { approved: true };
      }
      if (this.#prompter === undefined) {
        return { approved: false };
      }
      return await this.#prompter.approval(params);
    });
    this.#client.handleRequest("user/input", async (params): Promise<UserInputRequestResult> => {
      await this.#renderer.flush();
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
    this.#writeToolsAvailable = result.capabilities.write_tools === true;
    this.#processToolsAvailable = result.capabilities.process_tools === true;
    this.#writeToolsReason = result.capabilities.write_tools_reason;
    this.#processToolsReason = result.capabilities.process_tools_reason;
    this.#sandbox = result.capabilities.sandbox ?? "unavailable";
    this.#hostCeiling = result.hostCeiling;
  }

  public get writeToolsAvailable(): boolean {
    return this.#writeToolsAvailable;
  }

  public get processToolsAvailable(): boolean {
    return this.#processToolsAvailable;
  }

  public get sandbox(): string {
    return this.#sandbox;
  }

  public get writeToolsReason(): string | undefined {
    return this.#writeToolsReason;
  }

  public get processToolsReason(): string | undefined {
    return this.#processToolsReason;
  }

  public get hostCeiling(): RuntimeMode {
    return this.#hostCeiling;
  }

  public async listModels(provider?: string): Promise<readonly ModelDescriptor[]> {
    const result = await this.#client.command("model/list", {
      ...(provider === undefined ? {} : { provider }),
    });
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
      this.#workspaceId = resumed.thread.workspace_id;
      this.#workspaceTrust = workspace.workspace.trust;
      this.#threadId = resumed.thread.id;
      this.#renderer.replaceEvents(resumed.events);
      await this.#client.command("thread/subscribe", {
        threadId: resumed.thread.id,
        afterSeq: resumed.thread.last_seq,
      });
      return resumed.thread.id;
    }

    const workspace = await this.#client.command("workspace/open", { path: options.cwd });
    this.#workspaceRoot = workspace.workspace.root;
    this.#workspaceId = workspace.workspace.id;
    this.#workspaceTrust = workspace.workspace.trust;
    const created = await this.#client.command("thread/start", {
      workspaceId: workspace.workspace.id,
    });
    await this.#client.command("thread/subscribe", {
      threadId: created.thread.id,
      afterSeq: created.thread.last_seq,
    });
    this.#threadId = created.thread.id;
    return created.thread.id;
  }

  public get threadId(): string | undefined {
    return this.#threadId;
  }

  public get workspaceId(): string | undefined {
    return this.#workspaceId;
  }

  public get hasActiveTurn(): boolean {
    return this.#activeTurn !== undefined;
  }

  public get workspaceTrust(): WorkspaceTrust | undefined {
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
    const active = await this.startTurn(threadId, prompt, mode);
    return await active.completion;
  }

  public async startTurn(
    threadId: string,
    prompt: string,
    mode: RuntimeMode,
  ): Promise<ActiveTurnHandle> {
    if (mode === "observe" && this.#workspaceRoot === undefined) {
      throw new Error("Cannot enter observe mode without a resolved workspace root");
    }
    const started = await this.#client.command("turn/start", {
      threadId,
      content: [{ type: "text", text: prompt }],
      capabilityOverride: capabilityOverrideForMode(mode),
    });
    this.#activeTurn = { threadId, turnId: started.turn.id };
    const completion = this.#waitForTerminal(started.turn.id).finally(() => {
      if (this.#activeTurn?.turnId === started.turn.id) this.#activeTurn = undefined;
    });
    return { turnId: started.turn.id, completion };
  }

  public async readThread(threadId?: string): Promise<ThreadReadResult> {
    const resolvedThreadId = threadId ?? this.#threadId;
    if (resolvedThreadId === undefined || resolvedThreadId.length === 0) throw new Error("no active thread");
    return await this.#client.command("thread/read", {
      threadId: resolvedThreadId,
      afterSeq: 0,
      limit: 256,
    });
  }

  public async resumeThread(threadId?: string): Promise<string> {
    const resolvedThreadId = threadId ?? this.#threadId;
    if (resolvedThreadId === undefined || resolvedThreadId.length === 0) throw new Error("resume requires a thread id");
    const resumed = await this.#client.command("thread/resume", { threadId: resolvedThreadId, afterSeq: 0 });
    const workspace = await this.#client.command("workspace/status", {
      workspaceId: resumed.thread.workspace_id,
    });
    this.#workspaceId = resumed.thread.workspace_id;
    this.#workspaceRoot = workspace.workspace.root;
    this.#workspaceTrust = workspace.workspace.trust;
    this.#threadId = resumed.thread.id;
    this.#renderer.replaceEvents(resumed.events);
    await this.#client.command("thread/subscribe", {
      threadId: resumed.thread.id,
      afterSeq: resumed.thread.last_seq,
    });
    return resumed.thread.id;
  }

  public async compactThread(): Promise<ThreadCompactResult> {
    const threadId = this.#threadId;
    if (threadId === undefined) throw new Error("no active thread");
    return await this.#client.command("thread/compact", { threadId });
  }

  public async listThreads(): Promise<readonly Thread[]> {
    const result = await this.#client.command("thread/list", {
      ...(this.#workspaceId === undefined ? {} : { workspaceId: this.#workspaceId }),
      includeArchived: false,
      limit: 100,
    });
    return result.threads;
  }

  public async forkThread(title?: string): Promise<string> {
    const threadId = this.#threadId;
    if (threadId === undefined) throw new Error("no active thread");
    const current = await this.#client.command("thread/resume", { threadId, afterSeq: 0 });
    const result = await this.#client.command("thread/fork", {
      threadId,
      atSeq: current.thread.last_seq,
      ...(title === undefined ? {} : { title }),
    });
    return await this.resumeThread(result.thread.id);
  }

  public async steerActiveTurn(message: string): Promise<TurnSteerResult> {
    const active = this.#activeTurn;
    if (active === undefined) throw new Error("no active turn to steer");
    return await this.#client.command("turn/steer", {
      threadId: active.threadId,
      turnId: active.turnId,
      message,
    });
  }

  public async reconcileInvocation(
    threadId: string,
    options: {
      readonly invocationId: string;
      readonly outcome: "completed" | "failed";
      readonly summary: string;
      readonly artifactUri?: string;
    },
  ): Promise<InvocationReconcileResult> {
    return await this.#client.command("invocation/reconcile", {
      threadId,
      invocationId: options.invocationId,
      outcome: options.outcome,
      evidence: {
        source: "operator_review",
        summary: options.summary,
        ...(options.artifactUri === undefined ? {} : { artifactUri: options.artifactUri }),
      },
    });
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

interface InteractiveCommandResult {
  readonly threadId?: string;
  readonly requestedMode?: RuntimeMode;
  readonly exitCode?: number;
}

interface ActiveTurnHandle {
  readonly turnId: string;
  readonly completion: Promise<EventEnvelope>;
}

async function executeInteractiveCommand(
  session: RuntimeSession,
  renderer: EventRenderer,
  command: Exclude<InteractiveCommand, { readonly kind: "prompt" } | { readonly kind: "exit" }>,
  threadId: string,
  plan: string[],
  effectiveModeFor: (mode: RuntimeMode) => RuntimeMode,
  transport: string,
): Promise<InteractiveCommandResult> {
  switch (command.kind) {
    case "help":
      renderer.renderCommandResult("help", {}, INTERACTIVE_COMMAND_HELP);
      return {};
    case "model": {
      const models = await session.listModels(command.provider);
      renderer.renderModels(models);
      return {};
    }
    case "doctor":
      renderer.renderDoctor({
        sandbox: session.sandbox,
        writeTools: session.writeToolsAvailable,
        processTools: session.processToolsAvailable,
        ...(session.writeToolsReason === undefined ? {} : { writeReason: session.writeToolsReason }),
        ...(session.processToolsReason === undefined ? {} : { processReason: session.processToolsReason }),
        hostCeiling: session.hostCeiling,
        transport,
      });
      return {};
    case "context": {
      const context = await session.readThread(threadId);
      renderer.renderContext(context);
      return {};
    }
    case "plan":
      if (command.plan.action === "clear") plan.length = 0;
      else if (command.plan.action === "add") plan.push(command.plan.text);
      renderer.renderPlan(plan);
      return {};
    case "resume": {
      const resumed = await session.resumeThread(command.threadId);
      renderer.renderCommandResult("resume", { threadId: resumed }, `resumed thread ${resumed}`);
      return { threadId: resumed };
    }
    case "compact": {
      const result = await session.compactThread();
      renderer.renderCommandResult("compact", result, "checkpoint created");
      return {};
    }
    case "interrupt": {
      const accepted = await session.interruptActiveTurn(command.reason);
      renderer.renderCommandResult(
        "interrupt",
        { accepted },
        accepted ? "interrupt requested" : "no active turn",
      );
      return accepted ? { exitCode: 130 } : {};
    }
    case "steer": {
      const result = await session.steerActiveTurn(command.message);
      renderer.renderCommandResult("steer", result, result.accepted ? "steer queued" : "steer rejected");
      return {};
    }
    case "reconcile": {
      const result = await session.reconcileInvocation(threadId, {
        invocationId: command.invocationId,
        outcome: command.outcome,
        summary: command.summary,
        ...(command.artifactUri === undefined ? {} : { artifactUri: command.artifactUri }),
      });
      renderer.renderReconciliationResult(result);
      return {};
    }
    case "mode": {
      const effective = effectiveModeFor(command.mode);
      renderer.renderMode(command.mode, effective);
      if (command.mode !== "observe" && effective === "observe") {
        renderer.renderDiagnostic({
          code: "mode_unavailable",
          message: "the requested write mode is unavailable; continuing in observe mode",
          recoverable: true,
        });
      }
      return { requestedMode: command.mode };
    }
    case "threads": {
      const threads = await session.listThreads();
      renderer.renderThreads(threads);
      return {};
    }
    case "fork": {
      const child = await session.forkThread(command.title);
      renderer.renderCommandResult("fork", { threadId: child }, `forked thread ${child}`);
      return { threadId: child };
    }
    case "unknown":
      renderer.renderDiagnostic({
        code: "unknown_interactive_command",
        message: `unknown command /${command.name}; use /help`,
        recoverable: true,
      });
      return {};
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

interface InteractiveTurnOptions {
  readonly session: RuntimeSession;
  readonly renderer: EventRenderer;
  readonly prompter: TerminalPrompter;
  readonly threadId: string;
  readonly prompt: string;
  readonly plan: string[];
  readonly requestedMode: RuntimeMode;
  readonly effectiveModeFor: (mode: RuntimeMode) => RuntimeMode;
  readonly transport: string;
}

interface InteractiveTurnResult {
  readonly event: EventEnvelope;
  readonly requestedMode: RuntimeMode;
  readonly exitRequested: boolean;
}

async function runInteractiveTurn(options: InteractiveTurnOptions): Promise<InteractiveTurnResult> {
  let requestedMode = options.requestedMode;
  let exitRequested = false;
  const active = await options.session.startTurn(
    options.threadId,
    options.prompt,
    options.effectiveModeFor(requestedMode),
  );
  const terminal = active.completion.then(
    (event) => ({ kind: "terminal" as const, event }),
    (error: unknown) => ({ kind: "terminal_error" as const, error }),
  );

  while (true) {
    await options.renderer.flush();
    const controller = new AbortController();
    const input = options.prompter.command(controller.signal).then(
      (line) => ({ kind: "input" as const, line }),
      (error: unknown) => ({ kind: "input_error" as const, error }),
    );
    const outcome = await Promise.race([terminal, input]);
    if (outcome.kind === "terminal" || outcome.kind === "terminal_error") {
      controller.abort();
      await input;
      if (outcome.kind === "terminal_error") throw outcome.error;
      return { event: outcome.event, requestedMode, exitRequested };
    }
    if (outcome.kind === "input_error") throw outcome.error;
    if (outcome.line === undefined) {
      exitRequested = true;
      await options.session.interruptActiveTurn("stdin EOF");
      const finished = await terminal;
      if (finished.kind === "terminal_error") throw finished.error;
      return { event: finished.event, requestedMode, exitRequested };
    }

    let command: InteractiveCommand;
    try {
      command = parseInteractiveCommand(outcome.line);
    } catch (error) {
      options.renderer.renderDiagnostic({
        code: "invalid_interactive_command",
        message: formatError(error),
        recoverable: true,
      });
      continue;
    }
    if (command.kind === "prompt") {
      if (command.text.length > 0) {
        options.renderer.renderDiagnostic({
          code: "active_turn_requires_control_command",
          message: "a turn is active; use /steer <message> or /interrupt",
          recoverable: true,
        });
      }
      continue;
    }
    if (command.kind === "exit") {
      exitRequested = true;
      await options.session.interruptActiveTurn("interactive exit");
      const finished = await terminal;
      if (finished.kind === "terminal_error") throw finished.error;
      return { event: finished.event, requestedMode, exitRequested };
    }
    if (isBlockedDuringActiveTurn(command)) {
      options.renderer.renderDiagnostic({
        code: "command_unavailable_during_active_turn",
        message: `/${command.kind} is available after the active turn reaches a terminal state`,
        recoverable: true,
      });
      continue;
    }
    try {
      const handled = await executeInteractiveCommand(
        options.session,
        options.renderer,
        command,
        options.threadId,
        options.plan,
        options.effectiveModeFor,
        options.transport,
      );
      if (handled.requestedMode !== undefined) requestedMode = handled.requestedMode;
      if (command.kind === "interrupt") {
        const finished = await terminal;
        if (finished.kind === "terminal_error") throw finished.error;
        return { event: finished.event, requestedMode, exitRequested };
      }
    } catch (error) {
      options.renderer.renderDiagnostic({
        code: "interactive_command_failed",
        message: formatError(error),
        recoverable: true,
      });
    }
  }
}

function isBlockedDuringActiveTurn(
  command: Exclude<InteractiveCommand, { readonly kind: "prompt" } | { readonly kind: "exit" }>,
): boolean {
  return command.kind === "resume" || command.kind === "fork" ||
    command.kind === "compact" || command.kind === "reconcile";
}

function capabilityOverrideForMode(mode: RuntimeMode): CapabilityGrant {
  const write = mode === "observe" ? [] : ["*"];
  const operate = mode === "operate" ? ["*"] : [];
  return {
    mode,
    // Workspace effects are normalized to workspace-relative paths; `*`
    // narrows the layer without making a safe relative path fail solely
    // because the client does not know the daemon's canonical root string.
    filesystem_read: ["*"],
    filesystem_write: write,
    filesystem_delete: write,
    process: mode !== "observe",
    network: operate,
    secrets: operate,
    external_write: operate,
  };
}
