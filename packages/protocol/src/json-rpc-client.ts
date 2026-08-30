import { randomUUID } from "node:crypto";
import { EventEmitter, once } from "node:events";
import type { Readable, Writable } from "node:stream";
import { StringDecoder } from "node:string_decoder";

import {
  JsonRpcConnectionClosedError,
  JsonRpcProtocolError,
  JsonRpcRemoteError,
  JsonRpcTimeoutError,
} from "./errors.js";
import {
  JSON_RPC_VERSION,
  isEventEnvelope,
  isRecord,
  type CommandEnvelope,
  type EventEnvelope,
  type JsonRpcErrorObject,
  type JsonRpcId,
  type JsonRpcNotification,
  type RuntimeCommandMap,
  type RuntimeCommandMethod,
  type RuntimeServerRequestMap,
} from "./types.js";
import { uuidV7 } from "./uuid-v7.js";

export const JSON_RPC_ERROR = Object.freeze({
  parseError: -32700,
  invalidRequest: -32600,
  methodNotFound: -32601,
  invalidParams: -32602,
  internalError: -32603,
});

type RequestHandler = (
  params: unknown,
  context: { readonly id: Exclude<JsonRpcId, null>; readonly method: string },
) => unknown | Promise<unknown>;

type NotificationHandler = (params: unknown, method: string) => void;

interface PendingRequest {
  readonly resolve: (value: unknown) => void;
  readonly reject: (reason: unknown) => void;
  readonly cleanup: () => void;
}

export interface JsonRpcClientOptions {
  readonly maxLineBytes?: number;
  readonly requestTimeoutMs?: number;
  readonly idFactory?: () => string | number;
  readonly commandIdFactory?: () => string;
}

export interface RequestOptions {
  readonly commandId?: string;
  readonly signal?: AbortSignal;
  readonly timeoutMs?: number;
}

export interface NotificationSubscription {
  dispose(): void;
}

export class JsonRpcClient extends EventEmitter {
  readonly #readable: Readable;
  readonly #writable: Writable;
  readonly #decoder = new StringDecoder("utf8");
  readonly #pending = new Map<JsonRpcId, PendingRequest>();
  readonly #requestHandlers = new Map<string, RequestHandler>();
  readonly #notificationHandlers = new Map<string, Set<NotificationHandler>>();
  readonly #maxLineBytes: number;
  readonly #requestTimeoutMs: number;
  readonly #idFactory: () => string | number;
  readonly #commandIdFactory: () => string;
  #buffer = "";
  #closed = false;
  #writeChain: Promise<void> = Promise.resolve();

  public constructor(
    readable: Readable,
    writable: Writable,
    options: JsonRpcClientOptions = {},
  ) {
    super();
    this.#readable = readable;
    this.#writable = writable;
    this.#maxLineBytes = options.maxLineBytes ?? 8 * 1024 * 1024;
    this.#requestTimeoutMs = options.requestTimeoutMs ?? 30_000;
    this.#idFactory = options.idFactory ?? randomUUID;
    this.#commandIdFactory = options.commandIdFactory ?? uuidV7;

    this.#readable.on("data", this.#onData);
    this.#readable.once("end", this.#onEnd);
    this.#readable.once("error", this.#onError);
    this.#writable.once("error", this.#onError);
  }

  public get closed(): boolean {
    return this.#closed;
  }

  public async command<M extends RuntimeCommandMethod>(
    method: M,
    params: RuntimeCommandMap[M]["params"],
    options: RequestOptions = {},
  ): Promise<RuntimeCommandMap[M]["result"]> {
    return (await this.request(method, params, options)) as RuntimeCommandMap[M]["result"];
  }

