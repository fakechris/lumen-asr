import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { listen } from "@tauri-apps/api/event";
import { api, type AsrModelStatus } from "./api";

// App-level ASR model download coordinator.
//
// The backend download commands are single-flight (one download at a time,
// one global cancel flag), so the *UI* must funnel every model download
// through one place. This provider — mounted at the App root so it never
// unmounts on tab switches — is that place: onboarding's automatic background
// download, the onboarding model step, and the meeting model card all enqueue
// here. Targets download strictly one after another (a FIFO queue: the next
// starts only when the previous invoke settles), one `asr-download-progress`
// listener drives progress for whichever download is in flight, and cancel /
// failure state survives navigation.

export type ModelTarget = "sensevoice" | "qwen" | "streaming" | "offline";

export type ModelProgressState = {
  phase: string;
  message: string;
  percent: number | null;
};

export type MeetingModels = {
  status: AsrModelStatus | null;
  /** Target currently downloading (null when idle). */
  active: ModelTarget | null;
  /** Targets waiting behind `active`, in download order. */
  queued: ModelTarget[];
  progress: ModelProgressState | null;
  error: string | null;
  /** Targets dropped by a failure or cancel; `retry()` re-enqueues them. */
  failed: ModelTarget[];
  /** True after an explicit user cancel — the queue never restarts on its
   * own; only an explicit enqueue/retry starts downloads again. */
  cancelled: boolean;
  /** Add targets to the download queue (already-installed / already-queued
   * targets are skipped) and start it if idle. */
  enqueue: (targets: ModelTarget[]) => void;
  /** Re-enqueue whatever a failure or cancel left undone. */
  retry: () => void;
  /** Back-compat single-target entry point (meeting model card). */
  download: (target: ModelTarget) => Promise<void>;
  cancel: () => Promise<void>;
  refresh: () => Promise<void>;
};

export function isModelTargetReady(
  status: AsrModelStatus | null,
  target: ModelTarget,
): boolean {
  if (!status) return false;
  switch (target) {
    case "sensevoice":
      return status.sensevoiceReady;
    case "qwen":
      return status.qwenReady;
    case "streaming":
      return status.paraformerStreamingReady;
    case "offline":
      return status.paraformerOfflineReady;
  }
}

/** Union of two target lists, `add` entries last, no duplicates. */
function mergeTargets(prev: ModelTarget[], add: ModelTarget[]): ModelTarget[] {
  return [...prev.filter((t) => !add.includes(t)), ...add];
}

/** Owns model status + the single download queue. Only one model downloads at
 * a time (the backend guard rejects concurrent starts and the cancel command
 * is global), so the runner awaits each invoke before starting the next. */
