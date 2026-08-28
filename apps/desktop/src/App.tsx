import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, type AsrModelStatus, type MeetingAppCatalog } from "./api";
import { HotkeyRecorder } from "./HotkeyRecorder";
import { OnboardingWizard } from "./OnboardingWizard";
import { formatHotkeyLabel } from "./hotkeyFormat";
import { chooseAudioDevice } from "./audioDeviceSelection";
import { ClipboardWriteGate } from "./clipboardWriteGate";
import { firstNonBlankText } from "./sessionText";
import { LatestRequestGate } from "./latestRequestGate";
import { Icon, type IconName } from "./Icons";
import { ThemeToggle } from "./ThemeToggle";
import { ChordCaptureChip } from "./ChordCaptureChip";
import { MeetingPanel } from "./MeetingPanel";
import { IdentityPanel } from "./IdentityPanel";
import { MeetingModelsProvider } from "./meetingModels";
import {
  acknowledgeWindowsMicrophoneNotice,
  hasAcknowledgedWindowsMicrophoneNotice,
} from "./microphoneConsent";
import {
  copyToastLabel,
  correctorFallbackNotice,
  correctorFallbackReasonLabel,
  formatAsrEngineLabel,
} from "./fallbackPresentation";
import lumenMark from "./assets/product-icons/lumen-asr.svg";
import { initSoundFeedback, setSoundsEnabled } from "./sound";
import type {
  AsrStatus,
  AudioDevice,
  BuildInfo,
  CorrectorStatus,
  DictationAttemptRecord,
  DictionaryEntry,
  EditLearningFeedback,
  EditLearningObservability,
  EditEvent,
  EditObservation,
  Health,
  LearnCandidate,
  LearningConfig,
  LearningProposal,
  SessionRecord,
  TabId,
} from "./types";

const IS_WINDOWS = navigator.userAgent.includes("Windows");

// Edit-learning feedback banners auto-dismiss after this long. They are
// informational only — the actionable candidates stay in the Learn panel.
const EDIT_LEARNING_NOTICE_TTL_MS = 8000;

function previewText(s?: string | null, n = 72): string {
  if (!s) return "—";
  const t = s.replace(/\s+/g, " ").trim();
  return t.length > n ? t.slice(0, n) + "…" : t;
}

