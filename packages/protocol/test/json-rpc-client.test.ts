import { PassThrough } from "node:stream";

import { describe, expect, it, vi } from "vitest";

import {
  JsonRpcClient,
  JsonRpcRemoteError,
  type EventEnvelope,
} from "../src/index.js";

function createConnection(): {
  readonly client: JsonRpcClient;
  readonly toClient: PassThrough;
  readonly fromClient: PassThrough;
} {
  const toClient = new PassThrough();
  const fromClient = new PassThrough();
  const client = new JsonRpcClient(toClient, fromClient, {
    idFactory: () => "req-1",
    commandIdFactory: () => "cmd-1",
    requestTimeoutMs: 1_000,
  });
  return { client, toClient, fromClient };
}

function createDefaultConnection(): {
  readonly client: JsonRpcClient;
  readonly toClient: PassThrough;
  readonly fromClient: PassThrough;
} {
  const toClient = new PassThrough();
  const fromClient = new PassThrough();
  const client = new JsonRpcClient(toClient, fromClient, {
    idFactory: () => "req-1",
    requestTimeoutMs: 1_000,
  });
  return { client, toClient, fromClient };
}

async function nextLine(stream: PassThrough): Promise<Record<string, unknown>> {
  return await new Promise((resolve) => {
    stream.once("data", (chunk: Buffer) => {
      resolve(JSON.parse(chunk.toString("utf8").trim()) as Record<string, unknown>);
    });
  });
}

describe("JsonRpcClient", () => {
  it("sends a command envelope and resolves its response", async () => {
    const { client, toClient, fromClient } = createConnection();

    const pending = client.request<{ ok: boolean }>("turn/start", { prompt: "hello" });
    await expect(nextLine(fromClient)).resolves.toEqual({
      jsonrpc: "2.0",
      id: "req-1",
      command_id: "cmd-1",
      method: "turn/start",
      params: { prompt: "hello" },
    });

    toClient.write('{"jsonrpc":"2.0","id":"req-1","result":{"ok":true}}\n');
    await expect(pending).resolves.toEqual({ ok: true });
    client.close();
  });

  it("uses UUIDv7 command IDs by default", async () => {
    const { client, fromClient } = createDefaultConnection();
    void client.request("initialize", {}).catch(() => undefined);
    const request = await nextLine(fromClient);
    expect(request.command_id).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
    );
    client.close();
  });

  it("surfaces remote errors", async () => {
    const { client, toClient, fromClient } = createConnection();
    const pending = client.request("turn/start", {});
    await nextLine(fromClient);
    toClient.write(
      '{"jsonrpc":"2.0","id":"req-1","error":{"code":-32001,"message":"denied"}}\n',
    );
    await expect(pending).rejects.toMatchObject<JsonRpcRemoteError>({
      code: -32001,
      message: "denied",
    });
    client.close();
  });

  it("delivers validated event notifications split across chunks", async () => {
    const { client, toClient } = createConnection();
    const handler = vi.fn<(event: EventEnvelope) => void>();
    client.onEvent(handler);
    const event: EventEnvelope = {
      schema_version: { major: 1, minor: 0 },
      event_id: "evt-1",
      thread_id: "thread-1",
      agent_id: "agent-root",
      seq: 1,
      time: "2026-08-30T12:00:00Z",
      kind: "turn/started",
      payload: {},
    };
    const line = `${JSON.stringify({ jsonrpc: "2.0", method: "event", params: event })}\n`;
    toClient.write(line.slice(0, 14));
    toClient.write(line.slice(14));

    await vi.waitFor(() => expect(handler).toHaveBeenCalledWith(event));
    client.close();
  });

  it("answers inbound server requests through registered handlers", async () => {
    const { client, toClient, fromClient } = createConnection();
    client.handleRequest("approval/request", async () => ({ decision: "deny" }));
    toClient.write(
      '{"jsonrpc":"2.0","id":"approve-1","method":"approval/request","params":{}}\n',
    );

    await expect(nextLine(fromClient)).resolves.toEqual({
      jsonrpc: "2.0",
      id: "approve-1",
      result: { decision: "deny" },
    });
    client.close();
  });

  it("closes on an oversized line even when it follows a valid line", async () => {
    const toClient = new PassThrough();
    const fromClient = new PassThrough();
    const client = new JsonRpcClient(toClient, fromClient, { maxLineBytes: 32 });
    const closed = new Promise<Error>((resolve) => client.once("close", resolve));
    toClient.write(`{}\n${"x".repeat(33)}\n`);
    await expect(closed).resolves.toMatchObject({
      message: "JSON-RPC line exceeds the configured limit",
    });
  });
});
