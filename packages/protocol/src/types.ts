export const JSON_RPC_VERSION = "2.0" as const;

export const PROTOCOL_VERSION = Object.freeze({ major: 2, minor: 0 });

export type JsonRpcId = string | number | null;

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue =
  | JsonPrimitive
  | { readonly [key: string]: JsonValue }
  | readonly JsonValue[];
export type JsonObject = { readonly [key: string]: JsonValue };

export interface ProtocolVersion {
  readonly major: number;
  readonly minor: number;
}

export interface JsonRpcRequest<P = unknown> {
  readonly jsonrpc: typeof JSON_RPC_VERSION;
  readonly id: Exclude<JsonRpcId, null>;
  readonly method: string;
  readonly params?: P;
}

export interface CommandEnvelope<P = unknown> extends JsonRpcRequest<P> {
  readonly command_id: string;
}

export interface JsonRpcNotification<P = unknown> {
  readonly jsonrpc: typeof JSON_RPC_VERSION;
  readonly method: string;
  readonly params?: P;
}

export interface JsonRpcErrorObject<D = unknown> {
  readonly code: number;
  readonly message: string;
  readonly data?: D;
}

export interface JsonRpcSuccessResponse<R = unknown> {
  readonly jsonrpc: typeof JSON_RPC_VERSION;
  readonly id: JsonRpcId;
  readonly result: R;
}

export interface JsonRpcErrorResponse<D = unknown> {
  readonly jsonrpc: typeof JSON_RPC_VERSION;
  readonly id: JsonRpcId;
  readonly error: JsonRpcErrorObject<D>;
}

export type JsonRpcResponse<R = unknown, D = unknown> =
  | JsonRpcSuccessResponse<R>
  | JsonRpcErrorResponse<D>;

export type JsonRpcMessage =
  | JsonRpcRequest
  | JsonRpcNotification
  | JsonRpcResponse;

export type KnownEventKind =
  | "workspace/opened"
  | "workspace/trust_changed"
  | "thread/started"
  | "thread/forked"
  | "thread/archived"
  | "turn/started"
  | "turn/state_changed"
  | "turn/steered"
  | "item/added"
  | "model/requested"
  | "model/event"
  | "tool/proposed"
  | "tool/state_changed"
  | "tool/reconciled"
  | "job/created"
  | "job/state_changed"
  | "agent/spawned"
  | "agent/completed"
  | "runtime/diagnostic";

export type EventKind = KnownEventKind | (string & {});

export interface EventEnvelope<P = JsonObject> {
  readonly schema_version: ProtocolVersion;
  readonly event_id: string;
  readonly thread_id: string;
  readonly turn_id?: string;
  readonly agent_id: string;
  readonly seq: number;
  readonly time: string;
  readonly causation_id?: string;
  readonly kind: EventKind;
  readonly payload: P;
}

export type RuntimeMode = "observe" | "build" | "operate";

export type WorkspaceTrust = "untrusted" | "trusted";
export type ThreadStatus = "active" | "idle" | "archived" | "failed";
export type TurnState =
  | "accepted"
  | "building_context"
  | "requesting_model"
  | "streaming"
  | "proposed_tools"
  | "waiting_for_approval"
  | "authorizing"
  | "scheduling"
  | "executing"
  | "integrating_results"
  | "waiting_for_input"
  | "cancelling"
  | "completed"
  | "cancelled"
  | "failed";

export interface WorkspaceIdentity {
  readonly canonical_root: string;
  readonly digest: string;
  readonly device?: number;
  readonly inode?: number;
  readonly git_common_dir?: string;
}

export interface Workspace {
  readonly id: string;
  readonly root: string;
  readonly identity: WorkspaceIdentity;
  readonly trust: WorkspaceTrust;
  readonly opened_at: string;
}

export interface Thread {
  readonly id: string;
  readonly workspace_id: string;
  readonly parent_thread_id?: string;
  readonly parent_seq?: number;
  readonly title?: string;
  readonly status: ThreadStatus;
  readonly created_at: string;
  readonly updated_at: string;
  readonly last_seq: number;
}

