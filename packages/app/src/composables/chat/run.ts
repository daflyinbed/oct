import { ref } from "vue";

export interface RunOwner {
  conversationId: string;
  generation: number;
  signal: AbortSignal;
}

/** Owns view subscriptions, never backend execution. Replacing an owner
 * aborts its fetch/reader and makes all late callbacks and finally blocks inert. */
export function createRunController() {
  const busy = ref(false);
  let selected: string | null = null;
  let generation = 0;
  let current: (RunOwner & { abort: AbortController }) | null = null;

  function invalidate() {
    const previous = current;
    current = null;
    ++generation;
    busy.value = false;
    previous?.abort.abort();
  }

  function select(conversationId: string | null) {
    if (selected === conversationId) return false;
    invalidate();
    selected = conversationId;
    return true;
  }

  function begin(conversationId: string): RunOwner | null {
    select(conversationId);
    if (current) return null;
    const abort = new AbortController();
    current = {
      conversationId,
      generation: ++generation,
      signal: abort.signal,
      abort,
    };
    busy.value = true;
    return current;
  }

  function owns(owner: RunOwner) {
    return (
      current === owner &&
      selected === owner.conversationId &&
      !owner.signal.aborted
    );
  }

  function finish(owner: RunOwner) {
    if (!owns(owner)) return false;
    current = null;
    busy.value = false;
    return true;
  }

  return { busy, begin, owns, finish, select, invalidate };
}
