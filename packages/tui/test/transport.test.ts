import type { Stats } from "node:fs";
import { lstat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { beforeEach, describe, expect, it, vi } from "vitest";

import { defaultSocketPath, validateSocketPath } from "../src/transport.js";

vi.mock("node:fs/promises", () => ({ lstat: vi.fn() }));

const mockedLstat = vi.mocked(lstat);
const currentUid = process.geteuid?.() ?? process.getuid?.() ?? 501;
const unixDescribe = process.platform === "win32" ? describe.skip : describe;

beforeEach(() => {
  mockedLstat.mockReset();
});

describe("defaultSocketPath", () => {
  it("uses a private per-user fallback directory", () => {
    const uid = process.geteuid?.() ?? process.getuid?.() ?? "user";
    expect(defaultSocketPath({})).toBe(join(tmpdir(), `yeux-${uid}`, "yeuxd.sock"));
  });

  it("continues to honor configured runtime paths", () => {
    expect(defaultSocketPath({ YEUX_SOCKET: "/custom/runtime.sock" })).toBe(
      "/custom/runtime.sock",
    );
    expect(defaultSocketPath({ XDG_RUNTIME_DIR: "/run/user/1000" })).toBe(
      "/run/user/1000/yeux/yeuxd.sock",
    );
  });
});

unixDescribe("validateSocketPath", () => {
  it("accepts a private socket owned by the current uid", async () => {
    mockPrivatePath();

    await expect(validateSocketPath("/private/yeux/yeuxd.sock")).resolves
      .toBeUndefined();
    expect(mockedLstat).toHaveBeenNthCalledWith(1, "/private/yeux");
    expect(mockedLstat).toHaveBeenNthCalledWith(2, "/private/yeux/yeuxd.sock");
  });

  it("rejects a socket in a group-accessible parent", async () => {
    mockedLstat.mockResolvedValueOnce(directoryStats({ mode: 0o750 }));

    await expect(validateSocketPath("/private/yeux/yeuxd.sock")).rejects
      .toThrow(/Socket parent must not be accessible by group or other users/);
  });

  it("rejects a group-accessible socket", async () => {
    mockedLstat
      .mockResolvedValueOnce(directoryStats())
      .mockResolvedValueOnce(socketStats({ mode: 0o660 }));

    await expect(validateSocketPath("/private/yeux/yeuxd.sock")).rejects
      .toThrow(/Socket must not be accessible by group or other users/);
  });

  it("rejects a symlink in place of the socket", async () => {
    mockedLstat
      .mockResolvedValueOnce(directoryStats())
      .mockResolvedValueOnce(socketStats({ symbolicLink: true }));

    await expect(validateSocketPath("/private/yeux/yeuxd.sock")).rejects
      .toThrow(/Socket must not be a symlink/);
  });

  it("rejects a symlink in place of the socket parent", async () => {
    mockedLstat.mockResolvedValueOnce(directoryStats({ symbolicLink: true }));

    await expect(validateSocketPath("/private/yeux/yeuxd.sock")).rejects
      .toThrow(/Socket parent must not be a symlink/);
  });

  it("rejects a parent that is not owned by the current uid", async () => {
    mockedLstat.mockResolvedValueOnce(directoryStats({ uid: currentUid + 1 }));

    await expect(validateSocketPath("/private/yeux/yeuxd.sock")).rejects
      .toThrow(/Socket parent must be owned by uid/);
  });

  it("rejects a socket that is not owned by the current uid", async () => {
    mockedLstat
      .mockResolvedValueOnce(directoryStats())
      .mockResolvedValueOnce(socketStats({ uid: currentUid + 1 }));

    await expect(validateSocketPath("/private/yeux/yeuxd.sock")).rejects
      .toThrow(/Socket must be owned by uid/);
  });

  it("rejects a non-socket filesystem entry", async () => {
    mockedLstat
      .mockResolvedValueOnce(directoryStats())
      .mockResolvedValueOnce(fileStats());

    await expect(validateSocketPath("/private/yeux/yeuxd.sock")).rejects
      .toThrow(/Socket is not a Unix socket/);
  });
});

function mockPrivatePath(): void {
  mockedLstat
    .mockResolvedValueOnce(directoryStats())
    .mockResolvedValueOnce(socketStats());
}

function directoryStats(
  overrides: { readonly mode?: number; readonly uid?: number; readonly symbolicLink?: boolean } = {},
): Stats {
  return fakeStats({
    kind: "directory",
    mode: overrides.mode ?? 0o700,
    uid: overrides.uid ?? currentUid,
    symbolicLink: overrides.symbolicLink ?? false,
  });
}

function socketStats(
  overrides: { readonly mode?: number; readonly uid?: number; readonly symbolicLink?: boolean } = {},
): Stats {
  return fakeStats({
    kind: "socket",
    mode: overrides.mode ?? 0o600,
    uid: overrides.uid ?? currentUid,
    symbolicLink: overrides.symbolicLink ?? false,
  });
}

function fileStats(): Stats {
  return fakeStats({ kind: "file", mode: 0o600, uid: currentUid, symbolicLink: false });
}

function fakeStats(options: {
  readonly kind: "directory" | "socket" | "file";
  readonly mode: number;
  readonly uid: number;
  readonly symbolicLink: boolean;
}): Stats {
  return {
    dev: 1,
    ino: 2,
    mode: options.mode,
    uid: options.uid,
    isDirectory: () => options.kind === "directory",
    isSocket: () => options.kind === "socket",
    isSymbolicLink: () => options.symbolicLink,
  } as unknown as Stats;
}
