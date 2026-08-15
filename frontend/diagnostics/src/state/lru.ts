export interface FixedLru<K, V> {
  readonly capacity: number;
  readonly entries: ReadonlyMap<K, V>;
  readonly order: readonly K[];
}

export interface LruWrite<K, V> {
  readonly state: FixedLru<K, V>;
  readonly evicted: readonly { readonly key: K; readonly value: V }[];
}

export interface LruRead<K, V> {
  readonly state: FixedLru<K, V>;
  readonly value: V | undefined;
}

export function createFixedLru<K, V>(capacity: number): FixedLru<K, V> {
  if (!Number.isSafeInteger(capacity) || capacity < 1) {
    throw new RangeError("LRU capacity must be a positive safe integer");
  }
  return { capacity, entries: new Map(), order: [] };
}

export function lruPeek<K, V>(state: FixedLru<K, V>, key: K): V | undefined {
  return state.entries.get(key);
}

export function lruGet<K, V>(state: FixedLru<K, V>, key: K): LruRead<K, V> {
  const value = state.entries.get(key);
  if (value === undefined) {
    return { state, value: undefined };
  }
  if (Object.is(state.order[state.order.length - 1], key)) {
    return { state, value };
  }
  return {
    state: {
      ...state,
      order: [...state.order.filter((candidate) => !Object.is(candidate, key)), key],
    },
    value,
  };
}

export function lruSet<K, V>(state: FixedLru<K, V>, key: K, value: V): LruWrite<K, V> {
  const entries = new Map(state.entries);
  entries.set(key, value);
  const order = [...state.order.filter((candidate) => !Object.is(candidate, key)), key];
  const evicted: { key: K; value: V }[] = [];
  while (order.length > state.capacity) {
    const oldest = order.shift();
    if (oldest === undefined) {
      break;
    }
    const oldestValue = entries.get(oldest);
    entries.delete(oldest);
    if (oldestValue !== undefined) {
      evicted.push({ key: oldest, value: oldestValue });
    }
  }
  return {
    state: { capacity: state.capacity, entries, order },
    evicted,
  };
}

export function lruDelete<K, V>(state: FixedLru<K, V>, key: K): FixedLru<K, V> {
  if (!state.entries.has(key)) {
    return state;
  }
  const entries = new Map(state.entries);
  entries.delete(key);
  return {
    ...state,
    entries,
    order: state.order.filter((candidate) => !Object.is(candidate, key)),
  };
}
