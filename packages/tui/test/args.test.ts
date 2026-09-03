import { describe, expect, it } from "vitest";

import { parseArgs } from "../src/args.js";
import { parseApprovalChoice } from "../src/prompter.js";

describe("parseArgs", () => {
  it("parses a JSONL run", () => {
    expect(
      parseArgs(
        ["run", "-p", "fix the tests", "--jsonl", "--mode", "observe"],
        "/workspace",
      ),
    ).toMatchObject({
      command: "run",
      prompt: "fix the tests",
      jsonl: true,
      cwd: "/workspace",
      mode: "observe",
    });
  });

  it("rejects a run without a prompt", () => {
    expect(() => parseArgs(["run"])).toThrow("run requires a prompt");
  });

  it("defaults an unqualified turn to observe so read-only tools do not display build", () => {
    expect(parseArgs(["run", "-p", "status"], "/workspace").mode).toBe("observe");
    expect(parseArgs([], "/workspace").mode).toBe("observe");
  });

  it("parses a daemon-free fixture replay path", () => {
    expect(parseArgs(["replay", "fixtures/paper-approval-gate.jsonl"], "/workspace/tui"))
      .toMatchObject({
        command: "replay",
        replayPath: "/workspace/tui/fixtures/paper-approval-gate.jsonl",
      });
  });

  it("keeps replay mode when JSONL output is requested", () => {
    expect(parseArgs(["replay", "--jsonl", "fixtures/paper-approval-gate.jsonl"], "/workspace/tui"))
      .toMatchObject({
        command: "replay",
        jsonl: true,
        replayPath: "/workspace/tui/fixtures/paper-approval-gate.jsonl",
      });
  });

  it("still accepts --mode build as a request; the Session Bar must clamp display", () => {
    expect(parseArgs(["run", "-p", "status", "--mode", "build"], "/workspace").mode).toBe("build");
  });

  it("parses the operator-only reconciliation command", () => {
    expect(
      parseArgs([
        "reconcile",
        "--thread",
        "thread-1",
        "--invocation",
        "invocation-1",
        "--outcome",
        "completed",
        "--summary",
        "verified in the provider receipt",
        "--artifact-uri",
        "artifact://blake3/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "--jsonl",
      ], "/workspace"),
    ).toMatchObject({
      command: "reconcile",
      threadId: "thread-1",
      invocationId: "invocation-1",
      reconciliationOutcome: "completed",
      reconciliationSummary: "verified in the provider receipt",
      artifactUri: "artifact://blake3/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      jsonl: true,
    });
  });

  it("requires bounded reconciliation evidence fields", () => {
    expect(() => parseArgs(["reconcile", "--thread", "t", "--invocation", "i"])).toThrow(
      "reconcile requires --outcome completed|failed",
    );
    expect(() => parseArgs([
      "reconcile", "--thread", "t", "--invocation", "i", "--outcome", "completed", "--summary", "   ",
    ])).toThrow("reconcile requires a non-empty --summary");
    expect(() => parseArgs([
      "reconcile", "--thread", "t", "--invocation", "i", "--outcome", "completed", "--summary", "ok", "--artifact-uri", "file:///tmp/x",
    ])).toThrow("--artifact-uri must use the artifact:// scheme");
  });
});

describe("parseApprovalChoice", () => {
  it("defaults to deny", () => {
    expect(parseApprovalChoice("")).toBe("deny");
    expect(parseApprovalChoice("unexpected")).toBe("deny");
  });

  it("recognizes scoped approvals", () => {
    expect(parseApprovalChoice("1")).toBe("allow_once");
    expect(parseApprovalChoice("a")).toBe("allow_once");
    expect(parseApprovalChoice("2")).toBe("deny");
    expect(parseApprovalChoice("d")).toBe("deny");
    expect(parseApprovalChoice("i")).toBe("inspect");
  });
});
