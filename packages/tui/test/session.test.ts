import { describe, expect, it } from "vitest";

import { detectTerminalCapabilities } from "../src/aesthetic.js";
import {
  formatHint,
  formatInteractiveHelp,
  formatSessionBar,
  formatStatus,
  formatWelcome,
  snapshotFromSession,
  workspaceShortName,
} from "../src/session.js";

const capabilities = detectTerminalCapabilities({
  isTTY: true,
  columns: 120,
  color: false,
  ascii: true,
  plain: true,
});

const snapshot = snapshotFromSession({
  clientVersion: "0.1.0",
  initialize: {
    protocolVersion: { major: 1, minor: 0 },
    serverInfo: { name: "yeuxd", version: "0.1.0" },
    capabilities: { unix_socket: true, jobs: false, subagents: false, plugins: false },
    hostCeiling: "operate",
  },
  workspace: {
    id: "0193aaaa-bbbb-7ccc-dddd-eeeeeeeeeeee",
    root: "/workspace/yeux-harness",
    identity: {
      canonical_root: "/workspace/yeux-harness",
      digest: "abcdef0123456789deadbeef",
    },
    trust: "untrusted",
    opened_at: "2026-08-30T12:00:00Z",
  },
  thread: {
    id: "0193ffff-1111-7222-8333-444444444444",
    workspace_id: "0193aaaa-bbbb-7ccc-dddd-eeeeeeeeeeee",
    status: "active",
    created_at: "2026-08-30T12:00:00Z",
    updated_at: "2026-08-30T12:00:00Z",
    last_seq: 0,
  },
  transportKind: "socket",
  transportDescription: "/tmp/yeux-1000/yeuxd.sock",
  mode: "build",
  models: [],
});

describe("session view", () => {
  it("formats a welcome plate from live session facts", () => {
    const text = formatWelcome(snapshot, { capabilities });
    expect(text).toContain("><  YeuX / HARNESS  v0.1.0");
    expect(text).toContain("PAPER SIGNAL");
    expect(text).toContain("local-first");
  });

  it("formats a wide session bar with workspace, mode and transport", () => {
    const text = formatSessionBar(snapshot, { capabilities });
    expect(text).toContain("YeuX / HARNESS");
    expect(text).toContain("yeux-harness/abcd");
    expect(text).toContain("UNTRUSTED");
    expect(text).toContain("BUILD");
    expect(text).toContain("unconfigured");
    expect(text).toContain("SOCKET CONNECTED");
  });

  it("formats status without inventing a configured provider", () => {
    const text = formatStatus(snapshot, { capabilities });
    expect(text).toContain("/workspace/yeux-harness");
    expect(text).toContain("untrusted");
    expect(text).toContain("yeuxd 0.1.0");
    expect(text).toContain("/tmp/yeux-1000/yeuxd.sock");
    expect(text).toContain("unconfigured");
    expect(text).toContain("v0.1 baseline");
    expect(text).not.toContain("local/qwen");
  });

  it("lists interactive slash commands", () => {
    const text = formatInteractiveHelp({ capabilities });
    expect(text).toContain("/help");
    expect(text).toContain("/status");
    expect(text).toContain("/exit");
    expect(text).toContain("provider_unconfigured");
  });

  it("shortens workspace identity for the session bar", () => {
    expect(workspaceShortName(snapshot.workspace)).toBe("yeux-harness/abcd");
  });

  it("formats a muted hint without introducing a fake provider", () => {
    expect(formatHint("Type a prompt to start a turn, or /help.", { capabilities })).toContain(
      "/help",
    );
  });
});
