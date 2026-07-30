import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";
import { listen } from "@tauri-apps/api/event";
import { api, type AsrModelStatus } from "./api";

// App-level Paraformer model install state.
//
// Meeting transcription needs the Paraformer models, which onboarding may have
// skipped. Model downloads are inherently global (onboarding downloads them
// too) and the backend runs a single-flight ~1GB download, so the *UI* state
// must also be app-level: held here in a provider mounted at the App root that
// never unmounts on tab switches. That keeps one `asr-download-progress`
// listener and one in-flight download's progress/cancel binding alive even when
// the meeting panel is not mounted — switch away mid-download and back, and the
// progress/cancel are still there.

export type ModelTarget = "streaming" | "offline";

export type ModelProgressState = {
  phase: string;
  message: string;
  percent: number | null;
};

export type MeetingModels = {
  status: AsrModelStatus | null;
  active: ModelTarget | null;
  progress: ModelProgressState | null;
  error: string | null;
  download: (target: ModelTarget) => Promise<void>;
  cancel: () => Promise<void>;
  refresh: () => Promise<void>;
};

/** Owns Paraformer model status + a single in-flight download. Only one model
 * downloads at a time (the backend cancel command is global), so `active` marks
 * which target is running and disables the other. `download()` resolves with the
 * refreshed status; the `asr-download-progress` listener drives the progress bar
 * meanwhile (same pattern as OnboardingWizard). */
function useProvideMeetingModels(): MeetingModels {
  const [status, setStatus] = useState<AsrModelStatus | null>(null);
  const [active, setActive] = useState<ModelTarget | null>(null);
  const [progress, setProgress] = useState<ModelProgressState | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.checkAsrModelStatus());
      setError(null);
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

  const download = useCallback(async (target: ModelTarget) => {
    setError(null);
    setActive(target);
    setProgress({ phase: "waiting", message: "准备下载…", percent: null });
    try {
      const next =
        target === "streaming"
          ? await api.downloadParaformerStreaming()
          : await api.downloadParaformerOffline();
      setStatus(next);
    } catch (e) {
      setError(String(e));
    } finally {
      setActive(null);
      setProgress(null);
    }
  }, []);

  const cancel = useCallback(async () => {
    try {
      await api.cancelAsrModelDownload();
    } catch (e) {
      setError(String(e));
    }
    // active/progress clear when the in-flight download() promise settles.
  }, []);

  return { status, active, progress, error, download, cancel, refresh };
}

const MeetingModelsContext = createContext<MeetingModels | null>(null);

/** Mount once at the App root (outside any tab-switched subtree) so the single
 * download listener + in-flight state survive navigation between tabs. */
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
