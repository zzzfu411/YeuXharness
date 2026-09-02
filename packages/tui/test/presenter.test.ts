import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import type { EventEnvelope } from "@yeux/protocol";
import type { TerminalCapabilities } from "../src/aesthetic.js";
import { formatApprovalGate } from "../src/prompter.js";
import {
  EventRenderer,
  formatInspector,
  formatSessionBar,
} from "../src/renderer.js";
import { replayFixture } from "../src/app.js";

const fixtureDir = join(dirname(fileURLToPath(import.meta.url)), "../fixtures");

const ASCII_CAPS: TerminalCapabilities = {
  isTTY: false,
  columns: 80,
  colorDepth: "none",
  unicode: false,
  plain: true,
  reducedMotion: true,
};

function loadFixture(name: string): EventEnvelope[] {
  return readFileSync(join(fixtureDir, name), "utf8")
    .trim()
    .split("\n")
    .filter((line) => line.length > 0)
    .map((line) => JSON.parse(line) as EventEnvelope);
}

function renderFixture(name: string): string {
  let output = "";
  const renderer = new EventRenderer({
    color: false,
    ascii: true,
    write: (text) => {
      output += text;
    },
  });
  for (const event of loadFixture(name)) renderer.render(event);
  return output;
}

describe("paper presenters", () => {
  it("always prints cwd, thread, mode and model on the Session Bar", () => {
    const line = formatSessionBar({
      cwd: "/tmp/workshop\u001b[2J",
      thread: "thread-1\nspoof",
      mode: "observe",
      model: "local/qwen",
    }, { capabilities: ASCII_CAPS });

    expect(line).toContain("><  YeuX / HARNESS");
    expect(line).toContain("CWD /tmp/workshop");
    expect(line).toContain("THREAD thread-1 spoof");
    expect(line).toContain("MODE OBSERVE");
    expect(line).toContain("MODEL local/qwen");
    expect(line).not.toContain("\u001b");
    expect(line).not.toContain("\n");
  });

  it("renders a double-line ASCII approval gate with deny as the default", () => {
    const gate = formatApprovalGate({
      invocation: {
        invocation_id: "invocation-gate",
        tool_id: "fixture.approval_boundary",
        tool_version: "1.0",
        effects: { external_writes: [{ system: "fixture", operation: "review" }] },
        effect_digest: "digest-1",
        normalized_arguments: { reason: "review" },
      },
      explanation: "human review boundary",
    }, { capabilities: ASCII_CAPS });

    expect(gate).toContain("+- ? APPROVAL REQUIRED · fixture.approval_boundary@1.0");
    expect(gate).toContain("|   human review boundary");
    expect(gate).toContain("| binding digest-1 · invocation invocation-gate");
    expect(gate).toContain("`- [a] ALLOW ONCE   [d] DENY (default)   [i] INSPECT");
    expect(gate).toContain("[i] INSPECT -+");
    expect(gate.split("\n")[0]?.endsWith("+")).toBe(true);
    expect(gate.split("\n").at(-1)?.endsWith("+")).toBe(true);
    expect(gate.split("\n").some((line) => line.startsWith("|") && line.endsWith("|"))).toBe(true);
    expect(gate).not.toContain("\u001b");
  });

  it("closes the Unicode approval gate on the right", () => {
    const gate = formatApprovalGate({
      invocation: {
        invocation_id: "invocation-unicode",
        tool_id: "fixture.approval_boundary",
        tool_version: "1.0",
        effects: { external_writes: [{ system: "fixture", operation: "review" }] },
        effect_digest: "digest-unicode",
        normalized_arguments: {},
      },
      explanation: "human review boundary",
    }, { capabilities: { ...ASCII_CAPS, unicode: true } });
    const lines = gate.split("\n");

    expect(lines[0]?.startsWith("╔")).toBe(true);
    expect(lines[0]?.endsWith("╗")).toBe(true);
    expect(lines.at(-1)?.startsWith("╚")).toBe(true);
    expect(lines.at(-1)?.endsWith("╝")).toBe(true);
    expect(lines.some((line) => line.startsWith("║") && line.endsWith("║"))).toBe(true);
    expect(gate).toContain("[a] ALLOW ONCE");
    expect(gate).toContain("[d] DENY (default)");
    expect(gate).toContain("[i] INSPECT");
  });

  it("cannot display MODE BUILD without a write grant and sandbox", () => {
    const requested = formatSessionBar({
      cwd: "/tmp/ws",
      thread: "thread-1",
      mode: "build",
      model: "local/qwen",
    }, { capabilities: ASCII_CAPS });
    expect(requested).not.toContain("MODE BUILD");
    expect(requested).toContain("MODE OBSERVE");

    const writeOnly = formatSessionBar({
      cwd: "/tmp/ws",
      thread: "thread-1",
      mode: "build",
      model: "local/qwen",
      writeGrant: ["/tmp/ws"],
      sandbox: false,
    }, { capabilities: ASCII_CAPS });
    expect(writeOnly).not.toContain("MODE BUILD");

    const sandboxOnly = formatSessionBar({
      cwd: "/tmp/ws",
      thread: "thread-1",
      mode: "build",
      model: "local/qwen",
      writeGrant: [],
      sandbox: true,
    }, { capabilities: ASCII_CAPS });
    expect(sandboxOnly).not.toContain("MODE BUILD");

    const granted = formatSessionBar({
      cwd: "/tmp/ws",
      thread: "thread-1",
      mode: "build",
      model: "local/qwen",
      writeGrant: ["/tmp/ws"],
      sandbox: true,
    }, { capabilities: ASCII_CAPS });
    expect(granted).toContain("MODE BUILD");
    expect(granted).not.toContain("MODE OBSERVE");
  });

  it("replays the approval-gate fixture as a sequenced waiting turn", () => {
    const output = renderFixture("paper-approval-gate.jsonl");
    expect(output).toContain("0001 | . START TURN fixture-turn-gate");
    expect(output).toContain("0002 +- o TOOL PROPOSED · human review boundary");
    expect(output).toContain("0003 | ? WAITING FOR APPROVAL · approval required; default DENY");
  });

  it("replay renders the approval-required fixture event through the closed gate", () => {
    let output = "";
    const status = replayFixture(join(fixtureDir, "paper-approval-gate.jsonl"), {
      ascii: true,
      write: (text) => { output += text; },
    });
    expect(status).toBe(0);
    expect(output).toContain("0003 | ? WAITING FOR APPROVAL · approval required; default DENY");
    expect(output).toContain("+- ? APPROVAL REQUIRED · fixture.approval_boundary@fixture -+");
    expect(output).toContain("[a] ALLOW ONCE");
    expect(output).toContain("[d] DENY (default)");
    expect(output).toContain("[i] INSPECT");
    expect(output).toContain("`- [a] ALLOW ONCE   [d] DENY (default)   [i] INSPECT -+");
    expect(output).toMatch(/\+- \? APPROVAL REQUIRED[^\n]*\+/);
  });

  it("keeps unknown visible after the turn fails", () => {
    const events = loadFixture("paper-unknown-failed.jsonl");
    const output = renderFixture("paper-unknown-failed.jsonl");
    const inspector = formatInspector({ events }, { capabilities: ASCII_CAPS });

    expect(output).toContain("0002 +- !! UNKNOWN · RECONCILIATION REQUIRED · outcome cannot be proven");
    expect(output).toContain("0003 | ERR FAILED · reconciliation required");
    expect(output.indexOf("UNKNOWN")).toBeLessThan(output.indexOf("FAILED"));
    expect(inspector).toContain("tool/state_changed · outcome cannot be proven");
    expect(inspector).toContain("turn/state_changed · reconciliation required");
  });

  it("marks Inspector policy unresolved when the client did not send an observe grant", () => {
    expect(formatInspector({ events: [] }, { capabilities: ASCII_CAPS })).toContain(
      "POLICY · unresolved",
    );
    expect(formatInspector({
      policy: {
        mode: "observe",
        filesystem_read: ["/tmp/ws"],
        filesystem_write: [],
        filesystem_delete: [],
        process: false,
        network: [],
        secrets: [],
        external_write: [],
      },
      events: [],
    }, { capabilities: ASCII_CAPS })).toContain("POLICY · MODE OBSERVE · filesystem_read /tmp/ws · filesystem_write none");
  });
});
