import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { readFile } from "node:fs/promises";
import { dirname, extname, isAbsolute, normalize, resolve, sep } from "node:path";

import { isRecord } from "@yeux/protocol";
import { parse as parseToml } from "smol-toml";

const PLUGIN_ID = /^[a-z0-9](?:[a-z0-9.-]{0,126}[a-z0-9])?$/;
const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
const CAPABILITY = /^[a-z][a-z0-9_-]*(?::[a-zA-Z0-9*._/-]+)*$/;
const SHA256 = /^(?:sha256:)?([a-fA-F0-9]{64})$/;

export interface PluginManifest {
  readonly id: string;
  readonly version: string;
  readonly protocol: string;
  readonly executable: string;
  readonly args: readonly string[];
  readonly sha256: string;
  readonly publisher?: string;
  readonly requested_capabilities: readonly string[];
}

export interface LoadedPluginManifest {
  readonly manifest: PluginManifest;
  readonly root: string;
  readonly path: string;
}

export async function loadPluginManifest(path: string): Promise<LoadedPluginManifest> {
  const absolutePath = resolve(path);
  const source = await readFile(absolutePath, "utf8");
  const raw = extname(absolutePath).toLowerCase() === ".json"
    ? (JSON.parse(source) as unknown)
    : (parseToml(source) as unknown);
  return {
    manifest: validatePluginManifest(raw),
    root: dirname(absolutePath),
    path: absolutePath,
  };
}

export function validatePluginManifest(value: unknown): PluginManifest {
  if (!isRecord(value)) throw new Error("Plugin manifest must be an object");
  if (typeof value.id !== "string" || !PLUGIN_ID.test(value.id)) {
    throw new Error("Plugin manifest id is invalid");
  }
  if (typeof value.version !== "string" || !SEMVER.test(value.version)) {
    throw new Error("Plugin manifest version must be semantic versioning");
  }
  const protocol = typeof value.protocol === "number" ? String(value.protocol) : value.protocol;
  if (typeof protocol !== "string" || !/^1(?:\.\d+)?$/.test(protocol)) {
    throw new Error("Plugin protocol must be compatible with major version 1");
  }
  if (typeof value.executable !== "string" || !isSafeRelativePath(value.executable)) {
    throw new Error("Plugin executable must be a safe path relative to the manifest");
  }
  const hashMatch = typeof value.sha256 === "string" ? SHA256.exec(value.sha256) : null;
  if (hashMatch?.[1] === undefined) throw new Error("Plugin manifest requires a SHA-256 digest");
  if (value.publisher !== undefined && typeof value.publisher !== "string") {
    throw new Error("Plugin publisher must be a string");
  }
  const args = validateStringArray(value.args, "args", () => true);
  const requestedCapabilities = validateStringArray(
    value.requested_capabilities,
    "requested_capabilities",
    (item) => CAPABILITY.test(item),
  );
  if (new Set(requestedCapabilities).size !== requestedCapabilities.length) {
    throw new Error("Plugin requested_capabilities contains duplicates");
  }

  return Object.freeze({
    id: value.id,
    version: value.version,
    protocol,
    executable: normalize(value.executable),
    args: Object.freeze(args),
    sha256: hashMatch[1].toLowerCase(),
    ...(value.publisher === undefined ? {} : { publisher: value.publisher }),
    requested_capabilities: Object.freeze(requestedCapabilities),
  });
}

export function resolveExecutable(root: string, manifest: PluginManifest): string {
  const absoluteRoot = resolve(root);
  const executable = resolve(absoluteRoot, manifest.executable);
  if (executable !== absoluteRoot && !executable.startsWith(`${absoluteRoot}${sep}`)) {
    throw new Error("Plugin executable escapes the plugin root");
  }
  return executable;
}

export async function verifyPluginExecutable(
  root: string,
  manifest: PluginManifest,
): Promise<string> {
  const executable = resolveExecutable(root, manifest);
  const digest = await hashFile(executable);
  if (digest !== manifest.sha256) {
    throw new Error(`Plugin executable digest mismatch for ${manifest.id}`);
  }
  return executable;
}

export function resolveGrantedCapabilities(
  manifest: PluginManifest,
  grants: readonly string[] = [],
): readonly string[] {
  const unique = new Set(grants);
  if (unique.size !== grants.length) throw new Error("Granted capabilities contain duplicates");
  for (const capability of unique) {
    if (!manifest.requested_capabilities.includes(capability)) {
      throw new Error(`Capability ${capability} was not requested by ${manifest.id}`);
    }
  }
  return Object.freeze([...unique].sort());
}

function isSafeRelativePath(path: string): boolean {
  if (path.length === 0 || path.includes("\0") || isAbsolute(path)) return false;
  const normalized = normalize(path);
  return normalized !== ".." && !normalized.startsWith(`..${sep}`);
}

function validateStringArray(
  value: unknown,
  field: string,
  validate: (item: string) => boolean,
): string[] {
  if (value === undefined) return [];
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string" || !validate(item))) {
    throw new Error(`Plugin manifest ${field} must be an array of valid strings`);
  }
  return [...value] as string[];
}

async function hashFile(path: string): Promise<string> {
  const hash = createHash("sha256");
  await new Promise<void>((resolvePromise, reject) => {
    const stream = createReadStream(path);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.once("end", resolvePromise);
    stream.once("error", reject);
  });
  return hash.digest("hex");
}
