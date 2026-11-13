import { useEffect, useState } from '../lib.js';

/** The state of `store`; the component draws again on each change. */
export function useStore(store) {
  const [state, setState] = useState(store.state);
  useEffect(() => {
    setState(store.state);
    return store.subscribe(setState);
  }, [store]);
  return state;
}

/** The current time, updated every `ms` (for ages and countdowns). */
export function useNow(ms = 1000) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(id);
  }, [ms]);
  return now;
}