function useProvideMeetingModels(): MeetingModels {
  const [status, setStatus] = useState<AsrModelStatus | null>(null);
  const [active, setActive] = useState<ModelTarget | null>(null);
  const [queued, setQueued] = useState<ModelTarget[]>([]);
  const [progress, setProgress] = useState<ModelProgressState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [failed, setFailed] = useState<ModelTarget[]>([]);
  const [cancelled, setCancelled] = useState(false);

  // Refs mirror the queue for the async runner (state is for rendering).
  const statusRef = useRef<AsrModelStatus | null>(null);
  const queueRef = useRef<ModelTarget[]>([]);
  const activeRef = useRef<ModelTarget | null>(null);
  const runningRef = useRef(false);
  const cancelRef = useRef(false);

  const refresh = useCallback(async () => {
    try {
      const s = await api.checkAsrModelStatus();
      statusRef.current = s;
      setStatus(s);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    let un: (() => void) | undefined;
    let disposed = false;
    void listen<{
      phase: string;
      message: string;
      bytes: number;
      total: number | null;
      percent?: number | null;
    }>("asr-download-progress", (e) => {
      const p = e.payload;
      const pct =
        p.percent ??
        (p.total && p.total > 0 ? (p.bytes / p.total) * 100 : null);
      setProgress({ phase: p.phase, message: p.message, percent: pct });
    }).then((fn) => {
      if (disposed) fn();
      else un = fn;
    });
    return () => {
      disposed = true;
      un?.();
    };
  }, []);

  /** Drain the queue one target at a time. On failure (or cancel) the rest of
   * the queue is parked in `failed` so `retry()` can resume it. */
  const runQueue = useCallback(async () => {
    if (runningRef.current) return;
    runningRef.current = true;
    for (;;) {
      if (cancelRef.current) {
        // A cancel landed between downloads (or right as one settled): stop
        // before starting the next target and park the rest for retry.
        const remaining = [...queueRef.current];
        queueRef.current = [];
        if (remaining.length > 0) {
          setFailed((prev) => mergeTargets(prev, remaining));
          setCancelled(true);
        }
        break;
      }
      const target = queueRef.current.shift();
      if (!target) break;
      activeRef.current = target;
      setActive(target);
      setQueued([...queueRef.current]);
      setProgress({ phase: "waiting", message: "准备下载…", percent: null });
      try {
        const next =
          target === "sensevoice"
            ? await api.startAsrModelDownload()
            : target === "qwen"
              ? await api.downloadQwen3Sherpa()
              : target === "streaming"
                ? await api.downloadParaformerStreaming()
                : await api.downloadParaformerOffline();
        statusRef.current = next;
        setStatus(next);
      } catch (e) {
        const remaining = [target, ...queueRef.current];
        queueRef.current = [];
        setFailed((prev) => mergeTargets(prev, remaining));
        if (cancelRef.current) {
          // Explicit user cancel: stop quietly, never auto-restart.
          setCancelled(true);
        } else {
          setError(String(e));
        }
        break;
      }
    }
    activeRef.current = null;
    runningRef.current = false;
    setActive(null);
    setQueued([]);
    setProgress(null);
  }, []);

  const enqueue = useCallback(
    (targets: ModelTarget[]) => {
      const add = targets.filter(
        (t) =>
          !isModelTargetReady(statusRef.current, t) &&
          t !== activeRef.current &&
          !queueRef.current.includes(t),
      );
      if (add.length === 0) return;
      cancelRef.current = false;
      setCancelled(false);
      setError(null);
      // Only clear the failure records of the targets being re-enqueued;
      // unrelated targets keep their failed state visible.
      setFailed((prev) => prev.filter((t) => !add.includes(t)));
      queueRef.current = [...queueRef.current, ...add];
      setQueued([...queueRef.current]);
      void runQueue();
    },
    [runQueue],
  );

  const retry = useCallback(() => {
    if (failed.length > 0) enqueue(failed);
  }, [failed, enqueue]);

  const download = useCallback(
    async (target: ModelTarget) => {
      enqueue([target]);
    },
    [enqueue],
  );

  const cancel = useCallback(async () => {
    // Mark first so the in-flight invoke's rejection is treated as a user
    // cancel (queue drains into `failed` for a possible retry, no error UI).
    cancelRef.current = true;
    try {
      await api.cancelAsrModelDownload();
    } catch (e) {
      setError(String(e));
    }
    // active/progress clear when the in-flight download promise settles.
  }, []);

  return {
    status,
    active,
    queued,
    progress,
    error,
    failed,
    cancelled,
    enqueue,
    retry,
    download,
    cancel,
    refresh,
  };
}

const MeetingModelsContext = createContext<MeetingModels | null>(null);

/** Mount once at the App root (outside any tab-switched subtree) so the single
 * download listener + queue state survive navigation between tabs. */
export function MeetingModelsProvider({ children }: { children: ReactNode }) {
  const models = useProvideMeetingModels();
  return (
    <MeetingModelsContext.Provider value={models}>
      {children}
    </MeetingModelsContext.Provider>
  );
}

/** Consume the app-level model state. Must be used under MeetingModelsProvider. */
export function useMeetingModels(): MeetingModels {
  const ctx = useContext(MeetingModelsContext);
  if (!ctx) {
    throw new Error("useMeetingModels must be used within a MeetingModelsProvider");
  }
  return ctx;
}
