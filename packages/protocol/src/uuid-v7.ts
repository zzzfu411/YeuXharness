import { randomBytes as secureRandomBytes } from "node:crypto";

const MAX_UNIX_TIMESTAMP_MS = 0xffffffffffff;

export interface UuidV7FactoryOptions {
  readonly now?: () => number;
  readonly randomBytes?: (size: number) => Uint8Array;
}

export function createUuidV7Factory(options: UuidV7FactoryOptions = {}): () => string {
  const now = options.now ?? Date.now;
  const randomBytes = options.randomBytes ?? secureRandomBytes;

  return (): string => {
    const timestamp = Math.floor(now());
    if (
      !Number.isSafeInteger(timestamp) ||
      timestamp < 0 ||
      timestamp > MAX_UNIX_TIMESTAMP_MS
    ) {
      throw new RangeError("UUIDv7 timestamp is outside the 48-bit Unix millisecond range");
    }

    const random = randomBytes(10);
    if (random.byteLength !== 10) {
      throw new RangeError("UUIDv7 random source must return exactly 10 bytes");
    }

    const bytes = new Uint8Array(16);
    let remaining = BigInt(timestamp);
    for (let index = 5; index >= 0; index -= 1) {
      bytes[index] = Number(remaining & 0xffn);
      remaining >>= 8n;
    }
    bytes.set(random, 6);
    bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x70;
    bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;

    const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  };
}

export const uuidV7 = createUuidV7Factory();
