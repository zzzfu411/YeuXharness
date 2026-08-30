import { PassThrough } from "node:stream";

import { describe, expect, it } from "vitest";

import { TerminalPrompter } from "../src/prompter.js";

describe("TerminalPrompter", () => {
  it("sanitizes untrusted approval text before writing it", async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    output.setEncoding("utf8");
    let rendered = "";
    output.on("data", (chunk: string) => {
      rendered += chunk;
    });
    const prompter = new TerminalPrompter(input, output);

    const resultPromise = prompter.approval({
      invocation: {
        invocation_id: "invocation-1",
        tool_id: "shell\u001b[2J",
        tool_version: "1.0",
        effects: { message: "effect\u001b]0;title\u0007" },
        effect_digest: "digest",
        normalized_arguments: {},
      },
      explanation: "review\u001b[31m this\u001b[0m",
    });
    await new Promise<void>((resolve) => setImmediate(resolve));
    input.write("2\n");

    await expect(resultPromise).resolves.toEqual({ approved: false });
    prompter.close();
    expect(rendered).not.toContain("\u001b");
    expect(rendered).not.toContain("\u0007");
    expect(rendered).toContain("Approval required: shell@1.0");
    expect(rendered).toContain("review this");
  });
});