  public async request<R = unknown>(
    method: string,
    params?: unknown,
    options: RequestOptions = {},
  ): Promise<R> {
    if (this.#closed) throw new JsonRpcConnectionClosedError();
    if (options.signal?.aborted === true) throw options.signal.reason;

    const id = this.#idFactory();
    const request: CommandEnvelope = {
      jsonrpc: JSON_RPC_VERSION,
      id,
      command_id: options.commandId ?? this.#commandIdFactory(),
      method,
      ...(params === undefined ? {} : { params }),
    };

    const timeoutMs = options.timeoutMs ?? this.#requestTimeoutMs;

    return await new Promise<R>((resolve, reject) => {
      let timer: NodeJS.Timeout | undefined;
      const onAbort = (): void => {
        this.#pending.delete(id);
        cleanup();
        reject(options.signal?.reason ?? new DOMException("Aborted", "AbortError"));
      };
      const cleanup = (): void => {
        if (timer !== undefined) clearTimeout(timer);
        options.signal?.removeEventListener("abort", onAbort);
      };

      if (timeoutMs > 0) {
        timer = setTimeout(() => {
          this.#pending.delete(id);
          cleanup();
          reject(new JsonRpcTimeoutError(timeoutMs));
        }, timeoutMs);
        timer.unref();
      }

      options.signal?.addEventListener("abort", onAbort, { once: true });
      this.#pending.set(id, {
        resolve: (value) => resolve(value as R),
        reject,
        cleanup,
      });

      void this.#send(request).catch((error: unknown) => {
        const pending = this.#pending.get(id);
        if (pending === undefined) return;
        this.#pending.delete(id);
        pending.cleanup();
        pending.reject(error);
      });
    });
  }

  public async notify(method: string, params?: unknown): Promise<void> {
    const notification: JsonRpcNotification = {
      jsonrpc: JSON_RPC_VERSION,
      method,
      ...(params === undefined ? {} : { params }),
    };
    await this.#send(notification);
  }

  public handleRequest<M extends keyof RuntimeServerRequestMap>(
    method: M,
    handler: (
      params: RuntimeServerRequestMap[M]["params"],
    ) =>
      | RuntimeServerRequestMap[M]["result"]
      | Promise<RuntimeServerRequestMap[M]["result"]>,
  ): NotificationSubscription;
  public handleRequest(method: string, handler: RequestHandler): NotificationSubscription;
  public handleRequest(method: string, handler: RequestHandler): NotificationSubscription {
    if (this.#requestHandlers.has(method)) {
      throw new JsonRpcProtocolError(`A request handler is already registered for ${method}`);
    }
    this.#requestHandlers.set(method, handler);
    return {
      dispose: () => {
        if (this.#requestHandlers.get(method) === handler) {
          this.#requestHandlers.delete(method);
        }
      },
    };
  }

  public onNotification(
    method: string,
    handler: NotificationHandler,
  ): NotificationSubscription {
    const handlers = this.#notificationHandlers.get(method) ?? new Set();
    handlers.add(handler);
    this.#notificationHandlers.set(method, handlers);
    return {
      dispose: () => {
        handlers.delete(handler);
        if (handlers.size === 0) this.#notificationHandlers.delete(method);
      },
    };
  }

  public onEvent(handler: (event: EventEnvelope) => void): NotificationSubscription {
    return this.onNotification("event", (params) => {
      if (!isEventEnvelope(params)) {
        this.emit(
          "protocolError",
          new JsonRpcProtocolError("The event notification has an invalid envelope"),
        );
        return;
      }
      handler(params);
    });
  }

  public close(reason: Error = new JsonRpcConnectionClosedError()): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#readable.off("data", this.#onData);
    this.#readable.off("end", this.#onEnd);
    this.#readable.off("error", this.#onError);
    this.#writable.off("error", this.#onError);

    for (const pending of this.#pending.values()) {
      pending.cleanup();
      pending.reject(reason);
    }
    this.#pending.clear();
    this.emit("close", reason);
  }

  readonly #onData = (chunk: Buffer | string): void => {
    if (this.#closed) return;
    this.#buffer += typeof chunk === "string" ? chunk : this.#decoder.write(chunk);

    let newlineIndex = this.#buffer.indexOf("\n");
    while (newlineIndex >= 0) {
      const line = this.#buffer.slice(0, newlineIndex).replace(/\r$/, "");
      this.#buffer = this.#buffer.slice(newlineIndex + 1);
      if (Buffer.byteLength(line, "utf8") > this.#maxLineBytes) {
        this.close(new JsonRpcProtocolError("JSON-RPC line exceeds the configured limit"));
        return;
      }
      if (line.trim().length > 0) void this.#handleLine(line);
      newlineIndex = this.#buffer.indexOf("\n");
    }
    if (Buffer.byteLength(this.#buffer, "utf8") > this.#maxLineBytes) {
      this.close(new JsonRpcProtocolError("JSON-RPC line exceeds the configured limit"));
    }
  };

  readonly #onEnd = (): void => {
    const trailing = this.#buffer + this.#decoder.end();
    this.#buffer = "";
    if (trailing.trim().length > 0) void this.#handleLine(trailing);
    this.close();
  };

  readonly #onError = (error: Error): void => {
    this.close(error);
  };

  async #handleLine(line: string): Promise<void> {
    let message: unknown;
    try {
      message = JSON.parse(line) as unknown;
    } catch (cause) {
      this.emit("protocolError", new JsonRpcProtocolError("Invalid JSON-RPC JSON", { cause }));
      return;
    }

    if (!isRecord(message) || message.jsonrpc !== JSON_RPC_VERSION) {
      this.emit("protocolError", new JsonRpcProtocolError("Invalid JSON-RPC envelope"));
      return;
    }

    if ("method" in message && typeof message.method === "string") {
      if ("id" in message && (typeof message.id === "string" || typeof message.id === "number")) {
        await this.#handleInboundRequest(message.id, message.method, message.params);
      } else {
        this.#handleNotification(message.method, message.params);
      }
      return;
    }

    if (!("id" in message)) {
      this.emit("protocolError", new JsonRpcProtocolError("JSON-RPC response is missing id"));
      return;
    }

    const id = message.id;
    if (id !== null && typeof id !== "string" && typeof id !== "number") {
      this.emit("protocolError", new JsonRpcProtocolError("JSON-RPC response has an invalid id"));
      return;
    }

    const pending = this.#pending.get(id);
    if (pending === undefined) {
      this.emit("orphanResponse", message);
      return;
    }
    this.#pending.delete(id);
    pending.cleanup();

    if ("error" in message) {
      if (!isRecord(message.error) || typeof message.error.code !== "number" || typeof message.error.message !== "string") {
        pending.reject(new JsonRpcProtocolError("JSON-RPC response has an invalid error"));
        return;
      }
      pending.reject(
        new JsonRpcRemoteError({
          code: message.error.code,
          message: message.error.message,
          ...("data" in message.error ? { data: message.error.data } : {}),
        }),
      );
      return;
    }

    if (!("result" in message)) {
      pending.reject(new JsonRpcProtocolError("JSON-RPC response has neither result nor error"));
      return;
    }
    pending.resolve(message.result);
  }

  async #handleInboundRequest(
    id: Exclude<JsonRpcId, null>,
    method: string,
    params: unknown,
  ): Promise<void> {
    const handler = this.#requestHandlers.get(method);
    if (handler === undefined) {
      await this.#sendError(id, {
        code: JSON_RPC_ERROR.methodNotFound,
        message: `No client handler is registered for ${method}`,
      });
      return;
    }

    try {
      const result = await handler(params, { id, method });
      await this.#send({ jsonrpc: JSON_RPC_VERSION, id, result: result ?? null });
    } catch (error) {
      const rpcError = toJsonRpcError(error);
      await this.#sendError(id, rpcError);
    }
  }

  #handleNotification(method: string, params: unknown): void {
    const handlers = this.#notificationHandlers.get(method);
    if (handlers === undefined) {
      this.emit("notification", { method, params });
      return;
    }
    for (const handler of handlers) handler(params, method);
  }

  async #sendError(id: JsonRpcId, error: JsonRpcErrorObject): Promise<void> {
    await this.#send({ jsonrpc: JSON_RPC_VERSION, id, error });
  }

  async #send(message: unknown): Promise<void> {
    if (this.#closed) throw new JsonRpcConnectionClosedError();
    const line = `${JSON.stringify(message)}\n`;
    const operation = this.#writeChain.then(async () => {
      if (this.#closed) throw new JsonRpcConnectionClosedError();
      if (!this.#writable.write(line, "utf8")) await once(this.#writable, "drain");
    });
    this.#writeChain = operation.catch(() => undefined);
    await operation;
  }
}

function toJsonRpcError(error: unknown): JsonRpcErrorObject {
  if (error instanceof JsonRpcRemoteError) {
    return {
      code: error.code,
      message: error.message,
      ...(error.data === undefined ? {} : { data: error.data }),
    };
  }
  if (isRecord(error) && typeof error.code === "number" && typeof error.message === "string") {
    return {
      code: error.code,
      message: error.message,
      ...(error.data === undefined ? {} : { data: error.data }),
    };
  }
  return {
    code: JSON_RPC_ERROR.internalError,
    message: error instanceof Error ? error.message : "Client request handler failed",
  };
}
