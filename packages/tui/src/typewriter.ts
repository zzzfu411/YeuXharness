import type { TerminalCapabilities } from "./aesthetic.js";

/** Per-character delay for Latin and other non-CJK live model ink. */
export const TYPEWRITER_LATIN_DELAY_MS = 18;

/** Per-character delay for Han, Hiragana, Katakana, and Hangul ink. */
export const TYPEWRITER_CJK_DELAY_MS = 24;

/** One model segment walks for at most this long; leftover ink drops instantly. */
export const TYPEWRITER_SEGMENT_CAP_MS = 600;

/** Operator keystrokes are never buffered or delayed. */
export const HUMAN_KEYBOARD_DELAY_MS = 0;

/** Static graphite insertion bar. Never a blinking block. */
export const TYPEWRITER_CARET = "│";

/** ASCII fallback for the graphite caret. */
export const TYPEWRITER_CARET_ASCII = "|";

const CJK_CHAR =
  /^(?:\p{Script=Han}|\p{Script=Hiragana}|\p{Script=Katakana}|\p{Script=Hangul})$/u;

export interface TypewriterWriteOptions {
  readonly maxLagMs?: number;
  readonly caret?: string;
  readonly paintInk?: (chunk: string) => string;
  readonly catchUp?: () => boolean;
  readonly now?: () => number;
  readonly sleep?: (ms: number) => Promise<void>;
}

/**
 * Live model ink only. JSONL, pipes, plain, NO_COLOR and reduced-motion
 * must dump the finished line; this predicate is the single gate.
 */
export function modelInkMotionAllowed(
  capabilities: TerminalCapabilities,
  options: { readonly jsonl?: boolean; readonly typewriter?: boolean } = {},
): boolean {
  if (options.jsonl === true || options.typewriter === false) return false;
  return (
    capabilities.isTTY &&
    !capabilities.plain &&
    !capabilities.reducedMotion &&
    capabilities.colorDepth !== "none"
  );
}

export function isCjkCharacter(char: string): boolean {
  return CJK_CHAR.test(char);
}

export function typewriterDelayMs(char: string): number {
  return isCjkCharacter(char) ? TYPEWRITER_CJK_DELAY_MS : TYPEWRITER_LATIN_DELAY_MS;
}

export function typewriterCaret(unicode: boolean): string {
  return unicode ? TYPEWRITER_CARET : TYPEWRITER_CARET_ASCII;
}

function defaultSleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

/** Write `text` a character at a time; catch up when the source outruns the key. */
export async function writeTypewriterInk(
  text: string,
  write: (chunk: string) => void,
  options: TypewriterWriteOptions = {},
): Promise<void> {
  const maxLagMs = options.maxLagMs ?? TYPEWRITER_SEGMENT_CAP_MS;
  const now = options.now ?? Date.now;
  const sleep = options.sleep ?? defaultSleep;
  const caret = options.caret ?? "";
  const paintInk = options.paintInk ?? ((chunk: string) => chunk);
  const chars = [...text];
  if (chars.length === 0) return;

  const writeCaret = (): void => {
    if (caret.length > 0) write(caret);
  };
  const clearCaret = (): void => {
    if (caret.length > 0) write("\b");
  };

  const started = now();
  let scheduled = 0;
  writeCaret();
  for (let index = 0; index < chars.length; index += 1) {
    const char = chars[index] as string;
    clearCaret();
    write(paintInk(char));
    if (index === chars.length - 1) break;

    const delay = typewriterDelayMs(char);
    if (options.catchUp?.() === true || scheduled + delay > maxLagMs) {
      write(paintInk(chars.slice(index + 1).join("")));
      break;
    }

    writeCaret();
    const wait = started + scheduled + delay - now();
    if (wait > 0) await sleep(wait);
    scheduled += delay;
  }
}
