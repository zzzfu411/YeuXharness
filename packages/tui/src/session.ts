import type { InitializeResult, ModelDescriptor, RuntimeMode, Thread, Workspace } from "@yeux/protocol";

import {
  type TerminalCapabilities,
  type ThemeName,
  detectTerminalCapabilities,
  glyph,
  paint,
  type ColorRole,
} from "./aesthetic.js";
import { sanitizeTerminalText } from "./terminal.js";

export interface SessionSnapshot {
  readonly clientVersion: string;
  readonly serverName: string;
  readonly serverVersion: string;
  readonly protocolVersion: { readonly major: number; readonly minor: number };
  readonly hostCeiling: RuntimeMode;
  readonly mode: RuntimeMode;
  readonly workspace: Workspace;
  readonly thread: Thread;
  readonly transportKind: "socket" | "stdio";
  readonly transportDescription: string;
  readonly models: readonly ModelDescriptor[];
}

export interface SessionViewOptions {
  readonly capabilities?: TerminalCapabilities;
  readonly theme?: ThemeName;
}

export function snapshotFromSession(input: {
  readonly clientVersion: string;
  readonly initialize: InitializeResult;
  readonly workspace: Workspace;
  readonly thread: Thread;
  readonly transportKind: "socket" | "stdio";
  readonly transportDescription: string;
  readonly mode: RuntimeMode;
  readonly models: readonly ModelDescriptor[];
}): SessionSnapshot {
  return {
    clientVersion: input.clientVersion,
    serverName: input.initialize.serverInfo.name,
    serverVersion: input.initialize.serverInfo.version,
    protocolVersion: input.initialize.protocolVersion,
    hostCeiling: input.initialize.hostCeiling,
    mode: input.mode,
    workspace: input.workspace,
    thread: input.thread,
    transportKind: input.transportKind,
    transportDescription: input.transportDescription,
    models: input.models,
  };
}

export function formatWelcome(snapshot: SessionSnapshot, options: SessionViewOptions = {}): string {
  const { capabilities, theme } = viewOptions(options);
  const mark = glyph("brandCompact", capabilities);
  const lines = [
    color(`${mark}  YeuX / HARNESS  v${snapshot.clientVersion}`, "text", capabilities, theme),
    color("    PAPER SIGNAL · NOCTURNE", "focus", capabilities, theme),
    color("    local-first · replayable · explicit boundaries", "muted", capabilities, theme),
  ];
  return `${lines.join("\n")}\n`;
}

export function formatSessionBar(snapshot: SessionSnapshot, options: SessionViewOptions = {}): string {
  const { capabilities, theme } = viewOptions(options);
  const mark = glyph("brandCompact", capabilities);
  const connected = glyph("connected", capabilities);
  const identity = workspaceShortName(snapshot.workspace);
  const provider = providerLabel(snapshot);
  const transport =
    snapshot.transportKind === "socket"
      ? `${connected} SOCKET CONNECTED`
      : `${connected} STDIO`;
  const trust = snapshot.workspace.trust.toUpperCase();
  const mode = snapshot.mode.toUpperCase();

  if (capabilities.columns < 80) {
    return color(
      `${mark} ${mode} · ${identity} · ${trust}`,
      "text",
      capabilities,
      theme,
    );
  }

  if (capabilities.columns < 120) {
    return color(
      `${mark} YeuX  ${identity}  ${mode}  ${provider}  ${transport}`,
      "text",
      capabilities,
      theme,
    );
  }

  return color(
    `${mark}  YeuX / HARNESS   ${identity}   ${trust}   ${mode}   ${provider}   ${transport}`,
    "text",
    capabilities,
    theme,
  );
}

