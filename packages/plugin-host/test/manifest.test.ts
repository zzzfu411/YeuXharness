import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  PluginHost,
  resolveGrantedCapabilities,
  validatePluginManifest,
} from "../src/index.js";

const fixtureRoot = join(dirname(fileURLToPath(import.meta.url)), "fixtures");
const fixtureName = "echo-plugin.mjs";

async function fixtureManifest() {
  const bytes = await readFile(join(fixtureRoot, fixtureName));
  return validatePluginManifest({
    id: "dev.yeux.echo",
    version: "1.0.0",
    protocol: "1",
    executable: fixtureName,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  });
}

describe("plugin manifest", () => {
  it("defaults to no requested or granted capabilities", async () => {
    const manifest = await fixtureManifest();
    expect(manifest.requested_capabilities).toEqual([]);
    expect(resolveGrantedCapabilities(manifest)).toEqual([]);
  });

  it("rejects executable path traversal", () => {
    expect(() =>
      validatePluginManifest({
        id: "dev.yeux.bad",
        version: "1.0.0",
        protocol: "1",
        executable: "../escape",
        sha256: "0".repeat(64),
      }),
    ).toThrow("safe path");
  });

  it("does not grant capabilities that the manifest did not request", async () => {
    const manifest = await fixtureManifest();
    expect(() => resolveGrantedCapabilities(manifest, ["fs:read:workspace"])).toThrow(
      "was not requested",
    );
  });
});

describe("PluginHost", () => {
  it("starts an isolated plugin process and namespaces its tools", async () => {
    const manifest = await fixtureManifest();
    const host = new PluginHost({ manifest, root: fixtureRoot });
    try {
      await expect(host.start()).resolves.toMatchObject({
        id: "dev.yeux.echo",
        granted_capabilities: [],
        tools: [{ id: "dev.yeux.echo/echo" }],
      });
      await expect(
        host.invoke({ tool_id: "dev.yeux.echo/echo", input: { value: "hello" } }),
      ).resolves.toEqual({ echoed: { value: "hello" } });
    } finally {
      await host.close();
    }
  });
});
