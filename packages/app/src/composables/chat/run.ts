import { computed, shallowRef } from "vue";

export interface ViewLease {
  conversationId: string;
  viewGeneration: number;
}

export interface RunOwner extends ViewLease {
  generation: number;
  signal: AbortSignal;
}

type Activity =
  | { kind: "idle" }
  | { kind: "active"; owner: RunOwner; abort: AbortController }
  | { kind: "history-change" }
  | { kind: "disposed" };

/** Owns view subscriptions, never backend execution. Replacing an owner
 * aborts its fetch/reader and makes all late callbacks and finally blocks inert. */
export function createRunController() {
  const activity = shallowRef<Activity>({ kind: "idle" });
  const busy = computed(() => activity.value.kind === "active");
  let selected: string | null = null;
  let viewGeneration = 0;
  let generation = 0;

  function invalidate() {
    const previous = activity.value;
    if (previous.kind === "active") activity.value = { kind: "idle" };
    ++viewGeneration;
    if (previous.kind === "active") previous.abort.abort();
  }

  function select(conversationId: string | null) {
    if (activity.value.kind === "disposed" || selected === conversationId)
      return false;
    invalidate();
    selected = conversationId;
    return true;
  }

  function captureView(): ViewLease | null {
    return selected == null || activity.value.kind === "disposed"
      ? null
      : { conversationId: selected, viewGeneration };
  }

  function ownsView(lease: ViewLease) {
    return (
      activity.value.kind !== "disposed" &&
      selected === lease.conversationId &&
      viewGeneration === lease.viewGeneration
    );
  }

  function begin(conversationId: string): RunOwner | null {
    if (activity.value.kind !== "idle") return null;
    select(conversationId);
    const abort = new AbortController();
    const owner = {
      conversationId,
      viewGeneration,
      generation: ++generation,
      signal: abort.signal,
    };
    activity.value = { kind: "active", owner, abort };
    return owner;
  }

  function owns(owner: RunOwner) {
    return (
      activity.value.kind === "active" &&
      activity.value.owner === owner &&
      ownsView(owner) &&
      !owner.signal.aborted
    );
  }

  function finish(owner: RunOwner) {
    if (!owns(owner)) return false;
    activity.value = { kind: "idle" };
    return true;
  }

  function beginHistoryChange() {
    if (activity.value.kind !== "idle") return false;
    activity.value = { kind: "history-change" };
    return true;
  }

  function endHistoryChange() {
    if (activity.value.kind === "history-change")
      activity.value = { kind: "idle" };
  }

  function dispose() {
    invalidate();
    activity.value = { kind: "disposed" };
  }

  return {
    busy,
    begin,
    owns,
    finish,
    select,
    invalidate,
    captureView,
    ownsView,
    beginHistoryChange,
    endHistoryChange,
    dispose,
    get conversationId() {
      return selected;
    },
    get disposed() {
      return activity.value.kind === "disposed";
    },
    get historyChanging() {
      return activity.value.kind === "history-change";
    },
  };
}

export type RunController = ReturnType<typeof createRunController>;
