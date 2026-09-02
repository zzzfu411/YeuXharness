/**
 * The four Paper presenter blocks share one public import surface. They are
 * pure projections; protocol authority remains in the daemon and event ledger.
 */
export {
  formatAestheticModelEvent,
  formatInspector,
  formatInspectorBlock,
  formatSessionBar,
  formatTimelineEvent,
  renderInspector,
  renderSessionBar,
  renderTimelineEvent,
  type InspectorState,
  type PresenterFormatOptions,
  type SessionBarState,
} from "./renderer.js";
export {
  formatApprovalGate,
  isReadOnlyEffects,
  renderApprovalGate,
  type ApprovalGateFormatOptions,
} from "./prompter.js";
