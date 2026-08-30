import { describe, expect, it } from "vitest";

import { createUuidV7Factory } from "../src/index.js";

describe("createUuidV7Factory", () => {
  it("encodes the Unix timestamp and RFC version and variant bits", () => {
    const factory = createUuidV7Factory({
      now: () => 0x01890f9d0000,
      randomBytes: () => new Uint8Array(10),
    });

    expect(factory()).toBe("01890f9d-0000-7000-8000-000000000000");
  });

  it("produces daemon-compatible UUIDv7 command IDs", () => {
    const id = createUuidV7Factory()();
    expect(id).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
    );
  });

  it("rejects an invalid random source", () => {
    const factory = createUuidV7Factory({ randomBytes: () => new Uint8Array(9) });
    expect(factory).toThrow("exactly 10 bytes");
  });
});
