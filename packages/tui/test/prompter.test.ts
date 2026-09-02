import { PassThrough } from "node:stream";

import { describe, expect, it } from "vitest";

import { formatApprovalGate, isReadOnlyEffects, TerminalPrompter } from "../src/prompter.js";

describe("TerminalPrompter", () => {
  it("sanitizes untrusted approval text before writing it", async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    output.setEncoding("utf8");
    let rendered = "";
    output.on("data", (chunk: string) => {
      rendered += chunk;
    });
    // Keep the sanitizer assertion independent of the host/CI locale and
    // terminal capabilities; the ASCII rail makes the expected framing
    // deterministic while the rich glyphs have dedicated aesthetic tests.
    const prompter = new TerminalPrompter(input, output, { ascii: true });

    const resultPromise = prompter.approval({
      invocation: {
        invocation_id: "invocation-1",
        tool_id: "shell\nspoof\u001b[2J",
        tool_version: "1.0",
        effects: { message: "effect\u001b]0;title\u0007" },
        effect_digest: "digest",
        normalized_arguments: {},
      },
      explanation: "review\u001b[31m this\u001b[0m\n+- fake footer",
    });
    await new Promise<void>((resolve) => setImmediate(resolve));
    input.write("2\n");

    await expect(resultPromise).resolves.toEqual({ approved: false });
    prompter.close();
    expect(rendered).not.toContain("\u001b");
    expect(rendered).not.toContain("\u0007");
    expect(rendered).toContain("APPROVAL REQUIRED · shell spoof@1.0");
    expect(rendered).toContain("review this");
    expect(rendered).not.toContain("\n+- fake footer");
    expect(rendered).toContain("\n|   +- fake footer");
    expect(rendered).toContain("[d] DENY (default)");
  });

  it("uses the Paper prompt token in a rich terminal", async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    output.setEncoding("utf8");
    let rendered = "";
    output.on("data", (chunk: string) => {
      rendered += chunk;
    });
    const prompter = new TerminalPrompter(input, output, {
      isTTY: true,
      env: {
        TERM: "xterm-256color",
        COLORTERM: "truecolor",
        LANG: "en_US.UTF-8",
      },
    });

    const answer = prompter.command();
    await new Promise<void>((resolve) => setImmediate(resolve));
    input.write("status\n");

    await expect(answer).resolves.toBe("status");
    prompter.close();
    expect(rendered).toContain(
      "\u001b[38;2;49;86;107m\u001b[48;2;216;211;204myeux ›\u001b[0m ",
    );
  });

  it("auto-approves only proven read-only effect sets", () => {
    expect(isReadOnlyEffects({
      filesystem_read: [{ path: "README.md" }],
      filesystem_write: [],
      filesystem_delete: [],
      processes: [],
      network: [],
      secrets: [],
      external_writes: [],
      idempotency: "idempotent",
      reversibility: "reversible",
    })).toBe(true);
    expect(isReadOnlyEffects({ filesystem_read: ["/workspace"] })).toBe(true);
    expect(isReadOnlyEffects({
      filesystem_read: [],
      external_writes: [{ system: "fixture", operation: "review" }],
    })).toBe(false);
    expect(isReadOnlyEffects({ filesystem_read: [], process: true })).toBe(false);
    expect(isReadOnlyEffects({ filesystem_read: [], clipboard: true })).toBe(false);
    expect(isReadOnlyEffects({})).toBe(true);
    expect(isReadOnlyEffects(null)).toBe(false);
  });

  it("supports an inspect step without weakening deny-by-default", async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    output.setEncoding("utf8");
    let rendered = "";
    output.on("data", (chunk: string) => {
      rendered += chunk;
    });
    const prompter = new TerminalPrompter(input, output, {
      isTTY: true,
      env: { TERM: "dumb", LANG: "en_US.UTF-8" },
    });

    const resultPromise = prompter.approval({
      invocation: {
        invocation_id: "invocation-2",
        tool_id: "workspace.apply_patch",
        tool_version: "1.0",
        effects: { filesystem_write: [{ path: "src/app.ts" }] },
        effect_digest: "d42f91c8",
        normalized_arguments: { patch: "safe" },
      },
      explanation: "Write one file",
    });
    await new Promise<void>((resolve) => setImmediate(resolve));
    input.write("i\n");
    await new Promise<void>((resolve) => setImmediate(resolve));
    input.write("\n");

    await expect(resultPromise).resolves.toEqual({ approved: false });
    prompter.close();
    expect(rendered).not.toContain("\u001b");
    expect(rendered).toContain("+- ? APPROVAL REQUIRED · workspace.apply_patch@1.0");
    expect(rendered).toContain("INSPECT · NORMALIZED ARGUMENTS");
    expect(rendered).toContain('"patch": "safe"');
    expect(rendered).toContain("[a] ALLOW ONCE");
    expect(rendered).toContain("[d] DENY (default)");
    expect(rendered).toContain("[i] INSPECT");
  });

  it("draws a closed vermillion box using ╗ and ╝", () => {
    const gate = formatApprovalGate({
      invocation: {
        invocation_id: "invocation-box",
        tool_id: "workspace.apply_patch",
        tool_version: "1.0",
        effects: { filesystem_write: [{ path: "src/app.ts" }] },
        effect_digest: "d42f91c8",
        normalized_arguments: { patch: "safe" },
      },
      explanation: "Write one file",
    }, {
      capabilities: {
        isTTY: true,
        columns: 80,
        colorDepth: "none",
        unicode: true,
        plain: true,
        reducedMotion: true,
      },
    });
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

  it("inspects a unified diff when the approval payload includes one", async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    output.setEncoding("utf8");
    let rendered = "";
    output.on("data", (chunk: string) => {
      rendered += chunk;
    });
    const prompter = new TerminalPrompter(input, output, {
      isTTY: true,
      env: { TERM: "dumb", LANG: "en_US.UTF-8" },
    });

    const resultPromise = prompter.approval({
      invocation: {
        invocation_id: "invocation-diff",
        tool_id: "workspace.apply_patch",
        tool_version: "1.0",
        effects: { filesystem_write: [{ path: "src/app.ts" }] },
        effect_digest: "d42f91c8",
        normalized_arguments: { path: "src/app.ts" },
      },
      explanation: "Write one file",
      unifiedDiff: "--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\n",
    });
    await new Promise<void>((resolve) => setImmediate(resolve));
    input.write("i\n");
    await new Promise<void>((resolve) => setImmediate(resolve));
    input.write("\n");

    await expect(resultPromise).resolves.toEqual({ approved: false });
    prompter.close();
    expect(rendered).toContain("INSPECT · UNIFIED DIFF");
    expect(rendered).toContain("--- a/src/app.ts");
    expect(rendered).toContain("-old");
    expect(rendered).toContain("+new");
    expect(rendered).not.toContain("INSPECT · NORMALIZED ARGUMENTS");
  });
});
