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
