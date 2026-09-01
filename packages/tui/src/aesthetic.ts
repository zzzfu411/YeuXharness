/**
 * YeuX terminal presentation assets.
 *
 * This module is deliberately renderer-only: it owns no protocol or policy
 * decisions.  It centralises the visual vocabulary so that the line renderer,
 * approval prompt and a future OpenTUI client can share the same semantics.
 */

export type ColorDepth = "truecolor" | "ansi256" | "ansi16" | "none";

export type ThemeName = "nocturne" | "paper" | "mono" | "high-contrast";

export interface TerminalCapabilities {
  /** Whether stdout is an interactive terminal. */
  readonly isTTY: boolean;
  /** Number of columns available to the human-readable renderer. */
  readonly columns: number;
  /** The strongest colour mode safe to emit. */
  readonly colorDepth: ColorDepth;
  /** Whether box-drawing and other Unicode assets are safe to use. */
  readonly unicode: boolean;
  /** Append-only/plain mode; alternate-screen decoration must be disabled. */
  readonly plain: boolean;
  /** Whether continuous decorative motion should be disabled. */
  readonly reducedMotion: boolean;
}

export interface DetectTerminalCapabilitiesOptions {
  readonly env?: Readonly<Record<string, string | undefined>>;
  readonly isTTY?: boolean;
  readonly columns?: number;
  readonly locale?: string;
  readonly color?: boolean | ColorDepth;
  readonly ascii?: boolean;
  readonly plain?: boolean;
  readonly reducedMotion?: boolean;
}

export const DEFAULT_THEME: ThemeName = "nocturne";

/**
 * Stable, copyable glyphs.  They are intentionally kept to characters that
 * are normally one terminal column wide.  `brandCompact` is the one
 * intentional two-character mark.
 */
export const UNICODE_GLYPHS = Object.freeze({
  brandCompact: "><",
  workspace: "⌂",
  connected: "↔",
  provider: "◇",
  replay: "⟲",
  observe: "○",
  rail: "│",
  branch: "├─",
  end: "└─",
  continuation: "│ ",
  approvalStart: "┏",
  approvalRail: "┃",
  approvalEnd: "┗",
  lightDivider: "┄",
  strongDivider: "━",
  beat: "·",
  gap: "╎",
  accepted: "·",
  context: "◌",
  model: "↗",
  streaming: "≈",
  toolProposed: "◇",
  approval: "?",
  authorized: "◆",
  executing: "▶",
  integrating: "∿",
  completed: "✓",
  failed: "×",
  cancelled: "—",
  unknown: "!",
  paused: "Ⅱ",
  expired: "⌛",
  prompt: "›",
  causedBy: "↳",
  checkpoint: "▣",
  projectionMatch: "≡",
  projectionDrift: "≠",
  backpressure: "⋮",
  reconciliation: "↻",
  effectRead: "○",
  effectWrite: "✎",
  effectDelete: "⌫",
  effectProcess: "▶",
  effectNetwork: "↗",
  effectSecret: "#",
  effectExternalWrite: "⇥",
  sandbox: "□",
  allow: "✓",
  deny: "×",
} as const);

export const ASCII_GLYPHS = Object.freeze({
  brandCompact: "><",
  workspace: "[local]",
  connected: "<->",
  provider: "o",
  replay: "[R]",
  observe: "[RO]",
  rail: "|",
  branch: "+-",
  end: "`-",
  continuation: "| ",
  approvalStart: "+-",
  approvalRail: "|",
  approvalEnd: "`-",
  lightDivider: "- -",
  strongDivider: "=",
  beat: ".",
  gap: ":",
  accepted: ".",
  context: "o",
  model: "->",
  streaming: "~",
  toolProposed: "o",
  approval: "?",
  authorized: "*",
  executing: ">",
  integrating: "~",
  completed: "OK",
  failed: "ERR",
  cancelled: "--",
  unknown: "!!",
  paused: "||",
  expired: "EXP",
  prompt: ">",
  causedBy: "->",
  checkpoint: "[C]",
  projectionMatch: "==",
  projectionDrift: "!=",
  backpressure: "...",
  reconciliation: "[reconcile]",
  effectRead: "[r]",
  effectWrite: "[w]",
  effectDelete: "[del]",
  effectProcess: "[proc]",
  effectNetwork: "[net]",
  effectSecret: "[secret]",
  effectExternalWrite: "[ext]",
  sandbox: "[sbx]",
  allow: "[allow]",
  deny: "[deny]",
} as const);

export type GlyphName = keyof typeof UNICODE_GLYPHS;

export const STATUS_LABELS = Object.freeze({
  accepted: "ACCEPTED",
  building_context: "CONTEXT",
  requesting_model: "MODEL REQUESTED",
  streaming: "STREAMING",
  proposed_tools: "TOOL PROPOSED",
  waiting_for_approval: "WAITING FOR APPROVAL",
  authorizing: "AUTHORIZING",
  scheduling: "SCHEDULING",
  executing: "EXECUTING",
  integrating_results: "INTEGRATING",
  waiting_for_input: "WAITING FOR INPUT",
  cancelling: "CANCELLING",
  completed: "COMPLETED",
  cancelled: "CANCELLED",
  failed: "FAILED",
  unknown: "UNKNOWN · RECONCILIATION REQUIRED",
} as const);