export interface Turn {
  readonly id: string;
  readonly thread_id: string;
  readonly agent_id: string;
  readonly state: TurnState;
  readonly started_at: string;
  readonly ended_at?: string;
  readonly failure?: string;
}

export type ContentBlock =
  | { readonly type: "text"; readonly text: string }
  | { readonly type: "reasoning"; readonly text: string }
  | ({ readonly type: "image"; readonly media_type: string } & (
      | { readonly source_type: "url"; readonly url: string }
      | { readonly source_type: "base64"; readonly data: string }
      | { readonly source_type: "artifact"; readonly uri: string }
    ))
  | {
      readonly type: "tool_call";
      readonly call_id: string;
      readonly name: string;
      readonly arguments: JsonValue;
    }
  | {
      readonly type: "tool_result";
      readonly call_id: string;
      readonly content: JsonValue;
      readonly is_error?: boolean;
    };

export interface CapabilityGrant {
  readonly mode: RuntimeMode;
  readonly filesystem_read?: readonly string[];
  readonly filesystem_write?: readonly string[];
  readonly filesystem_delete?: readonly string[];
  readonly process?: boolean;
  readonly network?: readonly string[];
  readonly secrets?: readonly string[];
  readonly external_write?: readonly string[];
  readonly expires_at?: string;
}

export interface ClientInfo {
  readonly name: string;
  readonly version: string;
}

export interface InitializeParams {
  readonly protocolVersion: ProtocolVersion;
  readonly clientInfo: ClientInfo;
  readonly capabilities: {
    readonly event_replay?: boolean;
    readonly server_requests?: boolean;
    readonly rich_content?: boolean;
  };
}

export interface InitializeResult {
  readonly protocolVersion: ProtocolVersion;
  readonly serverInfo: ClientInfo;
  readonly capabilities: {
    readonly unix_socket: boolean;
    readonly jobs: boolean;
    readonly subagents: boolean;
    readonly plugins: boolean;
    readonly write_tools?: boolean;
    readonly process_tools?: boolean;
    readonly sandbox?: string;
  };
  readonly hostCeiling: RuntimeMode;
}

export interface WorkspaceOpenParams {
  readonly path: string;
}

export interface WorkspaceOpenResult {
  readonly workspace: Workspace;
}

export interface WorkspaceStatusParams {
  readonly workspaceId: string;
}

export interface WorkspaceStatusResult {
  readonly workspace: Workspace;
  readonly activeThreadId?: string;
}

export interface ThreadStartParams {
  readonly workspaceId: string;
  readonly title?: string;
  readonly agentId?: string;
}

export interface ThreadResult {
  readonly thread: Thread;
}

export interface ThreadResumeParams {
  readonly threadId: string;
  readonly afterSeq?: number;
}

export interface ThreadReadParams {
  readonly threadId: string;
  readonly afterSeq?: number;
  readonly limit?: number;
}

export interface ThreadReadResult {
  readonly thread: Thread;
  readonly events: readonly EventEnvelope[];
  readonly nextAfterSeq?: number;
}

export type ThreadResumeResult = ThreadReadResult;

export interface ThreadSubscribeParams {
  readonly threadId: string;
  readonly afterSeq?: number;
}

export interface ThreadSubscribeResult {
  readonly subscriptionId: string;
  readonly replayedThroughSeq: number;
}

export interface TurnStartParams {
  readonly threadId: string;
  readonly agentId?: string;
  readonly content: readonly ContentBlock[];
  readonly capabilityOverride?: CapabilityGrant;
}

export interface TurnResult {
  readonly turn: Turn;
}

export interface TurnInterruptParams {
  readonly threadId: string;
  readonly turnId: string;
  readonly reason?: string;
}

export interface AcceptedResult {
  readonly accepted: boolean;
}

export interface RuntimeDiagnosticNotification {
  readonly code: string;
  readonly message: string;
  readonly recoverable: boolean;
  readonly thread_id?: string;
  readonly expected_seq?: number;
  readonly actual_seq?: number;
}