export function formatStatus(snapshot: SessionSnapshot, options: SessionViewOptions = {}): string {
  const { capabilities, theme } = viewOptions(options);
  const divider = glyph("strongDivider", capabilities).repeat(Math.min(48, Math.max(12, capabilities.columns - 4)));
  const transport =
    snapshot.transportKind === "socket"
      ? `${glyph("connected", capabilities)} ${singleLine(snapshot.transportDescription)}`
      : `${glyph("connected", capabilities)} ${singleLine(snapshot.transportDescription)}`;
  const provider = providerStatus(snapshot);
  const rows: ReadonlyArray<readonly [string, string, ColorRole]> = [
    ["workspace", snapshot.workspace.root, "text"],
    ["identity", shortDigest(snapshot.workspace.identity.digest), "muted"],
    ["trust", snapshot.workspace.trust, snapshot.workspace.trust === "trusted" ? "success" : "warning"],
    ["thread", snapshot.thread.id, "muted"],
    ["mode", snapshot.mode, "text"],
    ["ceiling", snapshot.hostCeiling, "muted"],
    ["daemon", `${snapshot.serverName} ${snapshot.serverVersion}`, "text"],
    ["transport", transport, snapshot.transportKind === "socket" ? "success" : "focus"],
    ["provider", provider.text, provider.role],
    [
      "protocol",
      `${snapshot.protocolVersion.major}.${snapshot.protocolVersion.minor}`,
      "muted",
    ],
    ["note", "v0.1 baseline · not a coding agent", "warning"],
  ];

  const lines = [
    color(divider, "muted", capabilities, theme),
    ...rows.map(([key, value, role]) => {
      const label = key.padEnd(10, " ");
      return `${color(label, "muted", capabilities, theme)} ${color(singleLine(value), role, capabilities, theme)}`;
    }),
  ];
  return `${lines.join("\n")}\n`;
}

export function formatHint(text: string, options: SessionViewOptions = {}): string {
  const { capabilities, theme } = viewOptions(options);
  return color(text, "muted", capabilities, theme);
}

export function formatInteractiveHelp(options: SessionViewOptions = {}): string {
  const { capabilities, theme } = viewOptions(options);
  const divider = glyph("strongDivider", capabilities).repeat(Math.min(48, Math.max(12, capabilities.columns - 4)));
  const lines = [
    color(divider, "muted", capabilities, theme),
    color("  /help     this screen", "text", capabilities, theme),
    color("  /status   workspace, transport, provider", "text", capabilities, theme),
    color("  /exit     close the client", "text", capabilities, theme),
    color("  A prompt that is not a slash command starts one Turn.", "muted", capabilities, theme),
    color("  Unconfigured provider turns fail honestly with provider_unconfigured.", "muted", capabilities, theme),
  ];
  return `${lines.join("\n")}\n`;
}

export function workspaceShortName(workspace: Workspace): string {
  const base = workspace.root.replace(/\/+$/, "").split("/").pop() || "workspace";
  const tag = workspace.identity.digest.replace(/[^a-fA-F0-9]/g, "").slice(0, 4) || "----";
  return `${singleLine(base)}/${tag}`;
}

function providerLabel(snapshot: SessionSnapshot): string {
  const first = snapshot.models[0];
  if (first === undefined) return "unconfigured";
  return `${first.provider}/${first.model}`;
}

function providerStatus(snapshot: SessionSnapshot): { readonly text: string; readonly role: ColorRole } {
  if (snapshot.models.length === 0) {
    return {
      text: "unconfigured  (turn/start fails until --provider-base-url and --model)",
      role: "warning",
    };
  }
  return {
    text: snapshot.models.map((model) => `${model.provider}/${model.model}`).join(", "),
    role: "success",
  };
}

function shortDigest(digest: string): string {
  const compact = digest.replace(/[^a-fA-F0-9]/g, "");
  if (compact.length <= 12) return digest;
  return `${compact.slice(0, 8)}…${compact.slice(-4)}`;
}

function viewOptions(options: SessionViewOptions): {
  readonly capabilities: TerminalCapabilities;
  readonly theme: ThemeName;
} {
  return {
    capabilities: options.capabilities ?? detectTerminalCapabilities(),
    theme: options.theme ?? "nocturne",
  };
}

function color(
  text: string,
  role: ColorRole,
  capabilities: TerminalCapabilities,
  theme: ThemeName,
): string {
  return paint(sanitizeTerminalText(text), role, capabilities, theme);
}

function singleLine(value: string): string {
  return sanitizeTerminalText(value).replace(/[\r\n\t]+/g, " ").trim();
}
