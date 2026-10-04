import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { Activity, api } from "./api";
import { useWorkspace } from "./workspace";
import { readStored, writeStored } from "./storage";

/**
 * One poll of `/api/activity`, shared by everything that needs it.
 *
 * The sidebar badge, the activity page, and the notifier all want the same
 * snapshot; three independent timers would triple the query load and — worse
 * — disagree with each other for seconds at a time, so the badge could say
 * "1 waiting" while the page it links to shows none.
 */
interface ActivityContext {
  activity: Activity | null;
  /** Fetch now, rather than waiting out the interval. Call after any action
   *  that changes the picture (answering a permission, pausing the queue). */
  refresh: () => void;
}

const Ctx = createContext<ActivityContext>({ activity: null, refresh: () => {} });

export function ActivityProvider({ children }: { children: React.ReactNode }) {
  const { active } = useWorkspace();
  const [activity, setActivity] = useState<Activity | null>(null);

  const refresh = useCallback(() => {
    if (!active) return;
    api.activity(active.id).then(setActivity).catch(() => {});
  }, [active]);

  useEffect(() => {
    setActivity(null); // switching workspace must not show the old one's runs
    refresh();
    const timer = setInterval(refresh, 4000);
    return () => clearInterval(timer);
  }, [refresh]);

  useNotifier(activity);

  const value = useMemo(() => ({ activity, refresh }), [activity, refresh]);
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useActivity(): ActivityContext {
  return useContext(Ctx);
}

const STORAGE_KEY = "eren:notify";

/** Whether the user has opted in *and* the browser still agrees. */
export function notificationsOn(): boolean {
  return (
    typeof Notification !== "undefined" &&
    Notification.permission === "granted" &&
    readStored(STORAGE_KEY) === "on"
  );
}

/** Must be called from a click — browsers reject a permission prompt that
 *  isn't tied to a user gesture. Returns the resulting on/off state. */
export async function toggleNotifications(on: boolean): Promise<boolean> {
  if (!on) {
    writeStored(STORAGE_KEY, "off");
    return false;
  }
  if (typeof Notification === "undefined") return false;
  const permission =
    Notification.permission === "granted"
      ? "granted"
      : await Notification.requestPermission();
  writeStored(STORAGE_KEY, permission === "granted" ? "on" : "off");
  return permission === "granted";
}

/**
 * Raise a browser notification when something new starts waiting on you.
 *
 * Runs here take tens of minutes, so the tab is usually in the background
 * when a run finally asks a question — which meant a run could sit blocked
 * for an hour because nobody was looking at the right tab.
 */
function useNotifier(activity: Activity | null) {
  // Everything already announced. Seeded on the first poll rather than left
  // empty, so opening the app doesn't fire a notification per already-known
  // blocker.
  const announced = useRef<Set<string> | null>(null);

  useEffect(() => {
    if (!activity) return;

    const keys = new Set<string>([
      ...activity.blocked.map((b) => `${b.kind}:${b.requestId ?? b.runId}`),
      ...activity.live
        .filter((r) => r.status === "rate_limited")
        .map((r) => `limit:${r.id}`),
      // Keyed by the policy and its window, not just the state, so the next
      // window — or another policy — announces again rather than staying silent.
      ...(activity.gate.state === "over_budget"
        ? [`budget:${activity.gate.policyId}:${activity.gate.resetsAt}`]
        : []),
    ]);

    if (announced.current === null) {
      announced.current = keys;
      return;
    }

    // Plans and permission prompts are announced by the inbox now
    // (`lib/inbox`), which knows every kind of waiting; announcing them here
    // too would knock twice.
    if (notificationsOn()) {
      for (const r of activity.live.filter((x) => x.status === "rate_limited")) {
        if (announced.current.has(`limit:${r.id}`)) continue;
        notify("Rate limited", `${r.label} is waiting for the limit to reset`, `limit:${r.id}`);
      }
      if (activity.gate.state === "over_budget") {
        const key = `budget:${activity.gate.policyId}:${activity.gate.resetsAt}`;
        if (!announced.current.has(key)) {
          notify(`Budget “${activity.gate.policy}” is spent`, activity.gate.message, key);
        }
      }
    }

    announced.current = keys;
  }, [activity]);
}

export function notify(title: string, body: string, tag: string) {
  try {
    // `tag` collapses repeats: the same prompt re-announced after a reload
    // replaces its own notification instead of stacking a second one.
    const n = new Notification(title, { body, tag });
    n.onclick = () => {
      window.focus();
      n.close();
    };
  } catch {
    /* Safari throws for constructed notifications in some contexts. */
  }
}
