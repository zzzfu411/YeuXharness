const ESCAPE = 0x1b;
const BELL = 0x07;
const TAB = 0x09;
const LINE_FEED = 0x0a;
const CARRIAGE_RETURN = 0x0d;
const DELETE = 0x7f;
const DEVICE_CONTROL_STRING = 0x90;
const START_OF_STRING = 0x98;
const CONTROL_SEQUENCE_INTRODUCER = 0x9b;
const STRING_TERMINATOR = 0x9c;
const OPERATING_SYSTEM_COMMAND = 0x9d;
const PRIVACY_MESSAGE = 0x9e;
const APPLICATION_PROGRAM_COMMAND = 0x9f;

/**
 * Removes data that a terminal could interpret as commands rather than text.
 *
 * Newlines and tabs are retained for readable model output. A lone carriage
 * return becomes a newline so untrusted text cannot overwrite the current
 * line. ANSI/ECMA-48 escape sequences, C0/C1 controls, and bidirectional text
 * controls are removed. Call this only at human-terminal sinks: JSONL output
 * must retain the original protocol payload.
 */
export function sanitizeTerminalText(text: string): string {
  let sanitized = "";

  for (let index = 0; index < text.length; index += 1) {
    const code = text.charCodeAt(index);

    if (code === ESCAPE) {
      index = consumeEscapeSequence(text, index);
      continue;
    }
    if (code === CONTROL_SEQUENCE_INTRODUCER) {
      index = consumeControlSequence(text, index + 1);
      continue;
    }
    if (
      code === DEVICE_CONTROL_STRING ||
      code === START_OF_STRING ||
      code === OPERATING_SYSTEM_COMMAND ||
      code === PRIVACY_MESSAGE ||
      code === APPLICATION_PROGRAM_COMMAND
    ) {
      index = consumeControlString(text, index + 1);
      continue;
    }
    if (code === CARRIAGE_RETURN) {
      if (text.charCodeAt(index + 1) !== LINE_FEED) sanitized += "\n";
      continue;
    }
    if (code === TAB || code === LINE_FEED) {
      sanitized += text[index];
      continue;
    }
    if (code < 0x20 || (code >= DELETE && code <= APPLICATION_PROGRAM_COMMAND)) {
      continue;
    }
    if (isBidirectionalControl(code)) continue;

    sanitized += text[index];
  }

  return sanitized;
}

function consumeEscapeSequence(text: string, escapeIndex: number): number {
  const introducer = text.charCodeAt(escapeIndex + 1);
  if (Number.isNaN(introducer)) return escapeIndex;

  if (introducer === 0x5b) return consumeControlSequence(text, escapeIndex + 2); // CSI
  if (
    introducer === 0x50 || // DCS
    introducer === 0x58 || // SOS
    introducer === 0x5d || // OSC
    introducer === 0x5e || // PM
    introducer === 0x5f // APC
  ) {
    return consumeControlString(text, escapeIndex + 2);
  }

  let index = escapeIndex + 1;
  while (index < text.length && isEscapeIntermediate(text.charCodeAt(index))) index += 1;
  if (index < text.length && isEscapeFinal(text.charCodeAt(index))) return index;

  // The ESC byte is sufficient to activate a sequence. Dropping it makes any
  // malformed or chunk-split remainder display as inert text.
  return escapeIndex;
}

function consumeControlSequence(text: string, startIndex: number): number {
  for (let index = startIndex; index < text.length; index += 1) {
    const code = text.charCodeAt(index);
    if (isControlSequenceFinal(code)) return index;
    if (code === ESCAPE) return index - 1;
  }
  return text.length - 1;
}

function consumeControlString(text: string, startIndex: number): number {
  for (let index = startIndex; index < text.length; index += 1) {
    const code = text.charCodeAt(index);
    if (code === BELL || code === STRING_TERMINATOR) return index;
    if (code === ESCAPE && text.charCodeAt(index + 1) === 0x5c) return index + 1;
  }
  return text.length - 1;
}

function isEscapeIntermediate(code: number): boolean {
  return code >= 0x20 && code <= 0x2f;
}

function isEscapeFinal(code: number): boolean {
  return code >= 0x30 && code <= 0x7e;
}

function isControlSequenceFinal(code: number): boolean {
  return code >= 0x40 && code <= 0x7e;
}

function isBidirectionalControl(code: number): boolean {
  return (
    code === 0x061c ||
    code === 0x200e ||
    code === 0x200f ||
    (code >= 0x202a && code <= 0x202e) ||
    (code >= 0x2066 && code <= 0x2069)
  );
}
