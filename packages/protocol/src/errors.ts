import type { JsonRpcErrorObject } from "./types.js";

export class JsonRpcProtocolError extends Error {
  public constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = "JsonRpcProtocolError";
  }
}

export class JsonRpcConnectionClosedError extends Error {
  public constructor(message = "The JSON-RPC connection is closed") {
    super(message);
    this.name = "JsonRpcConnectionClosedError";
  }
}

export class JsonRpcRemoteError<D = unknown> extends Error {
  public readonly code: number;
  public readonly data: D | undefined;

  public constructor(error: JsonRpcErrorObject<D>) {
    super(error.message);
    this.name = "JsonRpcRemoteError";
    this.code = error.code;
    this.data = error.data;
  }
}

export class JsonRpcTimeoutError extends Error {
  public constructor(timeoutMs: number) {
    super(`JSON-RPC request timed out after ${timeoutMs} ms`);
    this.name = "JsonRpcTimeoutError";
  }
}
