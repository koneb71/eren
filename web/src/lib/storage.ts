/**
 * Guarded localStorage.
 *
 * A private window, blocked site data or a full quota makes every access
 * throw, and what the dashboard keeps there — a remembered project, a draft,
 * an open folder — is a per-viewer convenience, never worth a blank page. So
 * nothing reads or writes storage except through these: a read that cannot
 * happen is `null`, a write that cannot happen is `false`, and the caller
 * carries on.
 */

function store(): Storage | null {
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

export function readStored(key: string): string | null {
  try {
    return store()?.getItem(key) ?? null;
  } catch {
    return null;
  }
}

/** Whether the value was kept. */
export function writeStored(key: string, value: string): boolean {
  try {
    const s = store();
    if (!s) return false;
    s.setItem(key, value);
    return true;
  } catch {
    return false;
  }
}

export function removeStored(key: string): void {
  try {
    store()?.removeItem(key);
  } catch {
    // Nothing to forget, or nowhere to forget it from.
  }
}