export type StatusName = keyof typeof STATUS_LABELS;

export interface ThemePalette {
  readonly background: string;
  readonly surface: string;
  readonly raised: string;
  readonly line: string;
  readonly text: string;
  readonly textSoft: string;
  readonly muted: string;
  readonly focus: string;
  readonly focusDeep: string;
  readonly approval: string;
  readonly success: string;
  readonly warning: string;
  readonly danger: string;
}

export const NOCTURNE_PALETTE: ThemePalette = Object.freeze({
  background: "#080909",
  surface: "#101214",
  raised: "#171B1E",
  line: "#3B4145",
  text: "#E2DED5",
  textSoft: "#C0BBB1",
  muted: "#99958D",
  focus: "#6C9AB3",
  focusDeep: "#173A52",
  approval: "#D17968",
  success: "#79A988",
  warning: "#D0AB61",
  danger: "#D17968",
});

export const PAPER_PALETTE: ThemePalette = Object.freeze({
  background: "#D8D3CC",
  surface: "#E7E3DB",
  raised: "#C6C0B6",
  line: "#B4ADA1",
  text: "#1B1815",
  textSoft: "#423C36",
  muted: "#625B54",
  focus: "#31566B",
  focusDeep: "#31566B",
  approval: "#8C3A2C",
  success: "#355F43",
  warning: "#755719",
  danger: "#8C3A2C",
});

export function paletteForTheme(theme: ThemeName = DEFAULT_THEME): ThemePalette {
  return theme === "paper" ? PAPER_PALETTE : NOCTURNE_PALETTE;
}

/**
 * Detect terminal affordances without writing to the terminal.  `NO_COLOR`
 * follows its convention: the mere presence of the variable disables SGR,
 * including when it is set to an empty string.
 */
export function detectTerminalCapabilities(
  options: DetectTerminalCapabilitiesOptions = {},
): TerminalCapabilities {
  const env = options.env ?? process.env;
  const isTTY = options.isTTY ?? process.stdout.isTTY === true;
  const columns = normalizeColumns(options.columns ?? process.stdout.columns);
  const term = env.TERM?.toLowerCase() ?? "";
  const noColor = Object.prototype.hasOwnProperty.call(env, "NO_COLOR");
  const ci = env.CI === "1" || env.CI?.toLowerCase() === "true";
  const termDumb = term === "dumb";
  const locale = options.locale ?? env.LC_ALL ?? env.LC_CTYPE ?? env.LANG ?? "";
  const localeLooksUtf8 = locale.length === 0 || /utf-?8/i.test(locale);
  const ascii =
    options.ascii === true ||
    env.YEUX_ASCII === "1" ||
    !localeLooksUtf8 ||
    termDumb;

  // These are capability ceilings, not preferences: a caller cannot force an
  // alternate-screen presentation into a pipe/CI/dumb terminal by passing
  // `plain: false`.
  const plain = options.plain === true || !isTTY || termDumb || ci;
  const reducedMotion =
    options.reducedMotion === true ||
    env.YEUX_REDUCED_MOTION === "1" ||
    noColor ||
    plain;

  let colorDepth: ColorDepth;
  if (noColor || !isTTY || termDumb || ci || options.color === false) {
    colorDepth = "none";
  } else if (typeof options.color === "string") {
    colorDepth = options.color;
  } else if (options.color === true) {
    colorDepth = inferColorDepth(term, env.COLORTERM);
  } else {
    colorDepth = inferColorDepth(term, env.COLORTERM);
  }

  return Object.freeze({
    isTTY,
    columns,
    colorDepth,
    unicode: !ascii,
    plain,
    reducedMotion,
  });
}

function inferColorDepth(term: string, colorTerm: string | undefined): ColorDepth {
  if (/truecolor|24bit|direct/i.test(colorTerm ?? "") || /-direct$/.test(term)) {
    return "truecolor";
  }
  if (/256color/i.test(term)) return "ansi256";
  return "ansi16";
}

function normalizeColumns(columns: number | undefined): number {
  if (!Number.isFinite(columns)) return 80;
  return Math.max(20, Math.floor(columns as number));
}

export function glyph(name: GlyphName, capabilities: Pick<TerminalCapabilities, "unicode">): string {
  return (capabilities.unicode ? UNICODE_GLYPHS : ASCII_GLYPHS)[name];
}

export type ColorRole =
  | "muted"
  | "text"
  | "focus"
  | "approval"
  | "success"
  | "warning"
  | "danger";

export interface AnsiRoleToken {
  readonly ansi16: string;
  readonly ansi256: number;
  readonly rgb: readonly [number, number, number];
}

export const ANSI_RESET = "\u001b[0m";

/**
 * Nocturne is the canonical terminal palette. Keeping its SGR values in one
 * exported table prevents renderers, prompts and future OpenTUI widgets from
 * inventing subtly different safety colours.
 */