export interface ApprovalRequestParams {
  readonly invocation: {
    readonly invocation_id: string;
    readonly tool_id: string;
    readonly tool_version: string;
    readonly effects: JsonObject;
    readonly effect_digest: string;
    readonly normalized_arguments: JsonValue;
  } & JsonObject;
  readonly explanation: string;
  readonly unifiedDiff?: string;
  readonly unified_diff?: string;
}

export interface ApprovalRequestResult {
  readonly approved: boolean;
  readonly approval?: JsonObject;
}

export interface UserInputRequestParams {
  readonly threadId: string;
  readonly turnId: string;
  readonly prompt: string;
  readonly metadata: JsonValue;
}

export interface UserInputRequestResult {
  readonly content: readonly ContentBlock[];
}

export interface ModelDescriptor {
  readonly provider: string;
  readonly model: string;
  readonly display_name: string;
  readonly capabilities: JsonObject;
}

export interface RuntimeCommandMap {
  readonly initialize: {
    readonly params: InitializeParams;
    readonly result: InitializeResult;
  };
  readonly "workspace/open": {
    readonly params: WorkspaceOpenParams;
    readonly result: WorkspaceOpenResult;
  };
  readonly "workspace/status": {
    readonly params: WorkspaceStatusParams;
    readonly result: WorkspaceStatusResult;
  };
  readonly "thread/start": {
    readonly params: ThreadStartParams;
    readonly result: ThreadResult;
  };
  readonly "thread/resume": {
    readonly params: ThreadResumeParams;
    readonly result: ThreadResumeResult;
  };
  readonly "thread/read": {
    readonly params: ThreadReadParams;
    readonly result: ThreadReadResult;
  };
  readonly "thread/subscribe": {
    readonly params: ThreadSubscribeParams;
    readonly result: ThreadSubscribeResult;
  };
  readonly "turn/start": {
    readonly params: TurnStartParams;
    readonly result: TurnResult;
  };
  readonly "turn/interrupt": {
    readonly params: TurnInterruptParams;
    readonly result: AcceptedResult;
  };
  readonly "model/list": {
    readonly params: { readonly provider?: string };
    readonly result: { readonly models: readonly ModelDescriptor[] };
  };
}

export type RuntimeCommandMethod = keyof RuntimeCommandMap;

export interface RuntimeServerRequestMap {
  readonly "approval/request": {
    readonly params: ApprovalRequestParams;
    readonly result: ApprovalRequestResult;
  };
  readonly "user/input": {
    readonly params: UserInputRequestParams;
    readonly result: UserInputRequestResult;
  };
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function isProtocolVersion(value: unknown): value is ProtocolVersion {
  return (
    isRecord(value) &&
    Number.isSafeInteger(value.major) &&
    Number.isSafeInteger(value.minor) &&
    (value.major as number) >= 0 &&
    (value.minor as number) >= 0
  );
}

export function isEventEnvelope(value: unknown): value is EventEnvelope {
  if (!isRecord(value)) return false;

  return (
    isProtocolVersion(value.schema_version) &&
    typeof value.event_id === "string" &&
    typeof value.thread_id === "string" &&
    (value.turn_id === undefined || typeof value.turn_id === "string") &&
    typeof value.agent_id === "string" &&
    Number.isSafeInteger(value.seq) &&
    (value.seq as number) >= 0 &&
    typeof value.time === "string" &&
    (value.causation_id === undefined || typeof value.causation_id === "string") &&
    typeof value.kind === "string" &&
    isRecord(value.payload)
  );
}

export function isRuntimeDiagnosticNotification(
  value: unknown,
): value is RuntimeDiagnosticNotification {
  if (!isRecord(value)) return false;
  return (
    typeof value.code === "string" &&
    typeof value.message === "string" &&
    typeof value.recoverable === "boolean" &&
    (value.thread_id === undefined || typeof value.thread_id === "string") &&
    (value.expected_seq === undefined || Number.isSafeInteger(value.expected_seq)) &&
    (value.actual_seq === undefined || Number.isSafeInteger(value.actual_seq))
  );
}
