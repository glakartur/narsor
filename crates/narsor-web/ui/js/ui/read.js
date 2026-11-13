// Reads of the details of one task: started again when a key changes (the
// task, its revision), cancelled when the view goes away. A runner that does
// not answer (503) or a reader that is starting (409) is a note in the tab,
// not an error of the connection.

import { useEffect, useRef, useState } from '../lib.js';

export const RUNNER_SILENT = 'runner nie odpowiada — spróbuję ponownie przy następnej zmianie';
const READER_RETRY_MS = 1500;

/** A text for a failed detail read. */
export function readError(error) {
  if (error?.kind === 'degraded') return RUNNER_SILENT;
  if (error?.kind === 'unavailable') return 'brak połączenia z serwerem';
  if (error?.code === 'conversation_unavailable') return 'rozmowa niedostępna teraz';
  return error?.message || String(error);
}

/**
 * `load(signal, previous)` runs when `key` changes. Returns
 * `{data, error, loading}`; the last data stays while a new read runs.
 */
export function useRead(load, key) {
  const [result, setResult] = useState({ data: null, error: null, loading: true });
  const latest = useRef(load);
  latest.current = load;
  const dataRef = useRef(null);
  useEffect(() => {
    const controller = new AbortController();
    let timer = null;
    const run = async () => {
      setResult((old) => ({ ...old, loading: true }));
      try {
        const data = await latest.current(controller.signal, dataRef.current);
        if (controller.signal.aborted) return;
        dataRef.current = data;
        setResult({ data, error: null, loading: false });
      } catch (error) {
        if (controller.signal.aborted) return;
        if (error?.code === 'conversation_reader_starting') {
          timer = setTimeout(run, READER_RETRY_MS);
          return;
        }
        setResult((old) => ({ data: old.data, error: readError(error), loading: false }));
      }
    };
    run();
    return () => {
      controller.abort();
      clearTimeout(timer);
    };
  }, [key]);
  return result;
}

/** Resets cached data when the task changes (the key of `useRead` includes the task id). */
export function useTaskKey(task, extra = '') {
  return `${task.task_id}:${task.revision}:${extra}`;
}