export const NOCTURNE_ANSI_TOKENS: Readonly<Record<ColorRole, AnsiRoleToken>> =
  Object.freeze({
    muted: { ansi16: "2", ansi256: 245, rgb: [153, 149, 141] },
    text: { ansi16: "0", ansi256: 253, rgb: [226, 222, 213] },
    focus: { ansi16: "36", ansi256: 74, rgb: [108, 154, 179] },
    approval: { ansi16: "31", ansi256: 174, rgb: [209, 121, 104] },
    success: { ansi16: "32", ansi256: 108, rgb: [121, 169, 136] },
    warning: { ansi16: "33", ansi256: 179, rgb: [208, 171, 97] },
    danger: { ansi16: "31", ansi256: 174, rgb: [209, 121, 104] },
  });

const PAPER_ANSI_256_CODES: Readonly<Record<ColorRole, number>> = Object.freeze({
  muted: 59,
  text: 234,
  focus: 24,
  approval: 88,
  success: 22,
  warning: 94,
  danger: 88,
});

/** Paint only renderer-owned text; callers must sanitize untrusted text first. */
export function paint(
  text: string,
  role: ColorRole,
  capabilities: Pick<TerminalCapabilities, "colorDepth">,
  theme: ThemeName = DEFAULT_THEME,
): string {
  const prefix = ansiPrefix(role, capabilities.colorDepth, theme);
  return prefix.length === 0 ? text : `${prefix}${text}${ANSI_RESET}`;
}

function ansiPrefix(role: ColorRole, depth: ColorDepth, theme: ThemeName): string {
  if (theme === "mono") return "";

  switch (depth) {
    case "truecolor": {
      const [r, g, b] = rgbForRole(role, theme);
      return `\u001b[38;2;${r};${g};${b}m`;
    }
    case "ansi256":
      return `\u001b[38;5;${ansi256ForRole(role, theme)}m`;
    case "ansi16":
      return `\u001b[${NOCTURNE_ANSI_TOKENS[role].ansi16}m`;
    case "none":
      return "";
  }
}

function rgbForRole(role: ColorRole, theme: ThemeName): readonly [number, number, number] {
  if (theme !== "paper") return NOCTURNE_ANSI_TOKENS[role].rgb;

  const palette = PAPER_PALETTE;
  switch (role) {
    case "muted":
      return hexToRgb(palette.muted);
    case "text":
      return hexToRgb(palette.text);
    case "focus":
      return hexToRgb(palette.focus);
    case "approval":
      return hexToRgb(palette.approval);
    case "success":
      return hexToRgb(palette.success);
    case "warning":
      return hexToRgb(palette.warning);
    case "danger":
      return hexToRgb(palette.danger);
  }
}

function ansi256ForRole(role: ColorRole, theme: ThemeName): number {
  return theme === "paper"
    ? PAPER_ANSI_256_CODES[role]
    : NOCTURNE_ANSI_TOKENS[role].ansi256;
}

function hexToRgb(hex: string): readonly [number, number, number] {
  const value = hex.replace(/^#/, "");
  if (!/^[0-9a-f]{6}$/i.test(value)) return [255, 255, 255];
  return [
    Number.parseInt(value.slice(0, 2), 16),
    Number.parseInt(value.slice(2, 4), 16),
    Number.parseInt(value.slice(4, 6), 16),
  ];
}

export function statusGlyphName(state: string): GlyphName {
  switch (state) {
    case "accepted":
      return "accepted";
    case "building_context":
      return "context";
    case "requesting_model":
      return "model";
    case "streaming":
      return "streaming";
    case "proposed_tools":
      return "toolProposed";
    case "waiting_for_approval":
      return "approval";
    case "authorizing":
      return "authorized";
    case "scheduling":
      return "accepted";
    case "executing":
      return "executing";
    case "integrating_results":
      return "integrating";
    case "waiting_for_input":
      return "approval";
    case "completed":
      return "completed";
    case "cancelled":
    case "cancelling":
      return "cancelled";
    case "failed":
      return "failed";
    default:
      return "unknown";
  }
}

export function statusLabel(state: string): string {
  return STATUS_LABELS[state as StatusName] ?? state.replaceAll("_", " ").toUpperCase();
}

export function colorRoleForStatus(state: string): ColorRole {
  switch (state) {
    case "completed":
      return "success";
    case "waiting_for_approval":
      return "approval";
    case "failed":
    case "unknown":
      return "danger";
    case "cancelled":
      return "muted";
    case "accepted":
    case "building_context":
    case "requesting_model":
    case "streaming":
    case "proposed_tools":
    case "authorizing":
    case "scheduling":
    case "executing":
    case "integrating_results":
    case "waiting_for_input":
    case "cancelling":
      return "focus";
    default:
      return "muted";
  }
}

export function sequenceLabel(seq: number): string {
  return Number.isFinite(seq) ? Math.max(0, Math.floor(seq)).toString().padStart(4, "0") : "????";
}