function formatTime(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

function candidateFromLearningProposal(
  proposal: LearningProposal,
): LearnCandidate | null {
  let payload: Record<string, unknown>;
  try {
    payload = JSON.parse(proposal.payload_json) as Record<string, unknown>;
  } catch {
    return null;
  }
  if (proposal.kind === "term" && typeof payload.term === "string") {
    return {
      kind: "term",
      term: payload.term,
      reason: `听写后编辑证据 · 置信度 ${Math.round(proposal.confidence * 100)}%`,
      proposal_id: proposal.id,
    };
  }
  if (
    proposal.kind === "replacement" &&
    typeof payload.fromText === "string" &&
    typeof payload.toText === "string"
  ) {
    return {
      kind: "replacement",
      from_text: payload.fromText,
      to_text: payload.toText,
      reason: `需要确认的听写纠正 · 置信度 ${Math.round(proposal.confidence * 100)}%`,
      proposal_id: proposal.id,
    };
  }
  return null;
}

function editObservationReasonLabel(reason: string): string {
  const labels: Record<string, string> = {
    stable_edit_captured: "修改稳定后已捕获",
    stable_edit_captured_without_edit_event: "已观察到修改，但编辑记录保存失败",
    observation_window_elapsed: "观察时间结束",
    anchor_unavailable: "插入后无法建立文本锚点",
    anchor_task_failed: "锚点任务失败",
    inserted_text_not_found: "在输入框里找不到刚插入的文字",
    inserted_text_not_unique: "输入框里有多处相同文字，无法唯一定位",
    target_field_unavailable: "目标输入框不可读取",
    focused_field_changed: "焦点已切换到其他输入框",
    anchor_mismatch: "锚点外文本发生变化",
    next_dictation_started: "开始了下一次听写",
    edit_not_stable_before_timeout: "修改尚未稳定时观察结束",
    target_field_unrecovered_before_timeout: "观察结束前未能重新读取目标输入框",
    focused_field_unrecovered_before_timeout: "观察结束前焦点未回到原输入框",
    anchor_mismatch_before_timeout: "观察结束前文本锚点未恢复",
    edit_event_persistence_failed: "修改已观察到，但数据库保存失败",
    target_metadata_unavailable: "插入时无法读取目标应用信息",
    session_persistence_timeout: "听写记录保存超时，无法写入修改观察结果",
  };
  return labels[reason] || reason;
}

type LocalAsrEngine = "sensevoice" | "qwen" | "whisper";

function localAsrEngine(provider: string): LocalAsrEngine {
  if (provider === "local_qwen") return "qwen";
  if (provider === "local_whisper") return "whisper";
  return "sensevoice";
}

function localAsrModelDir(status: AsrModelStatus, provider: string): string {
  switch (localAsrEngine(provider)) {
    case "qwen":
      return status.qwenDir;
    case "whisper":
      return status.whisperDir;
    case "sensevoice":
      return status.sensevoiceDir;
  }
}

const NAV: { id: TabId; label: string; icon: IconName; title: string; blurb: string }[] = [
  {
    id: "record",
    label: "录音",
    icon: "mic",
    title: "录音",
    blurb: "本地转写 · 热键或按钮开始",
  },
  {
    id: "meeting",
    label: "会议",
    icon: "meeting",
    title: "会议",
    blurb: "会议库 · 纪要 · 逐字稿",
  },
  {
    id: "history",
    label: "历史",
    icon: "history",
    title: "历史",
    blurb: "核对文本 · 复制 · 必要时重识别",
  },
  {
    id: "dictionary",
    label: "词典",
    icon: "dictionary",
    title: "词典",
    blurb: "术语与替换规则",
  },
  {
    id: "identity",
    label: "声纹",
    icon: "identity",
    title: "声纹管理",
    blurb: "跨会议声纹库 · 重命名 · 合并 · 这是我",
  },
  {
    id: "learn",
    label: "学习",
    icon: "learn",
    title: "编辑学习",
    blurb: "从改写生成词典候选",
  },
  {
    id: "settings",
    label: "设置",
    icon: "settings",
    title: "设置",
    blurb: "权限 · 热键 · 插入 · 修正 · 学习",
  },
  {
    id: "overview",
    label: "概览",
    icon: "overview",
    title: "概览",
    blurb: "状态与快捷入口",
  },
];

export default function App() {
  const [tab, setTab] = useState<TabId>("record");
  const [health, setHealth] = useState<Health | null>(null);
  // Build identity for the version chip (same source as Settings' build line):
  // shows which git build is actually running next to the crate version.
  const [buildInfo, setBuildInfo] = useState<BuildInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [copyToast, setCopyToast] = useState<string | null>(null);
  const copyToastTimerRef = useRef<number | null>(null);
  const showCopyToast = useCallback((text: string) => {
    setCopyToast(text);
    if (copyToastTimerRef.current != null) {
      window.clearTimeout(copyToastTimerRef.current);
    }
    copyToastTimerRef.current = window.setTimeout(() => {
      setCopyToast(null);
      copyToastTimerRef.current = null;
    }, 2200);
  }, []);
  const [powerWarning, setPowerWarning] = useState<string | null>(null);
  // Prolonged-silence grace period. The backend owns the authoritative sample
  // clock; the deadline here is presentation-only for the visible countdown.
  const [silenceWarning, setSilenceWarning] = useState<{
    meetingId: string;
    deadlineMs: number;
  } | null>(null);
  // Max-duration grace period ("forgot to stop" cap). Same presentation-only
  // countdown contract as the silence warning above.
  const [maxDurationWarning, setMaxDurationWarning] = useState<{
    meetingId: string;
    deadlineMs: number;
  } | null>(null);
  // Info banner shown after the watchdog auto-stopped a silent recording.
  const [autoStopNotice, setAutoStopNotice] = useState<string | null>(null);
  // Calendar-linked meeting whose end time passed while still recording. A
  // reminder with a Stop button — never an auto-stop.
  const [calendarEnded, setCalendarEnded] = useState<{
    meetingId: string;
    title: string;
  } | null>(null);
  // Meetings we already asked to stop (auto-stop or calendar reminder), so a
  // second event / click never fires a duplicate stop command.
  const stoppedMeetingsRef = useRef<Set<string>>(new Set());
  const handledEditFeedbackRef = useRef<Set<string>>(new Set());
  const [editLearningNotices, setEditLearningNotices] = useState<
    EditLearningFeedback[]
  >([]);
  // Mirror of editLearningNotices for code paths that need the current value
  // without being a state-updater (single-banner supersede + auto-dismiss).
  const editLearningNoticesRef = useRef<EditLearningFeedback[]>([]);
  const [recoveryNotices, setRecoveryNotices] = useState<
    { meetingId: string; text: string; ok: boolean }[]
  >([]);
  const [editFeedbackRevision, setEditFeedbackRevision] = useState(0);
  const [editLearningObservability, setEditLearningObservability] =
    useState<EditLearningObservability | null>(null);
  const [busy, setBusy] = useState(false);
  const [hotkeyLabel, setHotkeyLabel] = useState("⌥Space");
  const [hotkeyEnabled, setHotkeyEnabledUi] = useState(true);
  const [showOnboarding, setShowOnboarding] = useState(false);
  const [onboardingIncomplete, setOnboardingIncomplete] = useState(false);
  // App-level meeting-detection prompt (opt-in, capability-gated). The backend
  // policy decides *when* to prompt; here we only render it and relay the user's
  // choice. Kept out of MeetingPanel on purpose so it is visible on any tab.
  const [detected, setDetected] = useState<{
    bundleId: string;
    appClass: string;
    displayName: string;
  } | null>(null);
  // End-of-meeting stop suggestion for a detection-started recording. The
  // backend asks; nothing is ever stopped without an explicit user click.
  const [stopSuggested, setStopSuggested] = useState<{
    bundleId: string;
    meetingId: string | null;
    displayName: string;
  } | null>(null);

  const [sessions, setSessions] = useState<SessionRecord[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [sessionsRequestGate] = useState(
    () => new LatestRequestGate<SessionRecord[]>(),
  );

  const [dict, setDict] = useState<DictionaryEntry[]>([]);
  const [termInput, setTermInput] = useState("");
  const [fromInput, setFromInput] = useState("");
  const [toInput, setToInput] = useState("");

  const [learnBefore, setLearnBefore] = useState("");
  const [learnAfter, setLearnAfter] = useState("");
  const [candidates, setCandidates] = useState<LearnCandidate[]>([]);
  const [sessionLearn, setSessionLearn] = useState<{
    sessionId: string;
    baseline: string;
    candidates: LearnCandidate[];
  } | null>(null);

  const refreshHealth = useCallback(async () => {
    try {
      setHealth(await api.health());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refreshSessions = useCallback(async () => {
    try {
      const outcome = await sessionsRequestGate.run(() => api.listSessions(100));
      if (outcome.status === "current") setSessions(outcome.value);
    } catch (e) {
      setError(String(e));
    }
  }, [sessionsRequestGate]);

  useEffect(
    () => () => {
      sessionsRequestGate.cancelPending();
    },
    [sessionsRequestGate],
  );

  const refreshDict = useCallback(async () => {
    try {
      setDict(await api.listDictionary());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const loadEditLearningCandidates = useCallback(
    async (feedback: EditLearningFeedback, guard?: () => boolean) => {
      const proposals = await api.listEditLearningProposals(
        feedback.edit_session_id,
      );
      // A newer banner may have superseded this feedback while the proposals
      // were loading — never let a stale request overwrite its candidates.
      if (guard && !guard()) return;
      const nextCandidates = proposals
        .filter(
          (proposal) =>
            proposal.status === "proposed" &&
            feedback.proposal_ids.includes(proposal.id),
        )
        .map(candidateFromLearningProposal)
        .filter((candidate): candidate is LearnCandidate => candidate !== null);
      const replacement = nextCandidates.find(
        (candidate) => candidate.kind === "replacement",
      );
      const term = nextCandidates.find((candidate) => candidate.kind === "term");
      setLearnBefore(replacement?.from_text ?? "");
      setLearnAfter(replacement?.to_text ?? term?.term ?? "");
      setCandidates(nextCandidates);
      setSessionLearn(null);
      setEditFeedbackRevision((value) => value + 1);
    },
    [],
  );

  // Dismiss a feedback banner and acknowledge it in the outbox so it does
  // not resurface on the next launch. Used by the 关闭 button, the
  // auto-dismiss timer, and single-banner supersede.
  const dismissEditLearningNotice = useCallback((id: string) => {
    editLearningNoticesRef.current = editLearningNoticesRef.current.filter(
      (notice) => notice.id !== id,
    );
    setEditLearningNotices(editLearningNoticesRef.current);
    void api
      .acknowledgeEditLearningFeedback(id)
      .catch((reason) => setError(String(reason)));
  }, []);

  // Durable edit-learning feedback. The live event is only an accelerator;
  // startup also drains the outbox so a hidden or closed UI never loses it.
  // Only one banner is shown at a time and it auto-dismisses — these notices
  // are informational; the actionable candidates live in the Learn panel.
  useEffect(() => {
    let disposed = false;
    let un: (() => void) | undefined;
    const dismissTimers: number[] = [];
    const showFeedback = async (feedback: EditLearningFeedback) => {
      if (disposed || handledEditFeedbackRef.current.has(feedback.id)) return;
      handledEditFeedbackRef.current.add(feedback.id);
      if (!editLearningNoticesRef.current.some((n) => n.id === feedback.id)) {
        // New feedback supersedes older banners — acknowledge them so they
        // never pile up again on the next launch.
        for (const notice of editLearningNoticesRef.current) {
          void api.acknowledgeEditLearningFeedback(notice.id).catch(() => {});
        }
        editLearningNoticesRef.current = [feedback];
        setEditLearningNotices([feedback]);
        dismissTimers.push(
          window.setTimeout(() => {
            if (!disposed) dismissEditLearningNotice(feedback.id);
          }, EDIT_LEARNING_NOTICE_TTL_MS),
        );
      }
      try {
        if (feedback.proposal_ids.length) {
          await loadEditLearningCandidates(
            feedback,
            () => editLearningNoticesRef.current[0]?.id === feedback.id,
          );
        } else {
          setEditFeedbackRevision((value) => value + 1);
        }
        if (disposed) return;
        void api
          .getEditLearningObservability()
          .then((snapshot) => {
            if (!disposed) setEditLearningObservability(snapshot);
          })
          .catch(() => undefined);
      } catch (reason) {
        if (!disposed) setError(String(reason));
      }
    };
    listen<EditLearningFeedback>("edit-learning-feedback", (event) => {
      void showFeedback(event.payload);
    }).then((fn) => {
      if (disposed) fn();
      else {
        un = fn;
        void api
          .listEditLearningFeedback(100)
          .then((notices) => notices.forEach((notice) => void showFeedback(notice)))
          .catch((reason) => {
            if (!disposed) setError(String(reason));
          });
      }
    });
    return () => {
      disposed = true;
      dismissTimers.forEach((timer) => window.clearTimeout(timer));
      un?.();
    };
  }, [loadEditLearningCandidates, dismissEditLearningNotice]);

  useEffect(() => {
    let un: (() => void) | undefined;
    listen<EditObservation>("edit-observation-completed", () => {
      setEditFeedbackRevision((value) => value + 1);
    }).then((fn) => {
      un = fn;
    });
    return () => un?.();
  }, []);

  // Global hotkey / capsule events
  useEffect(() => {
    initSoundFeedback();
    void api
      .getUiConfig()
      .then((c) => setSoundsEnabled(c.sounds))
      .catch(() => {});
    let un: (() => void) | undefined;
    listen<{
      phase: string;
      message?: string;
      outcome?: {
        text: string;
        asrText?: string;
        asrEngine?: string;
        correctorEngine?: string;
        fallbackReason?: string | null;
        insertNotice?: string | null;
        session?: SessionRecord;
      };
    }>("dictation", (e) => {
      const p = e.payload;
      if (p.phase === "listening") {
        setBusy(true);
        setError(null);
      } else if (p.phase === "processing") {
        setBusy(true);
      } else if (p.phase === "done" && p.outcome) {
        setBusy(false);
        // Update history quietly — do not force-activate or jump UI aggressively.
        void refreshHealth();
        void refreshSessions();
        const copied = copyToastLabel(p.outcome.insertNotice);
        if (copied) showCopyToast(copied);
        // Stash for Record tab if user opens it; hotkey path must not steal OS focus.
        window.dispatchEvent(
          new CustomEvent("lumen-dictation-done", { detail: p.outcome })
        );
      } else if (p.phase === "error") {
        setBusy(false);
        setError(p.message || "dictation error");
      } else if (p.phase === "idle") {
        setBusy(false);
      }
    }).then((fn) => {
      un = fn;
    });
    return () => un?.();
  }, [refreshHealth, refreshSessions, showCopyToast]);

  // Meeting-detection prompt lifecycle: the backend emits `meeting-detected`
  // when a stable meeting-app input is seen, and `meeting-detection-cancelled`
  // when the signal disappears before the user acts (auto-retract the prompt).
  useEffect(() => {
    let unDetected: (() => void) | undefined;
    let unCancelled: (() => void) | undefined;
    listen<{ bundleId: string; appClass: string; displayName: string }>(
      "meeting-detected",
      (e) => {
        setDetected(e.payload);
      },
    ).then((fn) => {
      unDetected = fn;
    });
    listen("meeting-detection-cancelled", () => setDetected(null)).then((fn) => {
      unCancelled = fn;
    });
    return () => {
      unDetected?.();
      unCancelled?.();
    };
  }, []);

  // Power warnings during a meeting recording: the backend emits
  // `meeting-power-warning` when the machine is on a low battery or is about to
  // sleep, either of which can cut a recording short. Show a dismissible banner
  // that auto-clears after a while.
  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    listen<{ kind: string; percent?: number }>("meeting-power-warning", (e) => {
      const msg =
        e.payload.kind === "low-battery"
          ? `电量偏低${
              typeof e.payload.percent === "number" ? `（${e.payload.percent}%）` : ""
            }，会议录音可能因断电中断，建议接上电源。`
          : "系统即将睡眠，会议录音会中断。";
      setPowerWarning(msg);
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => setPowerWarning(null), 15000);
    }).then((fn) => {
      // If the effect was already cleaned up before the listener resolved
      // (e.g. StrictMode remount), unlisten immediately instead of leaking it.
      if (cancelled) fn();
      else un = fn;
    });
    return () => {
      cancelled = true;
      un?.();
      if (timer) clearTimeout(timer);
    };
  }, []);

  // The backend starts a grace-period countdown only after every available
  // physical audio track has remained quiet for the configured threshold.
  // Sound resuming (or a Keep action acknowledged by the backend) retracts it.
  useEffect(() => {
    let unWarning: (() => void) | undefined;
    let unCleared: (() => void) | undefined;
    let cancelled = false;
    Promise.all([
      listen<{ meetingId: string; countdownSeconds: number }>(
        "meeting-silence-warning",
        (e) => {
          if (stoppedMeetingsRef.current.has(e.payload.meetingId)) return;
          setSilenceWarning({
            meetingId: e.payload.meetingId,
            deadlineMs: Date.now() + Math.max(0, e.payload.countdownSeconds) * 1000,
          });
        },
      ),
      listen<{ meetingId: string }>("meeting-silence-cleared", (e) => {
        setSilenceWarning((current) =>
          current?.meetingId === e.payload.meetingId ? null : current,
        );
      }),
    ]).then(([warning, cleared]) => {
      if (cancelled) {
        warning();
        cleared();
      } else {
        unWarning = warning;
        unCleared = cleared;
      }
    });
    return () => {
      cancelled = true;
      unWarning?.();
      unCleared?.();
    };
  }, []);

  // Max-duration cap: the backend starts a grace-period countdown once the
  // recording's wall-clock age crosses the configured limit. Only an explicit
  // Keep retracts it (there is no "sound resumed" escape for a duration cap).
  useEffect(() => {
    let unWarning: (() => void) | undefined;
    let unCleared: (() => void) | undefined;
    let cancelled = false;
    Promise.all([
      listen<{ meetingId: string; countdownSeconds: number }>(
        "meeting-max-duration-warning",
        (e) => {
          if (stoppedMeetingsRef.current.has(e.payload.meetingId)) return;
          setMaxDurationWarning({
            meetingId: e.payload.meetingId,
            deadlineMs: Date.now() + Math.max(0, e.payload.countdownSeconds) * 1000,
          });
        },
      ),
      listen<{ meetingId: string }>("meeting-max-duration-cleared", (e) => {
        setMaxDurationWarning((current) =>
          current?.meetingId === e.payload.meetingId ? null : current,
        );
      }),
    ]).then(([warning, cleared]) => {
      if (cancelled) {
        warning();
        cleared();
      } else {
        unWarning = warning;
        unCleared = cleared;
      }
    });
    return () => {
      cancelled = true;
      unWarning?.();
      unCleared?.();
    };
  }, []);

  // Silence auto-stop: after the warning countdown expires with no new sound,
  // the FRONT-END owns the real stop path. Guard duplicate events/clicks and
  // show a dismissible completion banner.
  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    listen<{ meetingId: string; reason: string }>("meeting-auto-stop", (e) => {
      const { meetingId, reason } = e.payload;
      if (stoppedMeetingsRef.current.has(meetingId)) return;
      stoppedMeetingsRef.current.add(meetingId);
      setSilenceWarning(null);
      setMaxDurationWarning(null);
      const doneMessage =
        reason === "max_duration"
          ? "录音已达最长时长上限，已自动停止。"
          : "录音已因长时间无声自动停止。";
      void api
        .stopMeetingRecording(meetingId)
        .then(() => setAutoStopNotice(doneMessage))
        .catch((error) => {
          stoppedMeetingsRef.current.delete(meetingId);
          setError(`自动结束会议失败：${String(error)}`);
        });
    }).then((fn) => {
      if (cancelled) fn();
      else un = fn;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, []);

  // Calendar-end reminder: the backend emits `meeting-calendar-ended` when a
  // calendar-linked meeting's end time passes while it is still recording. This
  // is a REMINDER — show a banner with a Stop button; never auto-stop.
  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    listen<{ meetingId: string; title: string }>(
      "meeting-calendar-ended",
      (e) => {
        if (stoppedMeetingsRef.current.has(e.payload.meetingId)) return;
        setCalendarEnded(e.payload);
      },
    ).then((fn) => {
      if (cancelled) fn();
      else un = fn;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, []);

  // Interrupted-recording recovery: on launch the backend scans for recordings
  // left unfinished by a previous run (crash / kill / power loss). Recovery can
  // run before this listener is ready, so we both DRAIN the buffered outcomes on
  // mount and LISTEN for live ones; both feed the same keyed list (dedup by
  // meetingId) so nothing is lost or shown twice, and multiple recoveries each
  // get their own banner.
  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    type Recovery = {
      meetingId: string;
      title?: string;
      outcome: string;
      durationSeconds?: number;
      reason?: string;
    };
    const toNotice = (p: Recovery) => {
      const name = p.title?.trim() || "上次录音";
      if (p.outcome === "recovered") {
        const mins =
          typeof p.durationSeconds === "number"
            ? Math.round(p.durationSeconds / 60)
            : null;
        return {
          meetingId: p.meetingId,
          ok: true,
          text: `「${name}」上次被中断，已抢救${
            mins != null ? ` ${mins} 分钟` : ""
          }并重新处理。`,
        };
      }
      return {
        meetingId: p.meetingId,
        ok: false,
        text: `「${name}」上次被中断且无法恢复${
          p.reason ? `：${p.reason}` : ""
        }。`,
      };
    };
    const add = (p: Recovery) =>
      setRecoveryNotices((prev) =>
        prev.some((n) => n.meetingId === p.meetingId)
          ? prev
          : [...prev, toNotice(p)],
      );
    // Drain what recovery buffered before this listener existed.
    void api
      .takeRecoveryNotices()
      .then((list) => {
        if (!cancelled) list.forEach(add);
      })
      .catch(() => {});
    listen<Recovery>("meeting-recovery", (e) => add(e.payload)).then((fn) => {
      if (cancelled) fn();
      else un = fn;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, []);

  // End-of-meeting stop suggestion lifecycle: `meeting-detection-stop-suggested`
  // when the triggering app's input has been gone for the stop-stability
  // window, `meeting-detection-stop-cancelled` when the suggestion is moot
  // (input came back, or the recording ended another way).
  useEffect(() => {
    let unSuggested: (() => void) | undefined;
    let unStopCancelled: (() => void) | undefined;
    listen<{ bundleId: string; meetingId: string | null; displayName: string }>(
      "meeting-detection-stop-suggested",
      (e) => {
        setStopSuggested(e.payload);
      }
    ).then((fn) => {
      unSuggested = fn;
    });
    listen("meeting-detection-stop-cancelled", () =>
      setStopSuggested(null)
    ).then((fn) => {
      unStopCancelled = fn;
    });
    return () => {
      unSuggested?.();
      unStopCancelled?.();
    };
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const s = await api.getOnboardingState();
        setShowOnboarding(s.showWizard);
        setOnboardingIncomplete(!s.completed);
      } catch {
        /* ignore */
      }
    })();
  }, []);

  useEffect(() => {
    void refreshHealth();
    void (async () => {
      try {
        const hk = await api.getHotkeyConfig();
        setHotkeyEnabledUi(hk.enabled);
        setHotkeyLabel(formatHotkeyLabel(hk.toggle));
      } catch {
        /* ignore */
      }
    })();
  }, [refreshHealth]);

  // Build identity is static for the process; fetch once. A failure just leaves
  // the chip showing the version alone (no sha), never blocking the header.
  useEffect(() => {
    api
      .buildInfo()
      .then(setBuildInfo)
      .catch(() => setBuildInfo(null));
  }, []);

  useEffect(() => {
    if (tab === "history" || tab === "overview") void refreshSessions();
    if (tab === "overview") {
      void api
        .getEditLearningObservability()
        .then(setEditLearningObservability)
        .catch(() => setEditLearningObservability(null));
    }
    if (tab === "dictionary" || tab === "overview" || tab === "learn")
      void refreshDict();
    if (tab === "settings") {
      void (async () => {
        try {
          const hk = await api.getHotkeyConfig();
          setHotkeyEnabledUi(hk.enabled);
          setHotkeyLabel(formatHotkeyLabel(hk.toggle));
        } catch {
          /* ignore */
        }
      })();
    }
  }, [tab, refreshSessions, refreshDict]);

  const selected = sessions.find((s) => s.id === selectedId) ?? null;

  async function run(label: string, fn: () => Promise<void>) {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(`${label}: ${String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  const nav = NAV.find((n) => n.id === tab) ?? NAV[0];

  useEffect(() => {
    return () => {
      if (copyToastTimerRef.current != null) {
        window.clearTimeout(copyToastTimerRef.current);
      }
    };
  }, []);

  return (
    <MeetingModelsProvider>
    <div className="app-frame">
      {copyToast && (
        <div className="copy-toast" role="status" aria-live="polite">
          {copyToast}
        </div>
      )}
      {showOnboarding && (
        <OnboardingWizard
          onDone={() => {
            setShowOnboarding(false);
            setOnboardingIncomplete(false);
            void refreshHealth();
          }}
        />
      )}
      {detected && (
        <DetectionPrompt
          bundleId={detected.bundleId}
          appClass={detected.appClass}
          displayName={detected.displayName}
          onStart={(captureSystemAudio) =>
            void (async () => {
              setDetected(null);
              try {
                await api.acceptMeetingDetection(captureSystemAudio);
                setTab("meeting");
              } catch (e) {
                setError(String(e));
              }
            })()
          }
          onDismiss={() =>
            void (async () => {
              setDetected(null);
              try {
                await api.dismissMeetingDetection();
              } catch {
                /* ignore */
              }
            })()
          }
        />
      )}
      {stopSuggested && (
        <StopSuggestPrompt
          bundleId={stopSuggested.bundleId}
          displayName={stopSuggested.displayName}
          onStop={() =>
            void (async () => {
              setStopSuggested(null);
              try {
                await api.acceptMeetingDetectionStop();
              } catch (e) {
                setError(String(e));
              }
            })()
          }
          onKeep={() =>
            void (async () => {
              setStopSuggested(null);
              try {
                await api.declineMeetingDetectionStop();
              } catch {
                /* ignore */
              }
            })()
          }
        />
      )}
      {silenceWarning && (
        <AutoStopPrompt
          title="持续检测不到声音"
          promptId="silence"
          deadlineMs={silenceWarning.deadlineMs}
          onKeep={() => {
            const { meetingId } = silenceWarning;
            void api.continueMeetingAfterSilence(meetingId).catch((e) =>
              setError(String(e)),
            );
          }}
          onStop={() => {
            const { meetingId } = silenceWarning;
            setSilenceWarning(null);
            if (stoppedMeetingsRef.current.has(meetingId)) return;
            stoppedMeetingsRef.current.add(meetingId);
            void api.stopMeetingRecording(meetingId).catch((e) => {
              stoppedMeetingsRef.current.delete(meetingId);
              setError(String(e));
            });
          }}
        />
      )}
      {maxDurationWarning && (
        <AutoStopPrompt
          title="录音已达最长时长上限"
          promptId="max-duration"
          deadlineMs={maxDurationWarning.deadlineMs}
          onKeep={() => {
            const { meetingId } = maxDurationWarning;
            void api.continueMeetingAfterMaxDuration(meetingId).catch((e) =>
              setError(String(e)),
            );
          }}
          onStop={() => {
            const { meetingId } = maxDurationWarning;
            setMaxDurationWarning(null);
            if (stoppedMeetingsRef.current.has(meetingId)) return;
            stoppedMeetingsRef.current.add(meetingId);
            void api.stopMeetingRecording(meetingId).catch((e) => {
              stoppedMeetingsRef.current.delete(meetingId);
              setError(String(e));
            });
          }}
        />
      )}
      {/* System titlebar (Visible) — native macOS drag / traffic lights */}
      <div className="app-body">
        <nav className="sidebar" aria-label="主导航">
          <div className="sidebar-brand">
            <img className="sidebar-brand-mark" src={lumenMark} alt="" aria-hidden />
            <div className="sidebar-brand-text">
              <span className="sidebar-brand-name">Lumen</span>
              <span className="sidebar-brand-sub">ASR</span>
            </div>
          </div>
          {NAV.map((item) => (
            <button
              key={item.id}
              type="button"
              className={`nav-item ${tab === item.id ? "active" : ""}`}
              onClick={() => setTab(item.id)}
            >
              <span className="nav-icon" aria-hidden>
                <Icon name={item.icon} size={18} />
              </span>
              <span>{item.label}</span>
            </button>
          ))}
          <div className="sidebar-spacer" />
          <div className="sidebar-foot">
            <ThemeToggle />
          </div>
          <div className="sidebar-meta">
            {onboardingIncomplete && (
              <>
                <button
                  type="button"
                  className="linkish"
                  onClick={() =>
                    void (async () => {
                      try {
                        await api.reopenOnboarding();
                        setShowOnboarding(true);
                      } catch (e) {
                        setError(String(e));
                      }
                    })()
                  }
                >
                  完成首次设置
                </button>
                <br />
              </>
            )}
            {hotkeyEnabled ? (
              <>
                热键 <span className="kbd">{hotkeyLabel}</span>
                <br />
                按住说话
              </>
            ) : (
              <>热键已关闭</>
            )}
          </div>
        </nav>

        <div className="content">
          {editLearningNotices.map((feedback) => (
            <div
              key={feedback.id}
              className={`banner ${
                feedback.kind === "observation_unavailable" ? "error" : "success"
              }`}
              role={feedback.kind === "observation_unavailable" ? "alert" : "status"}
            >
              {feedback.message}
              {feedback.proposal_ids.length > 0 && (
                <button
                  type="button"
                  className="linkish"
                  onClick={() => {
                    void loadEditLearningCandidates(feedback)
                      .then(() => setTab("learn"))
                      .catch((reason) => setError(String(reason)));
                  }}
                >
                  查看候选
                </button>
              )}
              <button
                type="button"
                className="linkish"
                onClick={() => dismissEditLearningNotice(feedback.id)}
              >
                关闭
              </button>
            </div>
          ))}
          {error && (
            <div className="banner error" role="alert">
              {error}
              <button type="button" className="linkish" onClick={() => setError(null)}>
                关闭
              </button>
            </div>
          )}
          {notice && (
            <div className="banner success" role="status">
              {notice}
              <button type="button" className="linkish" onClick={() => setNotice(null)}>
                关闭
              </button>
            </div>
          )}
          {powerWarning && (
            <div className="banner error" role="alert">
              {powerWarning}
              <button
                type="button"
                className="linkish"
                onClick={() => setPowerWarning(null)}
              >
                关闭
              </button>
            </div>
          )}
          {autoStopNotice && (
            <div className="banner success" role="status">
              {autoStopNotice}
              <button
                type="button"
                className="linkish"
                onClick={() => setAutoStopNotice(null)}
              >
                关闭
              </button>
            </div>
          )}
          {calendarEnded && (
            <div className="banner" role="status">
              {`「${calendarEnded.title?.trim() || "本次会议"}」的日历时间已到，是否停止录音？`}
              <button
                type="button"
                className="linkish"
                onClick={() => {
                  const { meetingId } = calendarEnded;
                  if (!stoppedMeetingsRef.current.has(meetingId)) {
                    stoppedMeetingsRef.current.add(meetingId);
                    void api.stopMeetingRecording(meetingId).catch(() => {});
                  }
                  setCalendarEnded(null);
                }}
              >
                停止录音
              </button>
              <button
                type="button"
                className="linkish"
                onClick={() => setCalendarEnded(null)}
              >
                关闭
              </button>
            </div>
          )}
          {recoveryNotices.map((n) => (
            <div
              key={n.meetingId}
              className={`banner ${n.ok ? "success" : "error"}`}
              role="status"
            >
              {n.text}
              <button
                type="button"
                className="linkish"
                onClick={() =>
                  setRecoveryNotices((prev) =>
                    prev.filter((x) => x.meetingId !== n.meetingId),
                  )
                }
              >
                关闭
              </button>
            </div>
          ))}

          <div className="content-scroll">
            <div className="content-header">
              <div>
                <h1>{nav.title}</h1>
                <p>{nav.blurb}</p>
              </div>
              {health && (
                <div className="actions" style={{ marginTop: 0 }}>
                  <span
                    className="chip"
                    title="版本 · git 短 sha（当前运行的构建）"
                  >
                    v{health.version}
                    {buildInfo?.git_sha ? ` · ${buildInfo.git_sha}` : ""}
                  </span>
                </div>
              )}
            </div>

            {tab === "record" && (
              <RecordPanel
                busy={busy}
                onError={setError}
                onBusy={setBusy}
                onCopyToast={showCopyToast}
                hotkeyLabel={hotkeyLabel}
                onSaved={async () => {
                  await refreshSessions();
                  await refreshHealth();
                }}
                onLearnCandidates={(sessionId, baseline, cands, before, after) => {
                  setSessionLearn({ sessionId, baseline, candidates: cands });
                  setLearnBefore(before);
                  setLearnAfter(after);
                  setCandidates(cands);
                  if (cands.length > 0) setTab("learn");
                }}
              />
            )}

            {tab === "meeting" && (
              <MeetingPanel onError={setError} onNavigate={(t) => setTab(t)} />
            )}

            {tab === "overview" && (
              <Overview
                health={health}
                sessions={sessions}
                dictCount={dict.length}
                editLearning={editLearningObservability}
                busy={busy}
                onSeed={() =>
                  run("seed", async () => {
                    await api.seedDemoSession();
                    await refreshSessions();
                    await refreshHealth();
                  })
                }
                onGoto={(t) => setTab(t)}
              />
            )}

            {tab === "history" && (
              <HistoryPanel
                sessions={sessions}
                selected={selected}
                editFeedbackRevision={editFeedbackRevision}
                busy={busy}
                onSelect={setSelectedId}
                onRefresh={() => void refreshSessions()}
                onBusy={setBusy}
                onError={setError}
                onUpdated={(s) => {
                  setSessions((prev) => prev.map((x) => (x.id === s.id ? s : x)));
                  setSelectedId(s.id);
                }}
                onDelete={(id) =>
                  run("delete session", async () => {
                    await api.deleteSession(id);
                    if (selectedId === id) setSelectedId(null);
                    await refreshSessions();
                    await refreshHealth();
                  })
                }
              />
            )}

            {tab === "dictionary" && (
              <DictionaryPanel
                entries={dict}
                termInput={termInput}
                fromInput={fromInput}
                toInput={toInput}
                busy={busy}
                onTermInput={setTermInput}
                onFromInput={setFromInput}
                onToInput={setToInput}
                onAddTerm={() =>
                  run("add term", async () => {
                    await api.addTerm(termInput);
                    setTermInput("");
                    await refreshDict();
                    await refreshHealth();
                  })
                }
                onAddReplacement={() =>
                  run("add replacement", async () => {
                    await api.addReplacement(fromInput, toInput);
                    setFromInput("");
                    setToInput("");
                    await refreshDict();
                    await refreshHealth();
                  })
                }
                onDelete={(id) =>
                  run("delete entry", async () => {
                    await api.deleteDictionaryEntry(id);
                    await refreshDict();
                    await refreshHealth();
                  })
                }
              />
            )}

            {tab === "identity" && <IdentityPanel onError={setError} />}

            {tab === "learn" && (
              <LearnPanel
                before={learnBefore}
                after={learnAfter}
                candidates={candidates}
                sessionId={sessionLearn?.sessionId}
                busy={busy}
                onBefore={setLearnBefore}
                onAfter={setLearnAfter}
                onSuggest={() =>
                  run("process edit", async () => {
                    const res = await api.processEdit({
                      beforeText: learnBefore,
                      afterText: learnAfter,
                      sessionId: sessionLearn?.sessionId,
                      source: "manual",
                      recordEvent: true,
                    });
                    setCandidates(res.candidates);
                    if (res.autoPromoted?.length) {
                      await refreshDict();
                    }
                  })
                }
                onConfirm={(c) =>
                  run("confirm learn", async () => {
                    await api.confirmLearn({
                      kind: c.kind,
                      term: c.term ?? undefined,
                      fromText: c.from_text ?? undefined,
                      toText: c.to_text ?? undefined,
                      sessionId: sessionLearn?.sessionId,
                      beforeText: learnBefore,
                      afterText: learnAfter,
                      proposalId: c.proposal_id,
                    });
                    setCandidates((prev) =>
                      prev.filter(
                        (x) =>
                          !(
                            x.kind === c.kind &&
                            x.term === c.term &&
                            x.from_text === c.from_text &&
                            x.to_text === c.to_text
                          )
                      )
                    );
                    await refreshDict();
                    await refreshHealth();
                  })
                }
                onReject={(c) =>
                  run("reject learn", async () => {
                    if (c.proposal_id) {
                      await api.decideEditLearningProposal(c.proposal_id, "rejected");
                    }
                    setCandidates((prev) =>
                      prev.filter((candidate) => candidate !== c),
                    );
                  })
                }
              />
            )}

            {tab === "settings" && (
              <SettingsPanel
                busy={busy}
                onBusy={setBusy}
                onError={setError}
                onSaved={() => {
                  void refreshHealth();
                  void (async () => {
                    try {
                      const hk = await api.getHotkeyConfig();
                      setHotkeyEnabledUi(hk.enabled);
                      setHotkeyLabel(formatHotkeyLabel(hk.toggle));
                    } catch {
                      /* ignore */
                    }
                  })();
                }}
              />
            )}
          </div>
        </div>
      </div>

      <footer className="statusbar">
        <span
          className={`dot ${busy ? "busy" : health?.db_ok ? "ok" : "bad"}`}
          title={busy ? "busy" : health?.db_ok ? "db ok" : "db down"}
        />
        <span>
          {busy ? "处理中" : health?.db_ok ? "就绪" : "数据库不可用"}
        </span>
        <span className="sep">·</span>
        <span>
          ASR{" "}
          <strong>
            {health
              ? `${health.active_asr_label}${health.active_asr_ready ? "" : "（未就绪）"}`
              : "—"}
          </strong>
        </span>
        <span className="sep">·</span>
        <span>
          修正 <strong>{health?.corrector_label || "—"}</strong>
        </span>
        <span className="sep">·</span>
        {hotkeyEnabled ? (
          <span>
            热键 <span className="kbd">{hotkeyLabel}</span>
          </span>
        ) : (
          <span>热键关</span>
        )}
        <span style={{ flex: 1 }} />
        <span>
          {health ? `${health.session_count} 条已保存 · ${health.dictionary_count} 词条` : ""}
        </span>
      </footer>
    </div>
    </MeetingModelsProvider>
  );
}

// App-level, non-blocking prompt shown when the backend detects likely meeting
// audio activity. It never records on its own — the user must click "开始记录".
// Accessibility: labelled by its title, focus moves to the primary action on
// open, and Esc dismisses (same as clicking 忽略).
function DetectionPrompt({
  bundleId,
  appClass,
  displayName,
  onStart,
  onDismiss,
}: {
  bundleId: string;
  appClass: string;
  displayName: string;
  onStart: (captureSystemAudio: boolean) => void;
  onDismiss: () => void;
}) {
  const isBrowser = appClass === "browser";
  const startRef = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    startRef.current?.focus();
  }, []);
  return (
    <div
      className="detection-prompt"
      role="alertdialog"
      aria-labelledby="detection-prompt-title"
      aria-describedby="detection-prompt-sub"
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation();
          onDismiss();
        }
      }}
    >
      <div className="detection-prompt-body">
        <span id="detection-prompt-title" className="detection-prompt-title">
          检测到可能的会议
        </span>
        <span id="detection-prompt-sub" className="detection-prompt-sub">
          {isBrowser
            ? `${displayName || bundleId} 正在使用麦克风。macOS 只能按整个浏览器录音，无法区分标签页；其他标签页的音乐或视频也会被录入。`
            : `${displayName || bundleId} 正在使用麦克风。是否开始记录本次会议？`}
        </span>
      </div>
      <div className="detection-prompt-actions">
        {isBrowser ? (
          <>
            <button
              type="button"
              className="btn"
              ref={startRef}
              onClick={() => onStart(true)}
            >
              录制整个浏览器声音
            </button>
            <button type="button" className="btn ghost" onClick={() => onStart(false)}>
              只录麦克风
            </button>
          </>
        ) : (
          <button
            type="button"
            className="btn"
            ref={startRef}
            onClick={() => onStart(true)}
          >
            开始记录
          </button>
        )}
        <button type="button" className="btn ghost" onClick={onDismiss}>
          忽略
        </button>
      </div>
    </div>
  );
}

// Counterpart of DetectionPrompt for the end of a detected meeting: the
// triggering app's audio input has been gone long enough that the meeting
// looks over. Purely advisory — recording continues until the user clicks
// 停止录音 (继续录制 / Esc keeps it running and suppresses further
// suggestions for this recording). Focus lands on 继续录制 so a stray Enter
// never stops a recording the user wanted to keep.
function StopSuggestPrompt({
  bundleId,
  displayName,
  onStop,
  onKeep,
}: {
  bundleId: string;
  displayName: string;
  onStop: () => void;
  onKeep: () => void;
}) {
  const keepRef = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    keepRef.current?.focus();
  }, []);
  return (
    <div
      className="detection-prompt"
      role="alertdialog"
      aria-labelledby="stop-suggest-title"
      aria-describedby="stop-suggest-sub"
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation();
          onKeep();
        }
      }}
    >
      <div className="detection-prompt-body">
        <span id="stop-suggest-title" className="detection-prompt-title">
          会议似乎已结束
        </span>
        <span id="stop-suggest-sub" className="detection-prompt-sub">
          {displayName || bundleId} 已不再使用麦克风。是否停止录音？
        </span>
      </div>
      <div className="detection-prompt-actions">
        <button type="button" className="btn danger" onClick={onStop}>
          停止录音
        </button>
        <button type="button" className="btn ghost" ref={keepRef} onClick={onKeep}>
          继续录制
        </button>
      </div>
    </div>
  );
}

// Non-modal right-side notice for the auto-stop watchdogs (silence and
// max-duration). It never steals keyboard focus from the user's meeting app;
// the backend clock, not this display timer, decides whether the recording
// actually stops.
function AutoStopPrompt({
  title,
  promptId,
  deadlineMs,
  onStop,
  onKeep,
}: {
  title: string;
  promptId: string;
  deadlineMs: number;
  onStop: () => void;
  onKeep: () => void;
}) {
  const titleId = `meeting-auto-stop-title-${promptId}`;
  const subId = `meeting-auto-stop-sub-${promptId}`;
  const remainingNow = () => Math.max(0, Math.ceil((deadlineMs - Date.now()) / 1000));
  const [remaining, setRemaining] = useState(remainingNow);
  useEffect(() => {
    setRemaining(remainingNow());
    const timer = window.setInterval(() => setRemaining(remainingNow()), 250);
    return () => window.clearInterval(timer);
  }, [deadlineMs]);

  return (
    <div
      className="detection-prompt meeting-silence-prompt"
      role="alert"
      aria-live="assertive"
      aria-atomic="true"
      aria-labelledby={titleId}
      aria-describedby={subId}
    >
      <div className="detection-prompt-body">
        <span id={titleId} className="detection-prompt-title">
          {title}
        </span>
        <span id={subId} className="detection-prompt-sub">
          {remaining > 0
            ? `${remaining} 秒后将自动结束会议录音。`
            : "正在结束会议录音…"}
        </span>
      </div>
      <div className="detection-prompt-actions">
        <button type="button" className="btn danger" onClick={onStop}>
          立即结束
        </button>
        <button type="button" className="btn ghost" onClick={onKeep}>
          继续录制
        </button>
      </div>
    </div>
  );
}

function MicrophoneConsentDialog({
  onlineRecognition,
  providerLabel,
  onAllow,
  onCancel,
  onOpenSettings,
}: {
  onlineRecognition: boolean;
  providerLabel: string;
  onAllow: () => void;
  onCancel: () => void;
  onOpenSettings: () => void;
}) {
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    cancelRef.current?.focus();
  }, []);

  return (
    <div className="meeting-modal-overlay microphone-consent-overlay">
      <div
        ref={dialogRef}
        className="card meeting-confirm microphone-consent"
        role="dialog"
        aria-modal="true"
        aria-labelledby="microphone-consent-title"
        aria-describedby="microphone-consent-description"
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.stopPropagation();
            onCancel();
            return;
          }
          if (event.key === "Tab") {
            const controls = Array.from(
              dialogRef.current?.querySelectorAll<HTMLElement>(
                'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
              ) ?? [],
            );
            if (controls.length === 0) {
              event.preventDefault();
              return;
            }
            const first = controls[0];
            const last = controls[controls.length - 1];
            if (event.shiftKey && document.activeElement === first) {
              event.preventDefault();
              last.focus();
            } else if (!event.shiftKey && document.activeElement === last) {
              event.preventDefault();
              first.focus();
            }
          }
        }}
      >
        <h2 id="microphone-consent-title" className="meeting-confirm-title">
          允许 Lumen 使用麦克风？
        </h2>
        <div id="microphone-consent-description" className="microphone-consent-copy">
          <p>Lumen 只会在你点击录音或主动使用录音热键时开启麦克风。</p>
          <p>
            {onlineRecognition
              ? `当前选择“${providerLabel}”，录音将发送给你配置的在线服务进行语音识别。`
              : `当前选择“${providerLabel}”，语音识别在本机完成；如果另行启用在线文本修正，转写文字可能发送给所配置的服务。`}
          </p>
          <p>
            Windows 桌面应用不一定显示单独的系统授权弹窗。你可以随时到系统麦克风设置中关闭访问。
          </p>
        </div>
        <div className="meeting-confirm-actions microphone-consent-actions">
          <button type="button" className="btn" onClick={onAllow}>
            允许并开始录音
          </button>
          <button type="button" className="btn ghost" onClick={onOpenSettings}>
            打开 Windows 设置
          </button>
          <button type="button" className="btn ghost" ref={cancelRef} onClick={onCancel}>
            取消
          </button>
        </div>
      </div>
    </div>
  );
}

function RecordPanel({
  busy,
  onError,
  onBusy,
  onSaved,
  onLearnCandidates,
  onCopyToast,
  hotkeyLabel,
}: {
  busy: boolean;
  onError: (e: string | null) => void;
  onBusy: (b: boolean) => void;
  onSaved: () => Promise<void>;
  onCopyToast: (text: string) => void;
  hotkeyLabel: string;
  onLearnCandidates: (
    sessionId: string,
    baseline: string,
    candidates: LearnCandidate[],
    before: string,
    after: string
  ) => void;
}) {
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [device, setDevice] = useState<string>("");
  const [status, setStatus] = useState<AsrStatus | null>(null);
  const [recording, setRecording] = useState(false);
  const [seconds, setSeconds] = useState(0);
  const [text, setText] = useState("");
  const [asrText, setAsrText] = useState("");
  const [meta, setMeta] = useState<string>("");
  const [baseline, setBaseline] = useState("");
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [liveCandidates, setLiveCandidates] = useState<LearnCandidate[]>([]);
  const [startError, setStartError] = useState<string | null>(null);
  const [microphoneNoticeAcknowledged, setMicrophoneNoticeAcknowledged] = useState(
    () => {
      if (!IS_WINDOWS) return true;
      try {
        return hasAcknowledgedWindowsMicrophoneNotice(window.localStorage);
      } catch {
        return false;
      }
    },
  );
  const [showMicrophoneConsent, setShowMicrophoneConsent] = useState(false);
  const microphoneConsentTriggerRef = useRef<HTMLElement | null>(null);
  const onSavedRef = useRef(onSaved);
  onSavedRef.current = onSaved;

  const refreshStatus = useCallback(async () => {
    try {
      const s = await api.getAsrStatus();
      setStatus(s);
      setRecording(s.recording);
      return s;
    } catch (e) {
      onError(String(e));
      return null;
    }
  }, [onError]);

  useEffect(() => {
    void (async () => {
      try {
        const [list, preferred] = await Promise.all([
          api.listAudioDevices(),
          api.getAudioDevice(),
        ]);
        setDevices(list);
        const selected = chooseAudioDevice(list, preferred);
        if (selected) {
          setDevice(selected);
          if (selected !== preferred) {
            await api.setAudioDevice(selected);
          }
        }
      } catch (e) {
        onError(String(e));
      }
      await refreshStatus();
    })();
  }, [onError, refreshStatus]);

  useEffect(() => {
    if (!status?.qwenRuntimeChecking) return;
    const timer = window.setInterval(
      () =>
        void (async () => {
          const next = await refreshStatus();
          if (next && !next.qwenRuntimeChecking) {
            await onSavedRef.current();
          }
        })(),
      500,
    );
    return () => window.clearInterval(timer);
  }, [refreshStatus, status?.qwenRuntimeChecking]);

  // Hotkey dictation done → fill result + baseline for learning
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent).detail as {
        text?: string;
        asrText?: string;
        correctedText?: string;
        session?: SessionRecord;
        asrEngine?: string;
        correctorEngine?: string;
        fallbackReason?: string | null;
      };
      if (!detail) return;
      const finalText = detail.text || detail.correctedText || "";
      setText(finalText);
      setAsrText(detail.asrText || "");
      setBaseline(finalText);
      setSessionId(detail.session?.id ?? null);
      setLiveCandidates([]);
      setMeta(
        detail.fallbackReason
          ? `hotkey · ASR ${formatAsrEngineLabel(detail.asrEngine) || "?"} · ${correctorFallbackNotice(detail.fallbackReason)}`
          : `hotkey · ASR ${formatAsrEngineLabel(detail.asrEngine) || "?"} · ${detail.correctorEngine || ""}`
      );
    };
    window.addEventListener("lumen-dictation-done", handler);
    return () => window.removeEventListener("lumen-dictation-done", handler);
  }, []);

  useEffect(() => {
    if (!recording) {
      setSeconds(0);
      return;
    }
    const t = setInterval(() => setSeconds((s) => s + 1), 1000);
    return () => clearInterval(t);
  }, [recording]);

  async function onDeviceChange(name: string) {
    setDevice(name);
    try {
      await api.setAudioDevice(name);
    } catch (e) {
      onError(String(e));
    }
  }

  async function onEngineChange(providerId: string) {
    onBusy(true);
    onError(null);
    try {
      // Saving the provider also switches the matching local engine atomically.
      await api.saveAsrServiceConfig({ provider: providerId });
      await refreshStatus();
      await onSaved();
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  }

  async function startRecording() {
    onBusy(true);
    onError(null);
    setStartError(null);
    setText("");
    setAsrText("");
    setMeta("");
    try {
      if (IS_WINDOWS) {
        const currentPermission = await api.getPermissionStatus();
        if (!currentPermission.canRecord) {
          const requestedPermission = await api.requestMicrophoneAccess();
          if (!requestedPermission.canRecord) {
            throw new Error(
              "麦克风尚未就绪。请允许 Windows 麦克风访问后点击“重试录音”，或到「设置 → 权限」再次请求。",
            );
          }
        }
      }
      await api.startRecording();
      setRecording(true);
      await refreshStatus();
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      setStartError(message);
      onError(message);
    } finally {
      onBusy(false);
    }
  }

  async function start() {
    if (IS_WINDOWS && !microphoneNoticeAcknowledged) {
      microphoneConsentTriggerRef.current =
        document.activeElement instanceof HTMLElement ? document.activeElement : null;
      setShowMicrophoneConsent(true);
      return;
    }
    await startRecording();
  }

  function cancelMicrophoneConsent() {
    setShowMicrophoneConsent(false);
    window.requestAnimationFrame(() => microphoneConsentTriggerRef.current?.focus());
  }

  function allowMicrophoneAndStart() {
    try {
      acknowledgeWindowsMicrophoneNotice(window.localStorage);
    } catch {
      // Keep the acknowledgement for this app session when storage is blocked.
    }
    setMicrophoneNoticeAcknowledged(true);
    setShowMicrophoneConsent(false);
    void startRecording();
  }

  async function stop() {
    onBusy(true);
    onError(null);
    try {
      const out = await api.stopAndTranscribe(true);
      setRecording(false);
      setAsrText(out.asrText);
      setText(out.text);
      setBaseline(out.text);
      setSessionId(out.session?.id ?? null);
      setLiveCandidates([]);
      const copied = copyToastLabel(out.insertNotice);
      if (copied) onCopyToast(copied);
      const corr = out.modelApplied
        ? `corrector ${out.correctorEngine}`
        : `${correctorFallbackNotice(out.fallbackReason)} (${out.correctorEngine})`;
      setMeta(
        `ASR ${formatAsrEngineLabel(out.asrEngine) || out.asrEngine} · ${corr} · ${(out.durationMs / 1000).toFixed(1)}s · ${out.numSamples} samples`
      );
      await onSaved();
      await refreshStatus();
    } catch (e) {
      setRecording(false);
      onError(String(e));
      try {
        await api.cancelRecording();
      } catch {
        /* ignore */
      }
    } finally {
      onBusy(false);
    }
  }

  async function onTextBlurLearn() {
    if (!baseline || !text.trim() || text.trim() === baseline.trim()) {
      setLiveCandidates([]);
      return;
    }
    try {
      const res = await api.processEdit({
        beforeText: baseline,
        afterText: text,
        sessionId: sessionId ?? undefined,
        source: "pre_insert_ui",
        recordEvent: true,
      });
      setLiveCandidates(res.candidates);
      if (res.candidates.length > 0 && sessionId) {
        onLearnCandidates(sessionId, baseline, res.candidates, baseline, text);
      }
    } catch {
      /* ignore soft learn failures */
    }
  }

  async function reCorrect() {
    if (!text.trim() && !asrText.trim()) return;
    onBusy(true);
    onError(null);
    try {
      const src = asrText || text;
      const out = await api.correctText(src);
      setText(out.text);
      setMeta(
        (meta ? meta + " · " : "") +
          (out.modelApplied
            ? `re-correct ${out.correctorEngine}`
            : `re-correct fallback (${out.correctorEngine})`)
      );
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  }

  async function insertNow() {
    if (!text.trim()) return;
    onBusy(true);
    onError(null);
    try {
      const out = await api.insertText(text);
      setMeta(
        (meta ? meta + " · " : "") +
          `insert ${out.strategy}${out.restoredClipboard ? " (clipboard restored)" : ""}`
      );
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  }

  async function cancel() {
    onBusy(true);
    try {
      await api.cancelRecording();
      setRecording(false);
      await refreshStatus();
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  }

  // Synced with 设置 → 语音识别 (config.asr.provider).
  const provider =
    status?.provider ||
    (status?.engine === "qwen"
      ? "local_qwen"
      : status?.engine === "whisper"
        ? "local_whisper"
        : "local_sensevoice");
  const ready = status?.activeReady ?? false;
  const isLocal =
    provider.startsWith("local") ||
    provider === "sensevoice" ||
    provider === "qwen" ||
    provider === "qwen3_asr" ||
    provider === "whisper";

  return (
    <>
      {showMicrophoneConsent && (
        <MicrophoneConsentDialog
          onlineRecognition={!isLocal}
          providerLabel={status?.providerLabel || provider}
          onAllow={allowMicrophoneAndStart}
          onCancel={cancelMicrophoneConsent}
          onOpenSettings={() => void api.openMicrophoneSettings()}
        />
      )}
      <section className="card">
        <h2>录音转写</h2>
        <p className="muted-text">
          引擎与「设置 → 语音识别」为同一配置。全局热键{" "}
          <span className="kbd">{hotkeyLabel}</span>{" "}
          在任意 App 按住说话。当前：
          <strong> {status?.providerLabel || provider}</strong>
        </p>
        <div className="form-row">
          <label className="form-label">
            设备
          </label>
          <select
            className="input"
            value={device}
            disabled={recording || busy}
            onChange={(e) => void onDeviceChange(e.target.value)}
          >
            {devices.map((d) => (
              <option key={d.name} value={d.name}>
                {d.name}
                {d.is_default ? " (默认)" : ""}
              </option>
            ))}
          </select>
        </div>
        <div className="form-row">
          <label className="form-label">
            ASR
          </label>
          <select
            className="input"
            value={provider}
            disabled={recording || busy}
            onChange={(e) => void onEngineChange(e.target.value)}
          >
            <option value="local_sensevoice">
              本地 SenseVoice {status?.sensevoice.ready ? "✓" : "（模型未就绪）"}
            </option>
            <option value="local_qwen">
              本地 Qwen3-ASR 0.6B 8-bit（高准确率）{" "}
              {status?.qwenRuntimeChecking
                ? "（正在检查运行环境…）"
                : status?.qwen.ready && status?.qwenRuntimeReady
                  ? "✓"
                  : "（模型或运行环境未就绪）"}
            </option>
            <option value="local_whisper">
              本地 Whisper {status?.whisper.ready ? "✓" : "（模型未就绪）"}
            </option>
            <option value="openai_audio">OpenAI Audio / Whisper（在线）</option>
            <option value="aliyun_qwen" disabled>
              阿里 Qwen ASR（预设，流式待接）
            </option>
            <option value="volcengine" disabled>
              火山 ASR（预设，待接）
            </option>
            <option value="soniox" disabled>
              Soniox（预设，待接）
            </option>
            <option value="stepfun" disabled>
              阶跃 ASR（预设，待接）
            </option>
            <option value="mimo" disabled>
              小米 MiMo ASR（预设，待接）
            </option>
          </select>
        </div>
        {status && isLocal && (
          <p className="muted-text" style={{ fontSize: "0.85rem" }}>
            本地模型目录：
            <code>
              {provider.includes("whisper")
                ? status.whisper.model_dir
                : provider.includes("qwen")
                  ? status.qwen.model_dir
                  : status.sensevoice.model_dir}
            </code>
          </p>
        )}
        {provider === "openai_audio" && !ready && (
          <p className="muted-text" style={{ fontSize: "0.85rem" }}>
            请到「设置 → 语音识别」填写 OpenAI API Key 并保存，再回到此处录音。
          </p>
        )}
        <div className="actions">
          {!recording ? (
            <button
              type="button"
              className="btn"
              disabled={busy || !ready}
              onClick={() => void start()}
            >
              开始录音
            </button>
          ) : (
            <>
              <button
                type="button"
                className="btn"
                disabled={busy}
                onClick={() => void stop()}
              >
                停止并转写 ({seconds}s)
              </button>
              <button
                type="button"
                className="btn ghost"
                disabled={busy}
                onClick={() => void cancel()}
              >
                取消
              </button>
            </>
          )}
        </div>
        {!recording && startError && (
          <div className="recording-retry" role="alert">
            <span>{startError}</span>
            <button
              type="button"
              className="btn small"
              disabled={busy || !ready}
              onClick={() => void start()}
            >
              重试录音
            </button>
          </div>
        )}
        {!ready && isLocal && (
          <p className="muted-text" style={{ marginTop: 12 }}>
            当前本地引擎未就绪。
            {provider.includes("qwen")
              ? "请到「设置 → 语音识别」选择 Qwen MLX 模型目录，并填写包含 mlx_qwen3_asr 的 Python 可执行文件。"
              : provider.includes("whisper")
                ? "请到「设置 → 语音识别」选择可用的 Whisper 模型目录。"
                : "请将 SenseVoice 的 model.int8.onnx 与 tokens.txt 放到模型目录，或到「设置 → 语音识别」切换其它 ASR。"}
          </p>
        )}
      </section>

      <section className="card">
        <h2>转写结果</h2>
        {meta && <p className="muted-text">{meta}</p>}
        {asrText && asrText !== text && (
          <div className="field-block">
            <div className="field-label">ASR 原文</div>
            <pre className="field-value">{asrText}</pre>
          </div>
        )}
        <div className="field-label" style={{ marginBottom: 6 }}>
          最终文本（已修正）
        </div>
        <textarea
          className="textarea"
          rows={8}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onBlur={() => void onTextBlurLearn()}
          placeholder={recording ? "录音中…" : "转写文本将显示在这里"}
        />
        {liveCandidates.length > 0 && (
          <div className="field-block" style={{ marginTop: 10 }}>
            <div className="field-label">检测到编辑 → 可学习</div>
            <ul className="list">
              {liveCandidates.map((c, i) => (
                <li key={i} className="candidate">
                  <div>
                    <span className="chip">{c.kind}</span>{" "}
                    {c.kind === "term"
                      ? c.term
                      : `${c.from_text ?? ""} → ${c.to_text ?? ""}`}
                  </div>
                  <button
                    type="button"
                    className="btn small"
                    disabled={busy}
                    onClick={() =>
                      void (async () => {
                        onBusy(true);
                        try {
                          await api.confirmLearn({
                            kind: c.kind,
                            term: c.term ?? undefined,
                            fromText: c.from_text ?? undefined,
                            toText: c.to_text ?? undefined,
                            sessionId: sessionId ?? undefined,
                            beforeText: baseline,
                            afterText: text,
                          });
                          setLiveCandidates((prev) => prev.filter((_, j) => j !== i));
                          await onSaved();
                        } catch (e) {
                          onError(String(e));
                        } finally {
                          onBusy(false);
                        }
                      })()
                    }
                  >
                    加入词典
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}
        <div className="actions">
          <button
            type="button"
            className="btn ghost"
            disabled={busy || (!text.trim() && !asrText.trim())}
            onClick={() => void reCorrect()}
          >
            重新 AI 修正
          </button>
          <button
            type="button"
            className="btn"
            disabled={busy || !text.trim()}
            onClick={() => void insertNow()}
          >
            插入到当前应用
          </button>
          <button
            type="button"
            className="btn ghost"
            disabled={busy || !baseline || text.trim() === baseline.trim()}
            onClick={() => void onTextBlurLearn()}
          >
            从编辑生成候选
          </button>
        </div>
        <p className="muted-text" style={{ marginTop: 8, fontSize: "0.85rem" }}>
          改字后失焦会分析词典候选。插入需要辅助功能；粘贴后系统可监听目标框再学习（设置中开关）。
        </p>
      </section>
    </>
  );
}

/** Short human label for the current translation style, used in the hint line. */
function styleLabel(preset: string, custom: string): string {
  switch (preset) {
    case "formal":
      return "正式风格";
    case "casual":
      return "口语风格";
    case "social":
      return "社媒风格";
    case "custom":
      return custom.trim() ? `自定义风格：${custom.trim()}` : "自定义风格";
    default:
      return "忠实翻译";
  }
}

function SettingsPanel({
  busy,
  onBusy,
  onError,
  onSaved,
}: {
  busy: boolean;
  onBusy: (b: boolean) => void;
  onError: (e: string | null) => void;
  onSaved: () => void;
}) {
  const [cfg, setCfg] = useState<CorrectorStatus | null>(null);
  const [enabled, setEnabled] = useState(true);
  const [provider, setProvider] = useState("ollama");
  const [baseUrl, setBaseUrl] = useState("http://127.0.0.1:11434/v1");
  const [model, setModel] = useState("qwen3.5:9b");
  const [apiKey, setApiKey] = useState("");
  const [timeoutSecs, setTimeoutSecs] = useState(60);
  const [llmPresets, setLlmPresets] = useState<
    {
      id: string;
      label: string;
      kind: string;
      baseUrl: string;
      defaultModel: string;
      models: string[];
      needsApiKey: boolean;
      notes: string;
    }[]
  >([]);
  const [asrPresets, setAsrPresets] = useState<
    {
      id: string;
      label: string;
      kind: string;
      baseUrl: string;
      defaultModel: string;
      models: string[];
      needsApiKey: boolean;
      status: string;
      notes: string;
    }[]
  >([]);
  const [asrProvider, setAsrProvider] = useState("local_sensevoice");
  const [asrRuntimePath, setAsrRuntimePath] = useState("");
  const [qwenShadowEnabled, setQwenShadowEnabled] = useState(false);
  const [asrBaseUrl, setAsrBaseUrl] = useState("");
  const [asrModel, setAsrModel] = useState("");
  const [asrApiKey, setAsrApiKey] = useState("");
  const [asrLanguage, setAsrLanguage] = useState("");
  const [asrHasKey, setAsrHasKey] = useState(false);
  const [asrModels, setAsrModels] = useState<AsrModelStatus | null>(null);
  const [asrCustomPath, setAsrCustomPath] = useState("");
  const [cleanup, setCleanup] = useState("medium");
  const cleanupDrafts = useRef<Record<string, string>>({});
  const [style, setStyle] = useState("neutral");
  const [casing, setCasing] = useState("sentence");
  const [punctuation, setPunctuation] = useState("standard");
  const [polish, setPolish] = useState<string[]>([]);
  const [customEnabled, setCustomEnabled] = useState(false);
  const [customInstruction, setCustomInstruction] = useState("");
  const [useCapturedContext, setUseCapturedContext] = useState(false);
  const [probe, setProbe] = useState<string>("");
  const [perm, setPerm] = useState<import("./api").PermissionStatus | null>(null);
  const [autoInsert, setAutoInsert] = useState(true);
  const [injectMode, setInjectMode] = useState("auto");
  const [preserveClip, setPreserveClip] = useState(true);
  const [hotkeyEnabled, setHotkeyEnabled] = useState(true);
  const [hotkeyToggle, setHotkeyToggle] = useState("Alt+Space");
  const [showCapsule, setShowCapsule] = useState(true);
  const [hotkeyMode, setHotkeyMode] = useState("hold");
  const [intents, setIntents] = useState<import("./api").HotkeyIntent[]>([]);
  const [hotkeyRegisterNote, setHotkeyRegisterNote] = useState("");
  const [learning, setLearning] = useState<LearningConfig | null>(null);
  const [autoPromote, setAutoPromote] = useState(false);
  const [promoteN, setPromoteN] = useState(3);
  const [postPaste, setPostPaste] = useState(true);
  const [postPasteSecs, setPostPasteSecs] = useState(20);
  const [persistEditEvidenceText, setPersistEditEvidenceText] = useState(false);
  const [soundsEnabledUi, setSoundsEnabledUi] = useState(true);
  const [savingSounds, setSavingSounds] = useState(false);
  const [detectionEnabled, setDetectionEnabled] = useState(false);
  const [detectionCapable, setDetectionCapable] = useState(false);
  const [meetingApps, setMeetingApps] = useState<MeetingAppCatalog | null>(null);
  const [meetingAppsSaving, setMeetingAppsSaving] = useState(false);
  // Meeting watchdog settings (silence auto-stop minutes, max-duration cap,
  // calendar-end reminder).
  const [silenceAutoStopMinutes, setSilenceAutoStopMinutes] = useState(15);
  const [maxDurationMinutes, setMaxDurationMinutes] = useState(480);
  const [calendarEndReminder, setCalendarEndReminder] = useState(true);
  // Debounce + latest-wins so rapid edits to either field don't race as
  // independent writes (each call persists both fields, so an older in-flight
  // write could otherwise clobber the newer one).
  const watchdogSaveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const saveWatchdog = useCallback(
    (minutes: number, maxDuration: number, reminder: boolean) => {
      if (watchdogSaveTimer.current) clearTimeout(watchdogSaveTimer.current);
      watchdogSaveTimer.current = setTimeout(() => {
        void api
          .setMeetingWatchdogConfig({
            silenceAutoStopMinutes: minutes,
            maxDurationMinutes: maxDuration,
            calendarEndReminder: reminder,
          })
          .catch((err) => onError(String(err)));
      }, 400);
    },
    [onError],
  );
  // null = still loading, "error" = lookup failed (never stuck on loading).
  const [buildInfo, setBuildInfo] = useState<BuildInfo | "error" | null>(null);
  useEffect(() => {
    void api
      .buildInfo()
      .then(setBuildInfo)
      .catch(() => setBuildInfo("error"));
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const [c, presets, asrP, asrC, asrStatus] = await Promise.all([
          api.getCorrectorConfig(),
          api.listLlmPresets(),
          api.listAsrPresets(),
          api.getAsrServiceConfig(),
          api.checkAsrModelStatus(),
        ]);
        setCfg(c);
        setLlmPresets(presets);
        setAsrPresets(asrP);
        setAsrProvider(asrC.provider);
        setAsrRuntimePath(asrC.runtimePath || "");
        setQwenShadowEnabled(asrC.qwenShadowEnabled);
        setAsrBaseUrl(asrC.baseUrl);
        setAsrModel(asrC.model);
        setAsrLanguage(asrC.language || "");
        setAsrHasKey(asrC.hasApiKey);
        setAsrModels(asrStatus);
        setAsrCustomPath(asrStatus.activeModelDir || "");
        setEnabled(c.enabled);
        setProvider(c.provider);
        setBaseUrl(c.baseUrl);
        setModel(c.model);
        setTimeoutSecs(c.timeoutSecs);
        setCleanup(c.cleanup || "medium");
        setStyle(c.style || "neutral");
        setCasing(c.casing || "sentence");
        setPunctuation(c.punctuation || "standard");
        setPolish(c.polish || []);
        setCustomEnabled(!!c.customEnabled);
        setCustomInstruction(c.customInstruction || "");
        setUseCapturedContext(c.useCapturedContext);
        const p = await api.getPermissionStatus();
        setPerm(p);
        const inj = await api.getInjectConfig();
        setAutoInsert(inj.autoInsert);
        setInjectMode(inj.mode);
        setPreserveClip(inj.preserveClipboard);
        const hk = await api.getHotkeyConfig();
        setHotkeyEnabled(hk.enabled);
        setHotkeyToggle(hk.toggle);
        setShowCapsule(hk.showCapsule);
        setHotkeyMode(hk.mode || "hold");
        setIntents(hk.intents || []);
        setHotkeyRegisterNote(hk.registerNote || "");
        const ln = await api.getLearningConfig();
        setLearning(ln);
        setAutoPromote(ln.autoPromote);
        setPromoteN(ln.autoPromoteThreshold);
        setPostPaste(ln.postPasteCapture);
        setPostPasteSecs(ln.postPasteSeconds);
        setPersistEditEvidenceText(ln.persistEditEvidenceText);
        try {
          const ui = await api.getUiConfig();
          setSoundsEnabledUi(ui.sounds);
          setSoundsEnabled(ui.sounds);
        } catch {
          /* ui config is best-effort */
        }
        try {
          const det = await api.getMeetingDetection();
          setDetectionEnabled(det.enabled);
          setDetectionCapable(det.capabilityAvailable);
        } catch {
          /* detection status is best-effort */
        }
        try {
          setMeetingApps(await api.getMeetingAppCatalog());
        } catch {
          /* catalog errors are surfaced when the user explicitly reloads/saves */
        }
        try {
          const wd = await api.getMeetingWatchdogConfig();
          setSilenceAutoStopMinutes(wd.silenceAutoStopMinutes);
          setMaxDurationMinutes(wd.maxDurationMinutes);
          setCalendarEndReminder(wd.calendarEndReminder);
        } catch {
          /* watchdog settings are best-effort */
        }
      } catch (e) {
        onError(String(e));
      }
    })();
  }, [onError]);

  async function refreshActiveCleanupProfile() {
    if (cfg?.cleanupProfile && cleanup !== (cfg.cleanup || "medium")) {
      cleanupDrafts.current[cfg.cleanupProfile] = cleanup;
    }
    const activeCorrector = await api.getCorrectorConfig();
    setCfg(activeCorrector);
    const activeProfile = activeCorrector.cleanupProfile || "default";
    setCleanup(
      cleanupDrafts.current[activeProfile] || activeCorrector.cleanup || "medium",
    );
  }

  async function saveMeetingApps() {
    if (!meetingApps) return;
    setMeetingAppsSaving(true);
    onError(null);
    try {
      const sanitized: MeetingAppCatalog = {
        ...meetingApps,
        applications: meetingApps.applications.map((entry) => ({
          ...entry,
          name: entry.name.trim(),
          bundle_ids: entry.bundle_ids.map((id) => id.trim()).filter(Boolean),
        })),
      };
      setMeetingApps(await api.saveMeetingAppCatalog(sanitized));
    } catch (error) {
      onError(`保存会议应用配置失败：${String(error)}`);
    } finally {
      setMeetingAppsSaving(false);
    }
  }

  async function save() {
    if (!cfg?.cleanupProfile) {
      onError("整理配置仍在加载，请稍后再保存。");
      return;
    }
    onBusy(true);
    onError(null);
    try {
      const input: Parameters<typeof api.saveCorrectorConfig>[0] = {
        enabled,
        useCapturedContext,
        provider,
        baseUrl,
        model,
        timeoutSecs,
        cleanup,
        cleanupProfile: cfg.cleanupProfile,
        style,
        casing,
        punctuation,
        polish,
        customEnabled,
        customInstruction,
      };
      if (apiKey.trim()) {
        input.apiKey = apiKey.trim();
      }
      const c = await api.saveCorrectorConfig(input);
      setCfg(c);
      delete cleanupDrafts.current[c.cleanupProfile || "default"];
      setCleanup(c.cleanup || cleanup);
      setStyle(c.style || style);
      setCasing(c.casing || casing);
      setPunctuation(c.punctuation || punctuation);
      setPolish(c.polish || polish);
      setCustomEnabled(!!c.customEnabled);
      setCustomInstruction(c.customInstruction || "");
      setUseCapturedContext(c.useCapturedContext);
      setApiKey("");
      onSaved();
      setProbe("已保存");
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  }

  async function probeCorrect() {
    onBusy(true);
    onError(null);
    setProbe("");
    try {
      const out = await api.correctText("你好  世界 用脱肯鉴权");
      setProbe(
        `${out.modelApplied ? "模型已应用" : "回退(预处理)"} · ${out.correctorEngine}\n→ ${out.text}`
      );
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  }

  async function saveInject() {
    onBusy(true);
    onError(null);
    try {
      await api.saveInjectConfig({
        mode: injectMode,
        preserveClipboard: preserveClip,
        autoInsert,
      });
      onSaved();
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  }

  async function refreshPerm() {
    try {
      setPerm(await api.pollPermissions());
    } catch (e) {
      onError(String(e));
    }
  }

  // Poll AX while settings tab is open so toggle flips update live.
  useEffect(() => {
    const id = window.setInterval(() => void refreshPerm(), 2000);
    return () => window.clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <>
      <section className="card settings-section">
        <h2>权限</h2>
        {perm ? (
          <>
            <div className="perm-list">
              <div
                className={`perm-row ${
                  perm.microphone === "granted"
                    ? "ok"
                    : IS_WINDOWS && perm.microphone === "not_determined"
                      ? ""
                      : "bad"
                }`}
              >
                <span className="perm-status">
                  <span
                    className={`perm-dot ${
                      perm.microphone === "granted"
                        ? "ok"
                        : IS_WINDOWS && perm.microphone === "not_determined"
                          ? ""
                          : "bad"
                    }`}
                    aria-hidden
                  />
                  <span className="perm-status-text">
                    <span className="perm-name">
                      麦克风
                      <span className="perm-badge">
                        {perm.microphone === "granted"
                          ? "已授权"
                          : perm.microphone === "denied"
                            ? "未授权"
                            : perm.microphone === "restricted"
                              ? "受限制"
                              : IS_WINDOWS
                                ? "未检查"
                                : "未授权"}
                      </span>
                    </span>
                    <span className="perm-state">
                      {perm.microphone === "granted"
                        ? "可以录音"
                        : perm.microphone === "denied"
                          ? "已被拒绝 — 到系统设置里打开"
                          : perm.microphone === "restricted"
                            ? "被系统策略限制"
                            : IS_WINDOWS
                              ? "尚未确认 — 点右侧检查，必要时 Windows 会显示授权提示"
                              : "尚未授权 — 点右侧请求，会弹系统授权窗"}
                    </span>
                  </span>
                </span>
                <button
                  type="button"
                  className={`btn small ${perm.microphone === "granted" ? "ghost" : ""}`}
                  disabled={busy}
                  onClick={() =>
                    void (async () => {
                      onBusy(true);
                      onError(null);
                      try {
                        setPerm(await api.requestMicrophoneAccess());
                        onSaved();
                      } catch (e) {
                        onError(String(e));
                      } finally {
                        onBusy(false);
                      }
                    })()
                  }
                >
                  {IS_WINDOWS
                    ? perm.microphone === "granted"
                      ? "测试麦克风"
                      : "检查麦克风"
                    : perm.microphone === "granted"
                      ? "重新检查"
                      : "请求麦克风"}
                </button>
              </div>

              {IS_WINDOWS ? (
                <div className="perm-row ok">
                  <span className="perm-status">
                    <span className="perm-dot ok" aria-hidden />
                    <span className="perm-status-text">
                      <span className="perm-name">
                        Windows 输出模式
                        <span className="perm-badge">自动插入</span>
                      </span>
                      <span className="perm-state">
                        转写完成后会写入当前窗口。失败时复制到剪贴板，并在胶囊说明原因。
                      </span>
                    </span>
                  </span>
                </div>
              ) : (
              <div className={`perm-row ${perm.accessibilityTrusted ? "ok" : "bad"}`}>
                <span className="perm-status">
                  <span
                    className={`perm-dot ${perm.accessibilityTrusted ? "ok" : "bad"}`}
                    aria-hidden
                  />
                  <span className="perm-status-text">
                    <span className="perm-name">
                      辅助功能
                      <span className="perm-badge">
                        {perm.accessibilityTrusted ? "已开启" : "未开启"}
                      </span>
                    </span>
                    <span className="perm-state">
                      {perm.accessibilityTrusted
                        ? "可直接把文本插入到其它应用"
                        : "未开启只能复制到剪贴板 — 在系统设置里打开当前进程的开关"}
                    </span>
                  </span>
                </span>
                <button
                  type="button"
                  className={`btn small ${perm.accessibilityTrusted ? "ghost" : ""}`}
                  disabled={busy}
                  onClick={() =>
                    void (async () => {
                      onBusy(true);
                      try {
                        setPerm(await api.requestAccessibilityAccess());
                        onSaved();
                      } catch (e) {
                        onError(String(e));
                      } finally {
                        onBusy(false);
                      }
                    })()
                  }
                >
                  {perm.accessibilityTrusted ? "重新检查" : "打开并拖入"}
                </button>
              </div>
              )}
            </div>

            {!IS_WINDOWS && <details className="settings-help perm-details">
              <summary>权限如何检测 · 技术细节</summary>
              <p className="muted-text" style={{ margin: "10px 0" }}>
                麦克风走系统弹窗授权。辅助功能系统<strong>不会</strong>弹窗，必须在「系统设置 →
                隐私与安全性 → 辅助功能」里打开<strong>当前进程</strong>的开关；检测用的是系统 API
                <code>AXIsProcessTrusted</code>，不是猜的。
              </p>
              <dl className="meta" style={{ margin: 0 }}>
                <dt>系统列表中的名称</dt>
                <dd>
                  <code>{perm.settingsListName || perm.processHint}</code>
                  {perm.processHint && perm.settingsListName !== perm.processHint ? (
                    <span className="muted-text"> （可执行文件名 {perm.processHint}）</span>
                  ) : null}
                </dd>
                <dt>完整路径</dt>
                <dd style={{ wordBreak: "break-all" }}>
                  <code>{perm.processPath}</code>
                </dd>
                <dt>代码签名</dt>
                <dd>
                  <code>{perm.codesignKind || "unknown"}</code>
                  {perm.codesignIdentifier ? (
                    <>
                      {" · "}
                      <code style={{ wordBreak: "break-all" }}>{perm.codesignIdentifier}</code>
                    </>
                  ) : null}
                </dd>
              </dl>
            </details>}
            {!IS_WINDOWS && !perm.accessibilityTrusted && (
              <div className="ax-recovery" style={{ marginTop: 12 }}>
                <p className="muted-text" style={{ marginBottom: 8 }}>
                  <strong>为什么开关开了仍显示「需要开启」？</strong>
                  多半不是检测坏了，而是开错了身份：macOS
                  按<strong>代码签名指纹</strong>记权限，不是按产品名。列表里出现两个
                  「Lumen ASR」很常见——分别对应开发版二进制和正式 .app，或两次不同的 adhoc
                  编译。打开其中任意一个旧条目，对<strong>当前这份</strong>进程无效。
                </p>
                <ol className="muted-text" style={{ margin: "0 0 8px 1.2em", lineHeight: 1.55 }}>
                  <li>完全退出 Lumen（菜单退出或 Activity Monitor 结束，不要只关窗口）。</li>
                  <li>
                    系统设置 → 辅助功能 → 用「−」删掉所有 Lumen / lumen-asr-desktop 相关项。
                  </li>
                  <li>
                    重新打开<strong>本程序</strong>，点下方「打开辅助功能设置」，只打开
                    <strong>新出现</strong>、且对应路径与上面一致的那一项（名称通常是「
                    {perm.settingsListName || "Lumen ASR"}」）。
                  </li>
                  <li>
                    再<strong>完全退出并重开</strong>一次，再点「刷新状态」。很多机器开关后不即时生效。
                  </li>
                </ol>
                {perm.codesignAdhoc && (
                  <p className="muted-text" style={{ marginBottom: 0 }}>
                    当前是 <strong>adhoc 未正式签名</strong> 构建：每次重新{" "}
                    <code>tauri build</code> 指纹会变，辅助功能往往要按上面步骤重做一遍。正式发版应用
                    Developer ID 签名后，同一 Team 身份会稳定得多。
                  </p>
                )}
              </div>
            )}
          </>
        ) : (
          <p className="muted-text">正在读取权限状态…</p>
        )}
        <div className="actions">
          <button
            type="button"
            className="btn ghost"
            disabled={busy}
            onClick={() => void api.openMicrophoneSettings()}
          >
            打开麦克风设置
          </button>
          <button
            type="button"
            className="btn ghost"
            disabled={busy}
            onClick={() => void refreshPerm()}
          >
            刷新状态
          </button>
          <button
            type="button"
            className="btn ghost"
            disabled={busy}
            onClick={() =>
              void (async () => {
                try {
                  await api.reopenOnboarding();
                  window.location.reload();
                } catch (e) {
                  onError(String(e));
                }
              })()
            }
          >
            重新运行首次设置
          </button>
        </div>
      </section>

      <HotkeyRecorder
        enabled={hotkeyEnabled}
        toggle={hotkeyToggle}
        showCapsule={showCapsule}
        mode={hotkeyMode}
        busy={busy}
        onBusy={onBusy}
        onError={onError}
        onChange={(next) => {
          setHotkeyEnabled(next.enabled);
          setHotkeyToggle(next.toggle);
          setShowCapsule(next.showCapsule);
          setHotkeyMode(next.mode);
        }}
        onSaved={onSaved}
      />

      <section className="card settings-section">
        <h2>声音反馈</h2>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={soundsEnabledUi}
              disabled={busy || savingSounds}
              onChange={(e) => {
                const next = e.target.checked;
                setSoundsEnabledUi(next);
                void (async () => {
                  setSavingSounds(true);
                  try {
                    const saved = await api.saveUiConfig({ sounds: next });
                    setSoundsEnabledUi(saved.sounds);
                    setSoundsEnabled(saved.sounds);
                    onSaved();
                  } catch (err) {
                    setSoundsEnabledUi(!next);
                    onError(String(err));
                  } finally {
                    setSavingSounds(false);
                  }
                })();
              }}
            />{" "}
            听写开始 / 完成 / 出错时播放提示音
          </label>
        </div>
      </section>

      <section className="card settings-section">
        <h2>翻译快捷键</h2>
        <p className="muted-text">
          另一组键，专门「说话 → 整理 → <strong>译成目标语言（按所选风格）</strong>」。
          和上面的全局热键一样：{hotkeyMode === "toggle" ? "再按一次结束" : "按住说话，松手结束"}
          ；可在下方单独重绑这组快捷键、设定目标语言与译文风格。
        </p>
        {(() => {
          const tr =
            intents.find((i) => i.intent.toLowerCase() === "translate") || {
              id: "translate",
              chord: "Alt+Shift+T",
              mode: hotkeyMode === "toggle" ? "toggle" : "hold",
              intent: "translate",
              targetLanguage: "en",
              enabled: false,
            };
          const lang = (tr.targetLanguage || "en").toLowerCase();
          const preset = ["en", "zh", "ja", "ko", "fr", "de", "es"].includes(lang)
            ? lang
            : "custom";
          // Translation style / register is a separate axis from target language.
          // Known preset keys map to a curated tone; anything else is custom text.
          const styleRaw = (tr.translateStyle || "").trim();
          const stylePreset =
            styleRaw === "" || styleRaw.toLowerCase() === "faithful"
              ? "faithful"
              : ["formal", "casual", "social"].includes(styleRaw.toLowerCase())
                ? styleRaw.toLowerCase()
                : "custom";

          async function saveTranslate(next: {
            enabled: boolean;
            chord: string;
            targetLanguage: string;
            translateStyle?: string;
          }) {
            onBusy(true);
            onError(null);
            try {
              const style = (next.translateStyle ?? "").trim();
              const existing = intents.find(
                (i) => i.intent.toLowerCase() === "translate",
              );
              const nextTranslate = {
                id: existing?.id || "translate",
                chord: next.chord || "Alt+Shift+T",
                mode: hotkeyMode === "toggle" ? "toggle" : "hold",
                intent: "translate",
                targetLanguage: next.targetLanguage || "en",
                translateStyle: style || undefined,
                enabled: next.enabled,
              };
              // Replace only the translation intent; never drop other configured
              // intents (save_hotkey_config overwrites the whole list).
              const list = existing
                ? intents.map((i) =>
                    i.intent.toLowerCase() === "translate" ? nextTranslate : i,
                  )
                : [...intents, nextTranslate];
              const h = await api.saveHotkeyConfig({
                enabled: hotkeyEnabled,
                toggle: hotkeyToggle,
                showCapsule,
                mode: hotkeyMode,
                intents: list,
              });
              setIntents(h.intents || list);
              setHotkeyRegisterNote(h.registerNote || "");
              onSaved();
            } catch (e) {
              onError(String(e));
            } finally {
              onBusy(false);
            }
          }

          return (
            <div className="intent-card">
              <label className="muted-text intent-enable">
                <input
                  type="checkbox"
                  checked={!!tr.enabled}
                  disabled={busy}
                  onChange={(e) =>
                    void saveTranslate({
                      enabled: e.target.checked,
                      chord: tr.chord || "Control+Alt",
                      targetLanguage: tr.targetLanguage || "en",
                      translateStyle: tr.translateStyle,
                    })
                  }
                />{" "}
                启用翻译热键
              </label>

              <div className="intent-card-row">
                <span className="muted-text intent-label">快捷键</span>
                <ChordCaptureChip
                  value={tr.chord || "Control+Alt"}
                  disabled={busy || !tr.enabled}
                  busy={busy}
                  onBusy={onBusy}
                  onError={onError}
                  onChange={(chord) =>
                    void saveTranslate({
                      enabled: true,
                      chord,
                      targetLanguage: tr.targetLanguage || "en",
                      translateStyle: tr.translateStyle,
                    })
                  }
                />
                <button
                  type="button"
                  className="btn small ghost"
                  disabled={busy}
                  onClick={() =>
                    void saveTranslate({
                      enabled: true,
                      chord: "Alt+Shift+T",
                      targetLanguage: tr.targetLanguage || "en",
                      translateStyle: tr.translateStyle,
                    })
                  }
                  title="推荐：与纯修饰键主热键不易冲突"
                >
                  推荐 ⌥⇧T
                </button>
              </div>
              {hotkeyRegisterNote && (
                <p className="muted-text intent-hint" style={{ fontSize: "0.8rem" }}>
                  注册状态：{hotkeyRegisterNote}
                </p>
              )}

              <div className="intent-card-row">
                <span className="muted-text intent-label">译成</span>
                <select
                  className="input"
                  style={{ maxWidth: 180 }}
                  value={preset}
                  disabled={busy || !tr.enabled}
                  onChange={(e) => {
                    const v = e.target.value;
                    void saveTranslate({
                      enabled: tr.enabled,
                      chord: tr.chord || "Alt+Shift+T",
                      targetLanguage: v === "custom" ? "pt" : v,
                      translateStyle: tr.translateStyle,
                    });
                  }}
                >
                  <option value="en">英语</option>
                  <option value="zh">中文</option>
                  <option value="ja">日语</option>
                  <option value="ko">韩语</option>
                  <option value="fr">法语</option>
                  <option value="de">德语</option>
                  <option value="es">西班牙语</option>
                  <option value="custom">其他…</option>
                </select>
                {preset === "custom" && (
                  <input
                    className="input"
                    style={{ maxWidth: 100 }}
                    value={tr.targetLanguage}
                    disabled={busy || !tr.enabled}
                    onChange={(e) =>
                      setIntents([
                        {
                          id: "translate",
                          chord: tr.chord || "Alt+Shift+T",
                          mode: hotkeyMode === "toggle" ? "toggle" : "hold",
                          intent: "translate",
                          targetLanguage: e.target.value,
                          translateStyle: tr.translateStyle,
                          enabled: !!tr.enabled,
                        },
                      ])
                    }
                    onBlur={() =>
                      void saveTranslate({
                        enabled: !!tr.enabled,
                        chord: tr.chord || "Alt+Shift+T",
                        targetLanguage: tr.targetLanguage || "en",
                        translateStyle: tr.translateStyle,
                      })
                    }
                    placeholder="语言代码"
                  />
                )}
              </div>

              <div className="intent-card-row">
                <span className="muted-text intent-label">风格</span>
                <select
                  className="input"
                  style={{ maxWidth: 180 }}
                  value={stylePreset}
                  disabled={busy || !tr.enabled}
                  onChange={(e) => {
                    const v = e.target.value;
                    // faithful → clear; a preset → its key; custom → seed an
                    // editable example (mirrors the target-language "其他…" flow).
                    const nextStyle =
                      v === "faithful"
                        ? ""
                        : v === "custom"
                          ? stylePreset === "custom"
                            ? styleRaw
                            : "更简洁"
                          : v;
                    void saveTranslate({
                      enabled: tr.enabled,
                      chord: tr.chord || "Alt+Shift+T",
                      targetLanguage: tr.targetLanguage || "en",
                      translateStyle: nextStyle,
                    });
                  }}
                >
                  <option value="faithful">忠实（默认）</option>
                  <option value="formal">正式</option>
                  <option value="casual">口语</option>
                  <option value="social">社媒（Twitter/X）</option>
                  <option value="custom">自定义…</option>
                </select>
                {stylePreset === "custom" && (
                  <input
                    className="input"
                    style={{ maxWidth: 240 }}
                    value={styleRaw}
                    disabled={busy || !tr.enabled}
                    onChange={(e) =>
                      setIntents([
                        {
                          id: "translate",
                          chord: tr.chord || "Alt+Shift+T",
                          mode: hotkeyMode === "toggle" ? "toggle" : "hold",
                          intent: "translate",
                          targetLanguage: tr.targetLanguage || "en",
                          translateStyle: e.target.value,
                          enabled: !!tr.enabled,
                        },
                      ])
                    }
                    onBlur={() =>
                      void saveTranslate({
                        enabled: !!tr.enabled,
                        chord: tr.chord || "Alt+Shift+T",
                        targetLanguage: tr.targetLanguage || "en",
                        translateStyle: tr.translateStyle,
                      })
                    }
                    placeholder="像李白写诗 / 更简洁 / 商务邮件语气"
                  />
                )}
              </div>

              <p className="muted-text intent-hint">
                {tr.enabled
                  ? hotkeyMode === "toggle"
                    ? `按一下开始录音，再按结束 → 整理后译成「${tr.targetLanguage || "en"}」（${styleLabel(stylePreset, styleRaw)}）。应弹出录音胶囊。`
                    : `按住 ${tr.chord || "⌥⇧T"} 说话，松手 → 整理后译成「${tr.targetLanguage || "en"}」（${styleLabel(stylePreset, styleRaw)}）。应弹出录音胶囊。`
                  : "勾选启用后立即生效。建议用带字母的组合（如 ⌥⇧T），避免与纯修饰键主热键冲突。"}
              </p>
              {hotkeyToggle &&
                tr.chord &&
                tr.chord.replace(/\+/g, "").toLowerCase().startsWith(
                  hotkeyToggle.replace(/\+/g, "").toLowerCase()
                ) &&
                tr.chord.split("+").length <= hotkeyToggle.split("+").length && (
                  <p className="muted-text intent-hint" style={{ color: "var(--error)" }}>
                    注意：当前主热键是「{hotkeyToggle}」。若翻译键与它完全相同会冲突；翻译键应多一个字母键（如主热键
                    ⌥⇧ 时用 ⌥⇧T）。
                  </p>
                )}
            </div>
          );
        })()}
      </section>

      <section className="card settings-section">
        <h2>会议自动检测</h2>
        <p className="muted-text">
          开启后，Lumen 会按下方的外置应用目录留意会议 App 与浏览器的麦克风活动，并在检测到时<strong>弹窗提示</strong>——
          仅在你点击「开始记录」后才会录音，绝不自动录制。默认关闭。
        </p>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={detectionEnabled}
              disabled={busy || !detectionCapable}
              onChange={(e) =>
                void (async () => {
                  onBusy(true);
                  onError(null);
                  try {
                    const next = await api.setMeetingDetectionEnabled(e.target.checked);
                    setDetectionEnabled(next.enabled);
                    setDetectionCapable(next.capabilityAvailable);
                  } catch (err) {
                    onError(String(err));
                  } finally {
                    onBusy(false);
                  }
                })()
              }
            />{" "}
            启用会议自动检测（弹窗提示，不自动录制）
          </label>
        </div>
        {!detectionCapable && (
          <p className="muted-text" style={{ fontSize: "0.85rem", marginTop: 8 }}>
            当前系统不支持会议检测所需的系统能力（需要较新的 macOS），此开关已停用。
          </p>
        )}

        <hr className="settings-divider" />
        <h3>会议 / 录制应用目录</h3>
        <p className="muted-text">
          这份列表来自用户数据目录中的独立 TOML 文件，不编译进客户端。检测和系统声音录制都会立即使用保存后的列表；浏览器即使允许检测，仍会在每次录制前提示“整个浏览器”范围。
        </p>
        {meetingApps ? (
          <>
            <p className="muted-text meeting-app-config-path">
              配置文件：<code>{meetingApps.path}</code>
            </p>
            {meetingApps.loadError && (
              <p className="banner error" role="alert">
                外置配置载入失败：{meetingApps.loadError}。请修正 TOML 后点击“从文件重新载入”。
              </p>
            )}
            <div className="meeting-app-config-list">
              {meetingApps.applications.map((entry, index) => (
                <div className="meeting-app-config-row" key={index}>
                  <input
                    className="input"
                    aria-label={`应用 ${index + 1} 名称`}
                    value={entry.name}
                    disabled={meetingAppsSaving}
                    onChange={(event) =>
                      setMeetingApps((current) =>
                        current
                          ? {
                              ...current,
                              applications: current.applications.map((item, itemIndex) =>
                                itemIndex === index
                                  ? { ...item, name: event.target.value }
                                  : item,
                              ),
                            }
                          : current,
                      )
                    }
                  />
                  <select
                    className="select"
                    aria-label={`应用 ${index + 1} 类型`}
                    value={entry.kind}
                    disabled={meetingAppsSaving}
                    onChange={(event) => {
                      const kind = event.target.value === "browser" ? "browser" : "meeting";
                      setMeetingApps((current) =>
                        current
                          ? {
                              ...current,
                              applications: current.applications.map((item, itemIndex) =>
                                itemIndex === index ? { ...item, kind } : item,
                              ),
                            }
                          : current,
                      );
                    }}
                  >
                    <option value="meeting">会议 App</option>
                    <option value="browser">浏览器</option>
                  </select>
                  <textarea
                    className="input meeting-app-bundle-ids"
                    aria-label={`应用 ${index + 1} Bundle IDs`}
                    rows={Math.max(1, Math.min(3, entry.bundle_ids.length))}
                    value={entry.bundle_ids.join("\n")}
                    disabled={meetingAppsSaving}
                    placeholder="每行一个 bundle ID"
                    onChange={(event) => {
                      const bundle_ids = event.target.value.split("\n");
                      setMeetingApps((current) =>
                        current
                          ? {
                              ...current,
                              applications: current.applications.map((item, itemIndex) =>
                                itemIndex === index ? { ...item, bundle_ids } : item,
                              ),
                            }
                          : current,
                      );
                    }}
                  />
                  <label className="muted-text meeting-app-config-toggle">
                    <input
                      type="checkbox"
                      checked={entry.detect}
                      disabled={meetingAppsSaving}
                      onChange={(event) =>
                        setMeetingApps((current) =>
                          current
                            ? {
                                ...current,
                                applications: current.applications.map((item, itemIndex) =>
                                  itemIndex === index
                                    ? { ...item, detect: event.target.checked }
                                    : item,
                                ),
                              }
                            : current,
                        )
                      }
                    />
                    检测
                  </label>
                  {entry.kind === "browser" ? (
                    <span className="muted-text meeting-app-config-toggle">录制时询问</span>
                  ) : (
                    <label className="muted-text meeting-app-config-toggle">
                      <input
                        type="checkbox"
                        checked={entry.capture}
                        disabled={meetingAppsSaving}
                        onChange={(event) =>
                          setMeetingApps((current) =>
                            current
                              ? {
                                  ...current,
                                  applications: current.applications.map((item, itemIndex) =>
                                    itemIndex === index
                                      ? { ...item, capture: event.target.checked }
                                      : item,
                                  ),
                                }
                              : current,
                          )
                        }
                      />
                      录制声音
                    </label>
                  )}
                  <button
                    type="button"
                    className="btn ghost"
                    disabled={meetingAppsSaving}
                    onClick={() =>
                      setMeetingApps((current) =>
                        current
                          ? {
                              ...current,
                              applications: current.applications.filter(
                                (_, itemIndex) => itemIndex !== index,
                              ),
                            }
                          : current,
                      )
                    }
                  >
                    删除
                  </button>
                </div>
              ))}
            </div>
            <div className="actions meeting-app-config-actions">
              <button
                type="button"
                className="btn ghost"
                disabled={meetingAppsSaving}
                onClick={() =>
                  setMeetingApps((current) =>
                    current
                      ? {
                          ...current,
                          applications: [
                            ...current.applications,
                            {
                              name: "新会议应用",
                              kind: "meeting",
                              bundle_ids: [""],
                              detect: true,
                              capture: true,
                            },
                          ],
                        }
                      : current,
                  )
                }
              >
                新增应用
              </button>
              <button
                type="button"
                className="btn ghost"
                disabled={meetingAppsSaving}
                onClick={() =>
                  void api
                    .reloadMeetingAppCatalog()
                    .then(setMeetingApps)
                    .catch((error) => onError(`重新载入会议应用配置失败：${String(error)}`))
                }
              >
                从文件重新载入
              </button>
              <button
                type="button"
                className="btn"
                disabled={meetingAppsSaving}
                onClick={() => void saveMeetingApps()}
              >
                {meetingAppsSaving ? "保存中…" : "保存应用目录"}
              </button>
            </div>
          </>
        ) : (
          <p className="muted-text">会议应用目录尚未载入。</p>
        )}

        <hr className="settings-divider" />
        <p className="muted-text">
          看护正在进行的录音：持续检测不到会议声音时先提醒，20 秒后自动停止；超过最长时长上限会先提醒，60
          秒后自动停止；关联日历会议结束时也会提醒你。
        </p>
        <div className="form-row">
          <label className="form-label" htmlFor="silence-auto-stop">
            无声提醒阈值（分钟），0 关闭
          </label>
          <input
            id="silence-auto-stop"
            className="input"
            type="number"
            min={0}
            step={1}
            value={silenceAutoStopMinutes}
            disabled={busy}
            style={{ maxWidth: 120 }}
            onChange={(e) => {
              const next = Math.max(0, Math.floor(Number(e.target.value) || 0));
              setSilenceAutoStopMinutes(next);
              saveWatchdog(next, maxDurationMinutes, calendarEndReminder);
            }}
          />
        </div>
        <div className="form-row">
          <label className="form-label" htmlFor="max-duration">
            最长录制时长（分钟），0 关闭
          </label>
          <input
            id="max-duration"
            className="input"
            type="number"
            min={0}
            step={1}
            value={maxDurationMinutes}
            disabled={busy}
            style={{ maxWidth: 120 }}
            onChange={(e) => {
              const next = Math.max(0, Math.floor(Number(e.target.value) || 0));
              setMaxDurationMinutes(next);
              saveWatchdog(silenceAutoStopMinutes, next, calendarEndReminder);
            }}
          />
        </div>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={calendarEndReminder}
              disabled={busy}
              onChange={(e) => {
                const next = e.target.checked;
                setCalendarEndReminder(next);
                saveWatchdog(silenceAutoStopMinutes, maxDurationMinutes, next);
              }}
            />{" "}
            日历结束时提醒停止录音
          </label>
        </div>
      </section>

      <section className="card settings-section">
        <h2>语音识别（ASR）</h2>
        <p className="muted-text">
          SenseVoice 与 Qwen 是两条独立的本地识别前端；Qwen
          优先准确率，SenseVoice 优先较低资源占用。识别后共用当前的文本整理、插入与学习流程。
        </p>
        <div className="form-row">
          <label className="form-label">
            Provider
          </label>
          <select
            className="input"
            value={asrProvider}
            disabled={busy}
            onChange={(e) => {
              const id = e.target.value;
              setAsrProvider(id);
              if (asrModels) {
                setAsrCustomPath(localAsrModelDir(asrModels, id));
              }
              const p = asrPresets.find((x) => x.id === id);
              if (p) {
                if (p.baseUrl) setAsrBaseUrl(p.baseUrl);
                if (p.defaultModel) setAsrModel(p.defaultModel);
              }
            }}
          >
            {asrPresets.map((p) => (
              <option key={p.id} value={p.id}>
                {p.label}
                {p.status === "config_only" ? "（预设）" : ""}
              </option>
            ))}
          </select>
        </div>
        {asrPresets.find((p) => p.id === asrProvider)?.notes && (
          <p className="muted-text" style={{ fontSize: "0.82rem", marginTop: 0 }}>
            {asrPresets.find((p) => p.id === asrProvider)?.notes}
          </p>
        )}
        {asrProvider.startsWith("local") && asrModels && (
          <div className="onboard-status" style={{ marginBottom: 12 }}>
            <div className="muted-text">当前解析的模型目录</div>
            <p className="muted-text" style={{ wordBreak: "break-all", marginTop: 4 }}>
              <code>{localAsrModelDir(asrModels, asrProvider)}</code>
            </p>
            {asrModels.candidates
              .filter(
                (candidate) =>
                  candidate.ready &&
                  candidate.engine === localAsrEngine(asrProvider),
              )
              .map((candidate) => (
                <div key={`${candidate.engine}:${candidate.path}`} className="onboard-candidate">
                  <span className="muted-text" style={{ wordBreak: "break-all" }}>
                    {candidate.label}
                  </span>
                  <button
                    type="button"
                    className="btn ghost"
                    disabled={busy}
                    onClick={() =>
                      void (async () => {
                        onBusy(true);
                        onError(null);
                        try {
                          const engine = localAsrEngine(asrProvider);
                          const status = await api.useExistingAsrModel(candidate.path, engine);
                          setAsrModels(status);
                          setAsrCustomPath(status.activeModelDir);
                          await refreshActiveCleanupProfile();
                          onSaved();
                        } catch (e) {
                          onError(String(e));
                        } finally {
                          onBusy(false);
                        }
                      })()
                    }
                  >
                    使用
                  </button>
                </div>
              ))}
            <div className="form-row" style={{ marginTop: 10 }}>
              <input
                className="input"
                value={asrCustomPath}
                disabled={busy}
                placeholder="或粘贴本地模型目录路径…"
                onChange={(event) => setAsrCustomPath(event.target.value)}
              />
              <button
                type="button"
                className="btn ghost"
                disabled={busy || !asrCustomPath.trim()}
                onClick={() =>
                  void (async () => {
                    onBusy(true);
                    onError(null);
                    try {
                      const engine = localAsrEngine(asrProvider);
                      const status = await api.useExistingAsrModel(
                        asrCustomPath.trim(),
                        engine,
                      );
                      setAsrModels(status);
                      setAsrCustomPath(status.activeModelDir);
                      await refreshActiveCleanupProfile();
                      onSaved();
                    } catch (e) {
                      onError(String(e));
                    } finally {
                      onBusy(false);
                    }
                  })()
                }
              >
                验证并使用
              </button>
            </div>
          </div>
        )}
        {asrProvider === "local_qwen" && (
          <>
            <div className="form-row">
              <label className="form-label wide">
                Qwen Python
              </label>
              <input
                className="input"
                value={asrRuntimePath}
                disabled={busy}
                onChange={(event) => setAsrRuntimePath(event.target.value)}
                placeholder="包含 mlx_qwen3_asr 的 Python 可执行文件"
              />
            </div>
            <div className="form-row">
              <label className="muted-text">
                <input
                  type="checkbox"
                  checked={qwenShadowEnabled}
                  disabled={busy}
                  onChange={(event) =>
                    setQwenShadowEnabled(event.target.checked)
                  }
                />{" "}
                启用本地术语候选分析（不改变输出）
              </label>
            </div>
          </>
        )}
        {!asrProvider.startsWith("local") && (
          <>
            <div className="form-row">
              <label className="form-label">
                Base URL
              </label>
              <input
                className="input"
                value={asrBaseUrl}
                disabled={busy}
                onChange={(e) => setAsrBaseUrl(e.target.value)}
              />
            </div>
            <div className="form-row">
              <label className="form-label">
                Model
              </label>
              <input
                className="input"
                value={asrModel}
                disabled={busy}
                onChange={(e) => setAsrModel(e.target.value)}
                list="asr-model-list"
              />
              <datalist id="asr-model-list">
                {(asrPresets.find((p) => p.id === asrProvider)?.models || []).map((m) => (
                  <option key={m} value={m} />
                ))}
              </datalist>
            </div>
            <div className="form-row">
              <label className="form-label">
                API Key
              </label>
              <input
                className="input"
                type="password"
                value={asrApiKey}
                disabled={busy}
                onChange={(e) => setAsrApiKey(e.target.value)}
                placeholder={asrHasKey ? "已保存（留空不改）" : "必填"}
              />
            </div>
            <div className="form-row">
              <label className="form-label">
                语言
              </label>
              <input
                className="input"
                value={asrLanguage}
                disabled={busy}
                onChange={(e) => setAsrLanguage(e.target.value)}
                placeholder="可选 zh / en"
              />
            </div>
          </>
        )}
        <div className="actions">
          <button
            type="button"
            className="btn"
            disabled={busy}
            onClick={() =>
              void (async () => {
                onBusy(true);
                try {
                  const input: Parameters<typeof api.saveAsrServiceConfig>[0] = {
                    provider: asrProvider,
                    runtimePath: asrRuntimePath,
                    qwenShadowEnabled,
                    baseUrl: asrBaseUrl,
                    model: asrModel,
                    language: asrLanguage,
                  };
                  if (asrApiKey.trim()) input.apiKey = asrApiKey.trim();
                  const s = await api.saveAsrServiceConfig(input);
                  setAsrProvider(s.provider);
                  setAsrRuntimePath(s.runtimePath || "");
                  setQwenShadowEnabled(s.qwenShadowEnabled);
                  setAsrBaseUrl(s.baseUrl);
                  setAsrModel(s.model);
                  setAsrLanguage(s.language);
                  setAsrHasKey(s.hasApiKey);
                  setAsrApiKey("");
                  await refreshActiveCleanupProfile();
                  onSaved();
                } catch (e) {
                  onError(String(e));
                } finally {
                  onBusy(false);
                }
              })()
            }
          >
            保存 ASR 设置
          </button>
        </div>
      </section>

      <section className="card settings-section">
        <h2>插入策略</h2>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={autoInsert}
              disabled={busy}
              onChange={(e) => setAutoInsert(e.target.checked)}
            />{" "}
            停止转写后自动插入
          </label>
        </div>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={preserveClip}
              disabled={busy}
              onChange={(e) => setPreserveClip(e.target.checked)}
            />{" "}
            保留并恢复原剪贴板
          </label>
        </div>
        <div className="form-row">
          <label className="form-label">
            模式
          </label>
          <select
            className="input"
            value={injectMode}
            disabled={busy}
            onChange={(e) => setInjectMode(e.target.value)}
          >
            <option value="auto">auto（paste → type）</option>
            <option value="paste">paste only</option>
            <option value="type">type unicode</option>
            <option value="copy_only">copy only（仅剪贴板）</option>
          </select>
        </div>
        <div className="actions">
          <button type="button" className="btn" disabled={busy} onClick={() => void saveInject()}>
            保存插入设置
          </button>
        </div>
      </section>

      <section className="card settings-section">
        <h2>AI 修正（Corrector）</h2>
        <p className="muted-text">
          识别原文始终保留。下面每一项都会写进发给模型的
          <strong>系统提示词分层</strong>
          （红线固定 + 整理强度 + 语气/标点 + 额外规则 + 自定义）。改完请点「保存」；下次热键松手转写时生效。模型失败时回退规则预处理。
        </p>
        <details className="settings-help">
          <summary>这些设置在模型侧具体改什么？</summary>
          <ul className="muted-text settings-help-list">
            <li>
              <strong>整理强度</strong>：无=不调模型；轻=纠错/去口头禅；中=默认理顺；强=更短更书面。并影响 temperature。
            </li>
            <li>
              <strong>语气 / 大小写 / 标点</strong>：追加到 prompt 的「语气与书写」段，不改红线（禁止回答问题等）。
            </li>
            <li>
              <strong>额外整理</strong>：concise/clarity 等勾选项变成独立规则条。
            </li>
            <li>
              <strong>自定义说明</strong>：叠加在红线之上；若要求「回答问题」会被 prompt 声明忽略。
            </li>
            <li>
              <strong>Provider / Model</strong>：走 Ollama 或 OpenAI 兼容 API；与整理文案无关，只决定谁执行。
            </li>
            <li>
              <strong>意图快捷键·翻译</strong>：同一套整理设置 + 额外「本轮译成目标语言」指令（若整理=无，翻译时仍至少轻度纠错）。
            </li>
          </ul>
        </details>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={enabled}
              disabled={busy}
              onChange={(e) => setEnabled(e.target.checked)}
            />{" "}
            启用模型修正
          </label>
        </div>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={useCapturedContext}
              disabled={busy || !enabled}
              onChange={(e) => setUseCapturedContext(e.target.checked)}
              aria-describedby="captured-context-help"
            />{" "}
            用当前应用和光标附近文字辅助纠错
          </label>
          <p id="captured-context-help" className="muted-text">
            仅发送有长度上限的应用、窗口、选区和光标附近文字；完整上下文仍只在本机加密保存。
            若当前 Corrector 是云模型，这份精简内容会随本次请求上传。
          </p>
        </div>
        <div className="cleanup-level-block">
          <div className="field-label">自动整理强度</div>
          <p className="muted-text cleanup-hint">
            {cfg?.cleanupProfile === "qwen"
              ? "当前生效：Qwen 独立整理配置。切回 SenseVoice 会恢复原来的整理强度。"
              : "当前生效：SenseVoice／其他 ASR 的默认整理配置。Qwen 会使用独立设置。"}
          </p>
          <div className="cleanup-seg" role="group" aria-label="整理强度">
            {(
              [
                ["none", "无", "原样，只做空格标点"],
                ["light", "轻", "去口头禅、纠错"],
                ["medium", "中", "更清楚（默认）"],
                ["strong", "强", "更短更顺"],
              ] as const
            ).map(([id, label, tip]) => (
              <button
                key={id}
                type="button"
                className={`cleanup-seg-btn ${cleanup === id ? "active" : ""}`}
                disabled={busy}
                title={tip}
                onClick={() => setCleanup(id)}
              >
                {label}
              </button>
            ))}
          </div>
          <p className="muted-text cleanup-hint">
            {cleanup === "none" && "不调用模型。历史里仍可复制整理后文本（若曾有）。"}
            {cleanup === "light" && "修正错字与口头禅，尽量保留原句。"}
            {cleanup === "medium" && "默认：理顺语序、轻度删冗余，不增删事实。"}
            {cleanup === "strong" && "更积极改写可读性；仍禁止回答问题或编造内容。"}
          </p>
          <div className="cleanup-example muted-text">
            例：嗯我们那个还约咖啡吗我觉得可能要早点出门因为会堵车
            <br />→ 轻：去「嗯/那个」+ 标点 · 中：更顺 · 强：压成更短几句
          </div>
        </div>
        <div className="form-row">
          <label className="form-label">
            语气
          </label>
          <select
            className="input"
            value={style}
            disabled={busy}
            onChange={(e) => setStyle(e.target.value)}
          >
            <option value="formal">正式</option>
            <option value="neutral">中性</option>
            <option value="casual">轻松</option>
            <option value="very_casual">很随意</option>
          </select>
        </div>
        <div className="form-row">
          <label className="form-label">
            英文大小写
          </label>
          <select
            className="input"
            value={casing}
            disabled={busy}
            onChange={(e) => setCasing(e.target.value)}
          >
            <option value="sentence">句首大写</option>
            <option value="preserve">保持原样</option>
            <option value="lower">尽量小写</option>
          </select>
        </div>
        <div className="form-row">
          <label className="form-label">
            标点
          </label>
          <select
            className="input"
            value={punctuation}
            disabled={busy}
            onChange={(e) => setPunctuation(e.target.value)}
          >
            <option value="standard">标准</option>
            <option value="light">从简</option>
            <option value="preserve">贴近输入</option>
          </select>
        </div>
        <div className="polish-checks">
          <div className="field-label">额外整理</div>
          {(
            [
              ["concise", "更短"],
              ["clarity", "更清楚"],
              ["reorder", "理顺语序"],
              ["structure", "加结构"],
              ["keep_tone", "保留语气"],
            ] as const
          ).map(([id, label]) => (
            <label key={id} className="muted-text polish-check">
              <input
                type="checkbox"
                checked={polish.includes(id)}
                disabled={busy}
                onChange={(e) => {
                  setPolish((prev) =>
                    e.target.checked ? [...prev, id] : prev.filter((x) => x !== id)
                  );
                }}
              />{" "}
              {label}
            </label>
          ))}
        </div>
        <div className="form-row" style={{ marginBottom: 8, marginTop: 10 }}>
          <label className="muted-text">
            <input
              type="checkbox"
              checked={customEnabled}
              disabled={busy}
              onChange={(e) => setCustomEnabled(e.target.checked)}
            />{" "}
            自定义补充说明（叠加在红线之上）
          </label>
        </div>
        {customEnabled && (
          <textarea
            className="textarea"
            rows={3}
            value={customInstruction}
            disabled={busy}
            onChange={(e) => setCustomInstruction(e.target.value)}
            placeholder="例如：保留英文专有名词；适合即时消息"
          />
        )}
        <div className="form-row" style={{ marginBottom: 10, marginTop: 12 }}>
          <label className="form-label">
            Provider
          </label>
          <select
            className="input"
            value={provider}
            disabled={busy}
            onChange={(e) => {
              const id = e.target.value;
              setProvider(id);
              const p = llmPresets.find((x) => x.id === id);
              if (p) {
                if (p.baseUrl) setBaseUrl(p.baseUrl);
                if (p.defaultModel) setModel(p.defaultModel);
              }
            }}
          >
            {(llmPresets.length
              ? llmPresets
              : [
                  { id: "ollama", label: "Ollama（本地）" },
                  { id: "openai_compatible", label: "OpenAI 兼容" },
                  { id: "none", label: "关闭" },
                ]
            ).map((p) => (
              <option key={p.id} value={p.id}>
                {p.label}
              </option>
            ))}
          </select>
        </div>
        {llmPresets.find((p) => p.id === provider)?.notes && (
          <p className="muted-text" style={{ fontSize: "0.82rem", marginTop: 0 }}>
            {llmPresets.find((p) => p.id === provider)?.notes}
          </p>
        )}
        <div className="form-row">
          <label className="form-label">
            Base URL
          </label>
          <input
            className="input"
            value={baseUrl}
            disabled={busy}
            onChange={(e) => setBaseUrl(e.target.value)}
            placeholder="http://127.0.0.1:11434/v1"
          />
        </div>
        <div className="form-row">
          <label className="form-label">
            Model
          </label>
          <input
            className="input"
            value={model}
            disabled={busy}
            onChange={(e) => setModel(e.target.value)}
            placeholder="qwen3.5:9b"
            list="llm-model-list"
          />
          <datalist id="llm-model-list">
            {(llmPresets.find((p) => p.id === provider)?.models || []).map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
        </div>
        <div className="form-row">
          <label className="form-label">
            API Key
          </label>
          <input
            className="input"
            type="password"
            value={apiKey}
            disabled={busy}
            onChange={(e) => setApiKey(e.target.value)}
            placeholder={cfg?.hasApiKey ? "已保存（留空不改）" : "可选"}
          />
        </div>
        <div className="form-row">
          <label className="form-label">
            Timeout
          </label>
          <input
            className="input"
            type="number"
            min={5}
            value={timeoutSecs}
            disabled={busy}
            onChange={(e) => setTimeoutSecs(Number(e.target.value) || 60)}
          />
        </div>
        {cfg && (
          <p className="muted-text" style={{ fontSize: "0.85rem" }}>
            当前：<code>{cfg.label}</code>
          </p>
        )}
        <div className="actions">
          <button type="button" className="btn" disabled={busy || !cfg} onClick={() => void save()}>
            保存
          </button>
          <button
            type="button"
            className="btn ghost"
            disabled={busy}
            onClick={() => void probeCorrect()}
          >
            测试修正
          </button>
        </div>
        {probe && <pre className="field-value" style={{ marginTop: 12 }}>{probe}</pre>}
      </section>

      <section className="card settings-section">
        <h2>编辑学习</h2>
        <p className="muted-text">
          转写结果改字、或粘贴后在目标 App 再改，可生成词典候选。默认需手动确认。
        </p>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={autoPromote}
              disabled={busy}
              onChange={(e) => setAutoPromote(e.target.checked)}
            />{" "}
            自动晋升（同一替换累计达到阈值）
          </label>
        </div>
        <div className="form-row">
          <label className="form-label">
            阈值 N
          </label>
          <input
            className="input"
            type="number"
            min={2}
            value={promoteN}
            disabled={busy}
            onChange={(e) => setPromoteN(Number(e.target.value) || 3)}
          />
        </div>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={postPaste}
              disabled={busy}
              onChange={(e) => setPostPaste(e.target.checked)}
            />{" "}
            粘贴后监听目标输入框改动（需辅助功能）
          </label>
        </div>
        <p className="muted-text" style={{ fontSize: "0.85rem" }}>
          无固定停顿超时：短暂停顿和多次修改不会结束监听。会话默认最多保留 24
          小时，并在目标内容清空、输入面失效、同输入框开始下一次听写或应用重启时结束。
        </p>
        <div className="form-row">
          <label className="muted-text">
            <input
              type="checkbox"
              checked={persistEditEvidenceText}
              disabled={busy}
              onChange={(e) => setPersistEditEvidenceText(e.target.checked)}
            />{" "}
            在本机数据库保留完整听写与修改文本
          </label>
        </div>
        <p className="muted-text" style={{ fontSize: "0.85rem" }}>
          默认关闭：仅保留哈希和待确认的词汇/替换候选，降低敏感正文持久化风险。
        </p>
        {learning && (
          <p className="muted-text" style={{ fontSize: "0.85rem" }}>
            当前：autoPromote={String(learning.autoPromote)} N=
            {learning.autoPromoteThreshold} postPaste=
            {String(learning.postPasteCapture)}
          </p>
        )}
        <div className="actions">
          <button
            type="button"
            className="btn"
            disabled={busy}
            onClick={() =>
              void (async () => {
                onBusy(true);
                try {
                  const ln = await api.saveLearningConfig({
                    autoPromote,
                    autoPromoteThreshold: promoteN,
                    postPasteCapture: postPaste,
                    postPasteSeconds: postPasteSecs,
                    persistEditEvidenceText,
                  });
                  setLearning(ln);
                  onSaved();
                } catch (e) {
                  onError(String(e));
                } finally {
                  onBusy(false);
                }
              })()
            }
          >
            保存学习设置
          </button>
        </div>
      </section>

      <section className="card muted">
        <h2>说明</h2>
        <ul style={{ margin: 0, paddingLeft: "1.2rem", lineHeight: 1.7 }}>
          <li>
            Ollama 需本机运行，例如：
            <code>ollama pull qwen2.5:7b</code>
          </li>
          <li>词典 term/replacement 会注入 prompt，并先做确定性替换</li>
          <li>
            配置文件：
            <code>~/Library/Application Support/LumenAsr/config.toml</code>
          </li>
          <li>
            热键：设置里「点击录制」后直接按键；默认 ⌥Space，避开 Spotlight
          </li>
        </ul>
      </section>

      <p
        className="muted-text"
        style={{
          margin: "0.25rem 0 0",
          fontSize: "0.78rem",
          textAlign: "center",
          userSelect: "text",
        }}
        title="构建标识：版本 · git 短 sha · 构建时间"
      >
        {buildInfo === null
          ? "构建信息加载中…"
          : buildInfo === "error"
            ? "构建信息不可用（版本未知）"
            : `v${buildInfo.version} · ${buildInfo.git_sha} · ${buildInfo.build_time}`}
      </p>
    </>
  );
}

function Overview({
  health,
  sessions,
  dictCount,
  editLearning,
  busy,
  onSeed,
  onGoto,
}: {
  health: Health | null;
  sessions: SessionRecord[];
  dictCount: number;
  editLearning: EditLearningObservability | null;
  busy: boolean;
  onSeed: () => void;
  onGoto: (t: TabId) => void;
}) {
  return (
    <>
      <section className="card">
        <h2>状态</h2>
        {health ? (
          <dl className="meta">
            <dt>数据目录</dt>
            <dd>
              <code>{health.data_dir}</code>
            </dd>
            <dt>数据库</dt>
            <dd>
              <code>{health.db_path}</code>
            </dd>
            <dt>已保存会话</dt>
            <dd>{health.session_count}</dd>
            <dt>词典条目</dt>
            <dd>{health.dictionary_count}</dd>
            <dt>SenseVoice</dt>
            <dd>{health.sensevoice_ready ? "就绪" : "未就绪"}</dd>
            <dt>Qwen3-ASR</dt>
            <dd>{health.qwen_ready ? "就绪" : "未就绪"}</dd>
            <dt>Whisper</dt>
            <dd>{health.whisper_ready ? "就绪" : "未就绪"}</dd>
            <dt>Corrector</dt>
            <dd>
              {health.corrector_enabled ? health.corrector_label : "关闭"}
            </dd>
            <dt>编辑观察</dt>
            <dd>
              {editLearning
                ? `${editLearning.active_sessions} 活跃 · ${editLearning.revisions_recorded} revision`
                : "加载中"}
            </dd>
            <dt>观察恢复</dt>
            <dd>
              {editLearning
                ? `${editLearning.recoveries}/${editLearning.suspensions} · ${editLearning.sessions_failed_to_start} 次未启动 · ${editLearning.persistence_failures} 持久化失败`
                : "—"}
            </dd>
            <dt>归因防护</dt>
            <dd>
              {editLearning
                ? `${editLearning.content_boundary_finalizations} 次内容边界结束 · ${editLearning.same_surface_sessions_finalized} 次同输入框换代 · ${editLearning.insertion_target_mismatches} 次目标不匹配阻断`
                : "—"}
            </dd>
            <dt>观察隐私/延迟</dt>
            <dd>
              {editLearning
                ? `${editLearning.evidence_records_redacted} 条正文已脱敏 · snapshot max ${editLearning.snapshot_latency_ms_max} ms`
                : "—"}
            </dd>
          </dl>
        ) : (
          <p className="muted-text">加载中…</p>
        )}
        <div className="actions">
          <button type="button" className="btn" onClick={() => onGoto("record")}>
            去录音
          </button>
          <button type="button" className="btn" disabled={busy} onClick={onSeed}>
            写入示例会话
          </button>
          <button type="button" className="btn ghost" onClick={() => onGoto("history")}>
            查看历史
          </button>
          <button type="button" className="btn ghost" onClick={() => onGoto("dictionary")}>
            管理词典
          </button>
        </div>
      </section>

      <section className="card">
        <h2>最近会话</h2>
        {sessions.length === 0 ? (
          <p className="muted-text">暂无历史。可写入示例会话，或等 M2 录音管线接入。</p>
        ) : (
          <ul className="list">
            {sessions.slice(0, 5).map((s) => (
              <li key={s.id}>
                <span className="list-time">{formatTime(s.created_at)}</span>
                <span>{previewText(sessionMainText(s))}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="card muted">
        <h2>里程碑</h2>
        <ol>
          <li className="done">M0 — 架构骨架</li>
          <li className="done">M1 — Store / 词典 IPC + 本页 UI</li>
          <li className="done">M2 — SenseVoice (sherpa) + 麦克风</li>
          <li className="done">M3 — Ollama 修正</li>
          <li className="done">M4 — paste-first 注入 + 权限</li>
          <li className="done">M5 — 热键 + 胶囊</li>
          <li className="done">M6 — 编辑学习 / 粘贴后捕获</li>
        </ol>
        <p className="muted-text">
          词典条目数：{dictCount} · 热键默认 ⌥Space（设置里可点按录制）
        </p>
      </section>
    </>
  );
}

/** Quality of a session result — drives recovery UI, not decoration. */
function sessionQuality(s: SessionRecord): "ok" | "weak" | "empty" {
  const t = sessionMainText(s);
  if (!t) return "empty";
  if (t.length <= 2 || t === "。" || t === "." || t === "…") return "weak";
  return "ok";
}

function sessionMainText(s: SessionRecord): string {
  return firstNonBlankText(s.corrected, s.pasted, s.asr_raw);
}

function HistoryPanel({
  sessions,
  selected,
  editFeedbackRevision,
  busy,
  onSelect,
  onRefresh,
  onBusy,
  onError,
  onUpdated,
  onDelete,
}: {
  sessions: SessionRecord[];
  selected: SessionRecord | null;
  editFeedbackRevision: number;
  busy: boolean;
  onSelect: (id: string | null) => void;
  onRefresh: () => void;
  onBusy: (b: boolean) => void;
  onError: (e: string | null) => void;
  onUpdated: (s: SessionRecord) => void;
  onDelete: (id: string) => void;
}) {
  const [playing, setPlaying] = useState(false);
  const [copiedDetailTarget, setCopiedDetailTarget] = useState<"main" | "raw" | null>(
    null,
  );
  const [copyPending, setCopyPending] = useState(false);
  const [copiedSessionId, setCopiedSessionId] = useState<string | null>(null);
  const [showPipeline, setShowPipeline] = useState(false);
  const [retryNote, setRetryNote] = useState<string | null>(null);
  const [attempts, setAttempts] = useState<DictationAttemptRecord[]>([]);
  const [editEvents, setEditEvents] = useState<EditEvent[]>([]);
  const [editObservations, setEditObservations] = useState<EditObservation[]>([]);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const blobUrlRef = useRef<string | null>(null);
  const copyFeedbackTimerRef = useRef<number | null>(null);
  const [clipboardWriteGate] = useState(() => new ClipboardWriteGate());

  const stopAudio = useCallback(() => {
    if (audioRef.current) {
      audioRef.current.pause();
      audioRef.current = null;
    }
    if (blobUrlRef.current) {
      URL.revokeObjectURL(blobUrlRef.current);
      blobUrlRef.current = null;
    }
    setPlaying(false);
  }, []);

  useEffect(() => {
    stopAudio();
    setCopiedDetailTarget(null);
    setShowPipeline(false);
    setRetryNote(null);
  }, [selected?.id, stopAudio]);

  useEffect(() => {
    let cancelled = false;
    if (!selected) {
      setAttempts([]);
      setEditEvents([]);
      setEditObservations([]);
      return;
    }
    void Promise.all([
      api.listSessionAttempts(selected.id, 20),
      api.listEditEvents(selected.id),
      api.listEditObservations(selected.id),
    ])
      .then(([attemptRows, events, observations]) => {
        if (!cancelled) {
          setAttempts(attemptRows);
          setEditEvents(events);
          setEditObservations(observations);
        }
      })
      .catch((error) => {
        if (!cancelled) onError(`读取历史详情失败: ${String(error)}`);
      });
    return () => {
      cancelled = true;
    };
  }, [editFeedbackRevision, onError, selected]);

  useEffect(
    () => () => {
      stopAudio();
      clipboardWriteGate.cancelPending();
      if (copyFeedbackTimerRef.current != null) {
        window.clearTimeout(copyFeedbackTimerRef.current);
      }
    },
    [clipboardWriteGate, stopAudio],
  );

  useEffect(() => {
    clipboardWriteGate.cancelPending();
    setCopiedDetailTarget(null);
    setCopiedSessionId(null);
    if (copyFeedbackTimerRef.current != null) {
      window.clearTimeout(copyFeedbackTimerRef.current);
      copyFeedbackTimerRef.current = null;
    }
  }, [clipboardWriteGate, selected?.id]);

  function showCopyFeedback(sessionId: string, detailTarget: "main" | "raw" | null) {
    if (copyFeedbackTimerRef.current != null) {
      window.clearTimeout(copyFeedbackTimerRef.current);
    }
    setCopiedDetailTarget(detailTarget);
    setCopiedSessionId(detailTarget ? null : sessionId);
    copyFeedbackTimerRef.current = window.setTimeout(() => {
      setCopiedDetailTarget(null);
      setCopiedSessionId(null);
      copyFeedbackTimerRef.current = null;
    }, 1600);
  }

  async function copySession(
    session: SessionRecord,
    detailTarget: "main" | "raw" | null = null,
    textOverride?: string,
  ) {
    const text = (textOverride ?? sessionMainText(session)).trim();
    if (clipboardWriteGate.isPending()) return;
    setCopyPending(true);
    try {
      const outcome = await clipboardWriteGate.write(text, (value) =>
        navigator.clipboard.writeText(value),
      );
      if (outcome !== "copied") return;
      showCopyFeedback(session.id, detailTarget);
    } catch (e) {
      onError(`复制失败: ${String(e)}`);
    } finally {
      setCopyPending(false);
    }
  }

  async function copyMain() {
    if (!selected) return;
    await copySession(selected, "main");
  }

  async function playAudio() {
    if (!selected?.audio_path) {
      onError("这条记录没有保存录音，无法回听。");
      return;
    }
    onError(null);
    try {
      if (playing) {
        stopAudio();
        return;
      }
      onBusy(true);
      const bytes = await api.getSessionAudio(selected.id);
      const url = URL.createObjectURL(
        new Blob([new Uint8Array(bytes)], { type: "audio/wav" })
      );
      blobUrlRef.current = url;
      const audio = new Audio(url);
      audioRef.current = audio;
      audio.onended = () => setPlaying(false);
      audio.onerror = () => {
        setPlaying(false);
        onError("录音播放失败");
      };
      await audio.play();
      setPlaying(true);
    } catch (e) {
      onError(String(e));
      setPlaying(false);
    } finally {
      onBusy(false);
    }
  }

  async function retry() {
    if (!selected) return;
    onBusy(true);
    onError(null);
    setRetryNote(null);
    try {
      const before = sessionMainText(selected);
      const out = await api.retrySessionTranscription(selected.id);
      onUpdated(out.session);
      onRefresh();
      setAttempts(await api.listSessionAttempts(selected.id, 20));
      const after = sessionMainText(out.session);
      if (out.fallbackReason) {
        setRetryNote(correctorFallbackNotice(out.fallbackReason));
      } else if (after && after !== before) {
        setRetryNote("识别结果已更新");
      } else if (!after) {
        setRetryNote("仍然没有识别出文字，可先听录音确认环境音");
      } else {
        setRetryNote("结果与上次相同");
      }
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  }

  const q = selected ? sessionQuality(selected) : null;
  const needsRecovery = q === "empty" || q === "weak";
  const hasAudio = Boolean(selected?.audio_path);
  const text = selected ? sessionMainText(selected) : "";
  const resultAttempt =
    attempts.find((attempt) => attempt.status === "completed" && attempt.corrected != null) ||
    attempts[0];
  const correctorFallback = resultAttempt?.pipeline_metrics.corrector_fallback === true;
  const correctorFallbackReason = correctorFallback
    ? resultAttempt?.pipeline_metrics.stage_issues.find(
        (issue) => issue.stage === "corrector" && issue.kind === "fallback",
      )?.message || "model_not_applied"
    : null;
  const pipelineAsrText = resultAttempt?.asr_raw || selected?.asr_raw || "";
  const pipelineCorrectedText = resultAttempt?.corrected ?? selected?.corrected ?? "";

  return (
    <div className="split history-layout">
      <section className="card list-pane">
        <div className="card-head">
          <h2>历史</h2>
          <button
            type="button"
            className="icon-btn"
            disabled={busy}
            onClick={onRefresh}
            title="刷新"
            aria-label="刷新"
          >
            <Icon name="refresh" size={16} />
          </button>
        </div>
        {sessions.length === 0 ? (
          <div className="empty-history">
            <p className="empty-history-title">还没有记录</p>
            <p className="muted-text">
              按住热键说一段话，结果会按时间出现在这里。识别不理想时可以回听录音再识别一次。
            </p>
          </div>
        ) : (
          <ul className="session-list">
            {sessions.map((s) => {
              const body = sessionMainText(s);
              const quality = sessionQuality(s);
              const copiedFromList = copiedSessionId === s.id;
              return (
                <li key={s.id} className="session-row">
                  <button
                    type="button"
                    className={[
                      "session-item",
                      selected?.id === s.id ? "active" : "",
                      quality !== "ok" ? "session-item-soft" : "",
                    ]
                      .filter(Boolean)
                      .join(" ")}
                    onClick={() => onSelect(s.id)}
                  >
                    <div className="session-item-top">
                      <span className="list-time">{formatTime(s.created_at)}</span>
                      {quality !== "ok" && (
                        <span className="session-flag">
                          {quality === "empty" ? "无结果" : "偏短"}
                        </span>
                      )}
                    </div>
                    <span
                      className={`session-preview ${quality === "empty" ? "empty" : ""}`}
                    >
                      {quality === "empty" ? "没有识别出文字" : previewText(body, 80)}
                    </span>
                    {s.focus?.app_name ? (
                      <span className="session-context muted-text">{s.focus.app_name}</span>
                    ) : null}
                  </button>
                  <button
                    type="button"
                    className={`icon-btn session-copy ${copiedFromList ? "copied" : ""}`}
                    disabled={!body || copyPending}
                    onClick={() => void copySession(s)}
                    title={copiedFromList ? "已复制" : "复制文本"}
                    aria-label={copiedFromList ? "已复制" : "复制这条文本"}
                  >
                    <Icon name={copiedFromList ? "copy-check" : "copy"} size={15} />
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </section>

      <section className="card detail-pane history-detail">
        {!selected ? (
          <div className="history-empty-detail">
            <p className="empty-history-title">查看某次识别</p>
            <p className="muted-text">从左侧选一条记录。核心是：核对文本、复制，必要时听录音再识别。</p>
          </div>
        ) : (
          <>
            <header className="history-detail-head">
              <div>
                <div className="history-detail-when">{formatTime(selected.created_at)}</div>
                <div className="history-detail-meta muted-text">
                  {[
                    selected.focus?.app_name,
                    formatAsrEngineLabel(selected.asr_engine),
                    selected.corrector_engine && selected.corrector_engine !== "none"
                      ? `修正 ${selected.corrector_engine}`
                      : null,
                  ]
                    .filter(Boolean)
                    .join(" · ") || "本地识别"}
                </div>
              </div>
              <div className="history-toolbar">
                <button
                  type="button"
                  className={`icon-btn ${copiedDetailTarget === "main" ? "copied" : ""}`}
                  disabled={busy || copyPending || !text}
                  onClick={() => void copyMain()}
                  title={
                    copiedDetailTarget === "main" ? "已复制" : "复制文本（双击正文亦可）"
                  }
                  aria-label={copiedDetailTarget === "main" ? "已复制" : "复制文本"}
                >
                  <Icon
                    name={copiedDetailTarget === "main" ? "copy-check" : "copy"}
                    size={16}
                  />
                </button>
                {selected.asr_raw &&
                  selected.asr_raw.trim() &&
                  selected.asr_raw.trim() !== text && (
                    <button
                      type="button"
                      className={`icon-btn ${copiedDetailTarget === "raw" ? "copied" : ""}`}
                      disabled={busy || copyPending}
                      onClick={() =>
                        void copySession(
                          selected,
                          "raw",
                          selected.asr_raw!,
                        )
                      }
                      title={
                        copiedDetailTarget === "raw" ? "已复制原文" : "复制识别原文（未整理）"
                      }
                      aria-label={copiedDetailTarget === "raw" ? "已复制原文" : "复制原文"}
                    >
                      <Icon
                        name={copiedDetailTarget === "raw" ? "copy-check" : "clipboard"}
                        size={16}
                      />
                    </button>
                  )}
                {hasAudio && (
                  <button
                    type="button"
                    className={`icon-btn ${playing ? "active" : ""}`}
                    disabled={busy}
                    onClick={() => void playAudio()}
                    title={playing ? "停止播放" : "听录音"}
                    aria-label={playing ? "停止播放" : "听录音"}
                  >
                    <Icon name={playing ? "stop" : "play"} size={16} />
                  </button>
                )}
                {hasAudio && (
                  <button
                    type="button"
                    className="icon-btn"
                    disabled={busy}
                    onClick={() => void retry()}
                    title={busy ? "识别中…" : "再识别一次"}
                    aria-label="再识别一次"
                  >
                    <Icon name="refresh" size={16} />
                  </button>
                )}
                <button
                  type="button"
                  className="icon-btn danger"
                  disabled={busy}
                  onClick={() => onDelete(selected.id)}
                  title="删除"
                  aria-label="删除"
                >
                  <Icon name="delete" size={16} />
                </button>
              </div>
            </header>

            {correctorFallback && (
              <div className="history-corrector-fallback" role="status">
                <strong>本次 AI 修订未采用</strong>
                <span>
                  {correctorFallbackReasonLabel(correctorFallbackReason)}
                  。已保留基础整理文本，且没有再次调用大模型。
                </span>
              </div>
            )}

            {/* Result first — text is the product */}
            <div
              className={`history-result ${needsRecovery ? "history-result-soft" : ""}`}
              onDoubleClick={() => void copyMain()}
              title="双击复制"
            >
              {text || (
                <span className="muted-text">
                  没有识别出文字。
                  {hasAudio ? "可以先听录音，再点「再识别一次」。" : ""}
                </span>
              )}
            </div>

            {editEvents.length > 0 && (
              <section className="history-edits" aria-label="插入后的用户编辑">
                <div className="history-edits-head">
                  <h3>插入后的编辑</h3>
                  <span className="chip">{editEvents.length}</span>
                </div>
                <p className="muted-text history-edits-note">
                  这里只显示已确认发生在本次插入文本范围内的修改。
                </p>
                <ol className="history-edit-list">
                  {editEvents.map((edit) => (
                    <li key={edit.id} className="history-edit-item">
                      <div className="history-edit-meta">
                        <span>{formatTime(edit.created_at)}</span>
                        <span>
                          {edit.source === "post_paste_ax"
                            ? "目标输入框"
                            : edit.source === "pre_insert_ui"
                              ? "插入前编辑"
                              : "手动记录"}
                        </span>
                      </div>
                      <div className="history-edit-change">
                        <pre>{edit.before_text || "（空）"}</pre>
                        <span aria-hidden="true">→</span>
                        <pre>{edit.after_text || "（已删除）"}</pre>
                      </div>
                    </li>
                  ))}
                </ol>
              </section>
            )}

            {editObservations.length > 0 && (
              <details
                className={`history-observations ${
                  editObservations.some((observation) => observation.status === "failed")
                    ? "has-failure"
                    : ""
                }`}
              >
                <summary>
                  <span>
                    {editObservations.some((observation) => observation.status === "failed")
                      ? "编辑反馈跟踪有失败"
                      : "编辑反馈跟踪已完成"}
                  </span>
                  <span className="chip">{editObservations.length}</span>
                </summary>
                <p className="muted-text history-edits-note">
                  这里记录观察终态，用来区分用户未修改和系统未能继续跟踪。
                </p>
                <ol className="history-edit-list">
                  {editObservations.map((observation) => (
                    <li
                      key={observation.id}
                      className={`history-edit-item observation-${observation.status}`}
                    >
                      <div className="history-edit-meta">
                        <span>{formatTime(observation.completed_at)}</span>
                        <span>
                          {observation.status === "completed_with_edit"
                            ? "已捕获修改"
                            : observation.status === "completed_no_edit"
                              ? "观察期内未修改"
                              : "跟踪失败"}
                        </span>
                      </div>
                      <div className="muted-text">
                        结束原因：{editObservationReasonLabel(observation.end_reason)}
                        {observation.normalized_edit_distance != null
                          ? ` · 编辑距离 ${observation.normalized_edit_distance.toFixed(2)}`
                          : ""}
                      </div>
                    </li>
                  ))}
                </ol>
              </details>
            )}

            {/* Recovery path: emphasized only when quality is bad */}
            {needsRecovery && hasAudio && (
              <div className="history-recover" role="status">
                <p className="history-recover-text">
                  {q === "empty"
                    ? "这次几乎没有可用文本。建议先听录音，确认说清楚了再识别一次。"
                    : "结果偏短，可能是误触或环境噪声。听一下录音，再决定是否重新识别。"}
                </p>
                <div className="history-recover-actions">
                  <button
                    type="button"
                    className="icon-btn-label primary"
                    disabled={busy}
                    onClick={() => void playAudio()}
                  >
                    <Icon name={playing ? "stop" : "play"} size={15} />
                    {playing ? "停止播放" : "听录音"}
                  </button>
                  <button
                    type="button"
                    className="icon-btn-label primary"
                    disabled={busy}
                    onClick={() => void retry()}
                  >
                    <Icon name="refresh" size={15} />
                    {busy ? "识别中…" : "再识别一次"}
                  </button>
                </div>
              </div>
            )}

            {retryNote && <p className="history-retry-note">{retryNote}</p>}

            {!hasAudio && (
              <p className="muted-text history-no-audio">未保存录音 · 无法回听或重识别</p>
            )}

            {/* Pipeline detail is secondary — for power users */}
            {(pipelineAsrText || pipelineCorrectedText) && (
              <div className="history-pipeline">
                <button
                  type="button"
                  className="linkish"
                  onClick={() => setShowPipeline((v) => !v)}
                >
                  {showPipeline ? "收起识别过程" : "识别过程"}
                </button>
                {showPipeline && (
                  <div className="history-pipeline-body">
                    <div>
                      <div className="field-label">ASR 原始转写</div>
                      <pre className="field-value">{pipelineAsrText || "—"}</pre>
                    </div>
                    {pipelineCorrectedText &&
                      (correctorFallback || pipelineCorrectedText !== pipelineAsrText) && (
                      <div>
                        <div className="field-label">
                          {correctorFallback ? "最终文本（AI 修订回退）" : "修正后"}
                        </div>
                        <pre className="field-value">{pipelineCorrectedText}</pre>
                      </div>
                    )}
                  </div>
                )}
              </div>
            )}
          </>
        )}
      </section>
    </div>
  );
}

function DictionaryPanel({
  entries,
  termInput,
  fromInput,
  toInput,
  busy,
  onTermInput,
  onFromInput,
  onToInput,
  onAddTerm,
  onAddReplacement,
  onDelete,
}: {
  entries: DictionaryEntry[];
  termInput: string;
  fromInput: string;
  toInput: string;
  busy: boolean;
  onTermInput: (v: string) => void;
  onFromInput: (v: string) => void;
  onToInput: (v: string) => void;
  onAddTerm: () => void;
  onAddReplacement: () => void;
  onDelete: (id: string) => void;
}) {
  return (
    <>
      <section className="card">
        <h2>添加术语</h2>
        <div className="form-row">
          <input
            className="input"
            placeholder="如 Morpho、GPT-4"
            value={termInput}
            onChange={(e) => onTermInput(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && onAddTerm()}
          />
          <button
            type="button"
            className="btn"
            disabled={busy || !termInput.trim()}
            onClick={onAddTerm}
          >
            添加 term
          </button>
        </div>
      </section>

      <section className="card">
        <h2>添加替换规则</h2>
        <div className="form-row">
          <input
            className="input"
            placeholder="from（识别错）"
            value={fromInput}
            onChange={(e) => onFromInput(e.target.value)}
          />
          <span className="arrow">→</span>
          <input
            className="input"
            placeholder="to（正确）"
            value={toInput}
            onChange={(e) => onToInput(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && onAddReplacement()}
          />
          <button
            type="button"
            className="btn"
            disabled={busy || !fromInput.trim() || !toInput.trim()}
            onClick={onAddReplacement}
          >
            添加 replacement
          </button>
        </div>
      </section>

      <section className="card">
        <h2>词条 ({entries.length})</h2>
        {entries.length === 0 ? (
          <p className="muted-text">词典为空。先添加术语或从「编辑学习」确认候选。</p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>类型</th>
                <th>内容</th>
                <th>来源</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {entries.map((e) => (
                <tr key={e.id}>
                  <td>
                    <span className="chip">{e.kind}</span>
                  </td>
                  <td>
                    {e.kind === "term"
                      ? e.term
                      : `${e.from_text ?? ""} → ${e.to_text ?? ""}`}
                  </td>
                  <td className="muted-text">{e.source}</td>
                  <td>
                    <button
                      type="button"
                      className="btn small danger"
                      disabled={busy}
                      onClick={() => onDelete(e.id)}
                    >
                      删除
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </>
  );
}

function LearnPanel({
  before,
  after,
  candidates,
  sessionId,
  busy,
  onBefore,
  onAfter,
  onSuggest,
  onConfirm,
  onReject,
}: {
  before: string;
  after: string;
  candidates: LearnCandidate[];
  sessionId?: string;
  busy: boolean;
  onBefore: (v: string) => void;
  onAfter: (v: string) => void;
  onSuggest: () => void;
  onConfirm: (c: LearnCandidate) => void;
  onReject: (c: LearnCandidate) => void;
}) {
  return (
    <section className="card">
      <h2>从编辑生成候选</h2>
      <p className="muted-text">
        只建议短词/短语级改动；确认后写入词典。可开「自动晋升」：同一替换出现 N 次后自动入库。
        {sessionId ? (
          <>
            {" "}
            关联会话 <code>{sessionId.slice(0, 8)}…</code>
          </>
        ) : null}
      </p>
      <div className="learn-grid">
        <label>
          <span>修改前（ASR / 修正稿）</span>
          <textarea
            className="textarea"
            rows={3}
            value={before}
            onChange={(e) => onBefore(e.target.value)}
            placeholder="脱肯"
          />
        </label>
        <label>
          <span>修改后（用户终稿）</span>
          <textarea
            className="textarea"
            rows={3}
            value={after}
            onChange={(e) => onAfter(e.target.value)}
            placeholder="Token"
          />
        </label>
      </div>
      <div className="actions">
        <button
          type="button"
          className="btn"
          disabled={busy || !before.trim() || !after.trim()}
          onClick={onSuggest}
        >
          生成候选
        </button>
      </div>

      {candidates.length > 0 && (
        <>
          <h3 className="subhead">候选</h3>
          <ul className="list">
            {candidates.map((c, i) => (
              <li key={i} className="candidate">
                <div>
                  <span className="chip">{c.kind}</span>{" "}
                  {c.kind === "term"
                    ? c.term
                    : `${c.from_text ?? ""} → ${c.to_text ?? ""}`}
                  <div className="muted-text">{c.reason}</div>
                </div>
                <button
                  type="button"
                  className="btn small"
                  disabled={busy}
                  onClick={() => onConfirm(c)}
                >
                  确认加入词典
                </button>
                <button
                  type="button"
                  className="btn small ghost"
                  disabled={busy}
                  onClick={() => onReject(c)}
                >
                  忽略
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}
