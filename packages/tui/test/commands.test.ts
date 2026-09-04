import { PassThrough } from "node:stream";

import { describe, expect, it } from "vitest";

import {
  displayWidth,
  TerminalPrompter,
} from "../src/prompter.js";
import {
  parseInteractiveCommand,
  resolveEffectiveMode,
} from "../src/commands.js";

describe("interactive command grammar", () => {
  it("keeps ordinary prompts out of the slash command router", () => {
    expect(parseInteractiveCommand(" fix the failing test ")).toEqual({
      kind: "prompt",
      text: "fix the failing test",
    });
  });

  it("parses plan, mode and recovery commands", () => {
    expect(parseInteractiveCommand("/plan add \"run the focused test\"")).toEqual({
      kind: "plan",
      plan: { action: "add", text: "run the focused test" },
    });
    expect(parseInteractiveCommand("/mode build")).toEqual({ kind: "mode", mode: "build" });
    expect(parseInteractiveCommand("/doctor")).toEqual({ kind: "doctor" });
    expect(parseInteractiveCommand("/fork \"checkpoint review\"")).toEqual({
      kind: "fork",
      title: "checkpoint review",
    });
    expect(parseInteractiveCommand("/reconcile inv-1 failed \"test was not rerun\"")).toEqual({
      kind: "reconcile",
      invocationId: "inv-1",
      outcome: "failed",
      summary: "test was not rerun",
    });
  });

  it("supports long-form reconciliation evidence and rejects malformed input", () => {
    expect(parseInteractiveCommand(
      "/reconcile --invocation inv-1 --outcome completed --summary \"verified locally\" --artifact-uri artifact://blake3/abc",
    )).toMatchObject({
      kind: "reconcile",
      invocationId: "inv-1",
      outcome: "completed",
      summary: "verified locally",
      artifactUri: "artifact://blake3/abc",
    });
    expect(() => parseInteractiveCommand("/mode danger")).toThrow("/mode requires");
    expect(() => parseInteractiveCommand("/plan add \"unfinished")).toThrow("unterminated");
  });

  it("does not silently route unknown slash commands to the model", () => {
    expect(parseInteractiveCommand("/deploy now")).toEqual({ kind: "unknown", name: "deploy" });
  });
});

describe("effective capability mode", () => {
  it("mirrors host, project trust and tool readiness as a shrinking intersection", () => {
    expect(resolveEffectiveMode({
      requested: "operate",
      hostCeiling: "operate",
      workspaceTrust: "trusted",
      writeReady: true,
    })).toBe("build");
    expect(resolveEffectiveMode({
      requested: "build",
      hostCeiling: "operate",
      workspaceTrust: "untrusted",
      writeReady: true,
    })).toBe("observe");
    expect(resolveEffectiveMode({
      requested: "build",
      hostCeiling: "observe",
      workspaceTrust: "trusted",
      writeReady: true,
    })).toBe("observe");
    expect(resolveEffectiveMode({
      requested: "build",
      hostCeiling: "operate",
      workspaceTrust: "trusted",
      writeReady: false,
    })).toBe("observe");
  });
});

describe("terminal cell width and EOF", () => {
  it("counts wide CJK/emoji and ignores combining marks", () => {
    expect(displayWidth("abc")).toBe(3);
    expect(displayWidth("中文")).toBe(4);
    expect(displayWidth("e\u0301")).toBe(1);
    expect(displayWidth("🦊")).toBe(2);
    expect(displayWidth("👩‍💻")).toBe(2);
    expect(displayWidth("🇨🇳")).toBe(2);
    expect(displayWidth("❤️")).toBe(2);
    expect(displayWidth("1️⃣")).toBe(2);
  });

  it("resolves command input with undefined when stdin reaches EOF", async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const prompter = new TerminalPrompter(input, output, { isTTY: false });
    input.end();
    await expect(prompter.command()).resolves.toBeUndefined();
    prompter.close();
  });

  it("can abort an active command read without closing the terminal", async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const prompter = new TerminalPrompter(input, output, { isTTY: false });
    const controller = new AbortController();
    const interrupted = prompter.command(controller.signal);
    await new Promise<void>((resolve) => setImmediate(resolve));
    controller.abort();
    await expect(interrupted).resolves.toBe("");

    const next = prompter.command();
    await new Promise<void>((resolve) => setImmediate(resolve));
    input.write("/help\n");
    await expect(next).resolves.toBe("/help");
    prompter.close();
  });
});
