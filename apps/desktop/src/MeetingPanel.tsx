import type { ChangeEvent, MutableRefObject, ReactNode } from "react";
import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { convertFileSrc } from "@tauri-apps/api/core";
import { api } from "./api";
import {
  useMeetingModels,
  type MeetingModels,
  type ModelProgressState,
  type ModelTarget,
} from "./meetingModels";
import { Icon } from "./Icons";
import { diarGuidance, isNoLlmMarker } from "./meetingGuidance";
import type {
  ActionItem,
  EnrolledSpeaker,
  ExportPreset,
  LiveAnnotation,
  Meeting,
  MeetingDetail,
  MeetingStatus,
  Minutes,
  Speaker,
  SourceRef,
  TabId,
  TranscriptSegment,
} from "./types";

// ---- formatting helpers -------------------------------------------------

const STATUS_META: Record<
  MeetingStatus,
  { label: string; tone: "live" | "work" | "done" | "bad" }
> = {
  recording: { label: "录制中", tone: "live" },
  processing: { label: "处理中", tone: "work" },
  transcribing: { label: "转录中", tone: "work" },
  summarizing: { label: "总结中", tone: "work" },
  ready: { label: "完成", tone: "done" },
  failed: { label: "失败", tone: "bad" },
};

const IN_PROGRESS: MeetingStatus[] = [
  "recording",
  "processing",
  "transcribing",
  "summarizing",
];

function isInProgress(status: MeetingStatus): boolean {
  return IN_PROGRESS.includes(status);
}

function meetingTitle(m: Meeting): string {
  const t = m.title?.trim();
  return t && t.length > 0 ? t : "未命名会议";
}

function pad2(n: number): string {
  return n < 10 ? `0${n}` : String(n);
}

/** mm:ss (or h:mm:ss) for a duration in seconds. */
function formatDuration(seconds?: number | null): string {
  if (seconds == null || !Number.isFinite(seconds) || seconds <= 0) return "—";
  const s = Math.round(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  return h > 0 ? `${h}:${pad2(m)}:${pad2(sec)}` : `${m}:${pad2(sec)}`;
}

/** mm:ss timestamp used for click-to-jump labels. */
function formatClock(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  return h > 0 ? `${h}:${pad2(m)}:${pad2(sec)}` : `${pad2(m)}:${pad2(sec)}`;
}

function startOfDay(d: Date): number {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
}

/** 今天 / 昨天 / 更早 grouping key for the list. */
function dayGroup(iso: string): "today" | "yesterday" | "earlier" {
  const then = new Date(iso);
  if (Number.isNaN(then.getTime())) return "earlier";
  const today = startOfDay(new Date());
  const day = startOfDay(then);
  const oneDay = 86_400_000;
  if (day === today) return "today";
  if (day === today - oneDay) return "yesterday";
  return "earlier";
}

const GROUP_LABEL: Record<"today" | "yesterday" | "earlier", string> = {
  today: "今天",
  yesterday: "昨天",
  earlier: "更早",
};

function formatTimeOfDay(iso: string): string {
  try {
    return new Date(iso).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
  } catch {
    return iso;
  }
}

function formatFullDate(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

/**
 * A speaker's display: confirmed speakers show their name; an unconfirmed
 * cluster shows its engine label and is flagged so the UI never presents a
 * tentative cluster as a real identity (docs/MEETING_M4_UX.md, 说话人修正).
 */
function speakerDisplay(speaker?: Speaker | null): {
  name: string;
  confirmed: boolean;
} {
  if (!speaker) return { name: "未知说话人", confirmed: false };
  const name = speaker.display_name?.trim();
  if (name && name.length > 0) return { name, confirmed: true };
  return { name: speaker.label, confirmed: false };
}

/** True when the meeting is `ready` but its minutes were skipped because no LLM
 * was configured (the backend leaves a sentinel `summary` row). */
function noLlmMinutes(detail: MeetingDetail): boolean {
  const row = detail.summaries.find((s) => s.kind === "summary");
  return row != null && isNoLlmMarker(row.content);
}

/** Parse the structured Minutes JSON out of the `summary`-kind row. Returns
 * null for the no-LLM sentinel so it is never rendered as real minutes. */
function parseMinutes(detail: MeetingDetail): Minutes | null {
  const row = detail.summaries.find((s) => s.kind === "summary");
  if (!row) return null;
  if (isNoLlmMarker(row.content)) return null;
  try {
    const raw = JSON.parse(row.content) as Partial<Minutes>;
    return {
      one_liner: raw.one_liner ?? "",
      decisions: raw.decisions ?? [],
      action_items: raw.action_items ?? [],
      discussion: raw.discussion ?? [],
      open_questions: raw.open_questions ?? [],
    };
  } catch {
    return null;
  }
}

// ---- panel root ---------------------------------------------------------

export function MeetingPanel({
  onError,
  onNavigate,
}: {
  onError: (e: string | null) => void;
  /** Switch the top-level app tab (used by the "配置 LLM" / "去设置" links). */
  onNavigate?: (tab: TabId) => void;
}) {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  // App-level model state (from MeetingModelsProvider at the App root). It is
  // not tied to this panel's lifetime, so a running (~1GB) download's
  // progress/cancel survive switching tabs away and back, and the library and
  // detail view always share one listener and one in-flight download.
  const models = useMeetingModels();

  if (selectedId) {
    return (
      <MeetingDetailView
        meetingId={selectedId}
        onBack={() => setSelectedId(null)}
        onError={onError}
        onNavigate={onNavigate}
        models={models}
      />
    );
  }
  return (
    <MeetingLibrary onOpen={setSelectedId} onError={onError} models={models} />
  );
}

// ---- library (high-density list, non-card) ------------------------------

type FilterId = "all" | "in_progress" | "ready" | "failed";

const FILTERS: { id: FilterId; label: string }[] = [
  { id: "all", label: "全部" },
  { id: "in_progress", label: "处理中" },
  { id: "ready", label: "已完成" },
  { id: "failed", label: "失败" },
];

function matchesFilter(status: MeetingStatus, filter: FilterId): boolean {
  switch (filter) {
    case "all":
      return true;
    case "in_progress":
      return isInProgress(status);
    case "ready":
      return status === "ready";
    case "failed":
      return status === "failed";
  }
}

function MeetingLibrary({
  onOpen,
  onError,
  models,
}: {
  onOpen: (id: string) => void;
  onError: (e: string | null) => void;
  models: MeetingModels;
}) {
  const [meetings, setMeetings] = useState<Meeting[]>([]);
  const [filter, setFilter] = useState<FilterId>("all");
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(false);
  // Local recording handle set on start; `null` when idle. Kept alongside the
  // list-derived recording row so the recording bar keeps an accurate start
  // time even before the next poll reflects the new meeting.
  const [active, setActive] = useState<{ id: string; startedAtMs: number } | null>(
    null,
  );
  const [titleDraft, setTitleDraft] = useState("");
  const [starting, setStarting] = useState(false);

  const refresh = useCallback(
    async (q: string) => {
      try {
        const trimmed = q.trim();
        const rows = await api.listMeetings({
          query: trimmed || undefined,
          limit: 200,
        });
        setMeetings(rows);
      } catch (e) {
        onError(String(e));
      }
    },
    [onError],
  );

  // Initial load + debounced reload on search.
  useEffect(() => {
    setLoading(true);
    const t = window.setTimeout(() => {
      void refresh(query).finally(() => setLoading(false));
    }, 220);
    return () => window.clearTimeout(t);
  }, [query, refresh]);

  // Light polling while anything is still processing, so 转录中/总结中 advance
  // without a manual refresh.
  const anyInProgress = meetings.some((m) => isInProgress(m.status));
  useEffect(() => {
    if (!anyInProgress) return;
    const id = window.setInterval(() => void refresh(query), 6000);
    return () => window.clearInterval(id);
  }, [anyInProgress, query, refresh]);

  const counts = useMemo(() => {
    const c: Record<FilterId, number> = {
      all: meetings.length,
      in_progress: 0,
      ready: 0,
      failed: 0,
    };
    for (const m of meetings) {
      if (isInProgress(m.status)) c.in_progress += 1;
      else if (m.status === "ready") c.ready += 1;
      else if (m.status === "failed") c.failed += 1;
    }
    return c;
  }, [meetings]);

  // A backend meeting still in `recording` status is the source of truth for
  // "we are recording"; `active` supplies the accurate wall-clock start time.
  const listRecording = meetings.find((m) => m.status === "recording") ?? null;
  const recording =
    active ??
    (listRecording
      ? {
          id: listRecording.id,
          startedAtMs: Date.parse(listRecording.created_at),
        }
      : null);

  const startMeeting = useCallback(async () => {
    setStarting(true);
    onError(null);
    try {
      const id = await api.startMeetingRecording(titleDraft.trim() || undefined);
      setActive({ id, startedAtMs: Date.now() });
      setTitleDraft("");
      await refresh(query);
      // Land the user straight in the two-pane recording view. The library
      // still derives its own "录制中" indicator from the meeting row, so
      // navigating back (← 会议库) shows the recording and can re-open it.
      onOpen(id);
    } catch (e) {
      onError(String(e));
    } finally {
      setStarting(false);
    }
  }, [titleDraft, refresh, query, onError, onOpen]);

  const onStopped = useCallback(() => {
    setActive(null);
    void refresh(query);
  }, [refresh, query]);

  // Rename a meeting in place, then refresh so the row shows the new title.
  const renameMeeting = useCallback(
    async (id: string, title: string) => {
      onError(null);
      try {
        await api.renameMeeting(id, title);
        await refresh(query);
      } catch (e) {
        onError(String(e));
      }
    },
    [refresh, query, onError],
  );

  // Delete flow: a row asks to delete → we hold the target here and render a
  // confirmation dialog; only an explicit confirm calls the backend.
  const [pendingDelete, setPendingDelete] = useState<Meeting | null>(null);
  const [deleting, setDeleting] = useState(false);
  const confirmDelete = useCallback(async () => {
    if (!pendingDelete) return;
    setDeleting(true);
    onError(null);
    try {
      await api.deleteMeeting(pendingDelete.id);
      setPendingDelete(null);
      await refresh(query);
    } catch (e) {
      onError(String(e));
    } finally {
      setDeleting(false);
    }
  }, [pendingDelete, refresh, query, onError]);

  const visible = meetings.filter((m) => matchesFilter(m.status, filter));

  // Group by day, preserving the newest-first order the backend returns.
  const groups: { key: "today" | "yesterday" | "earlier"; items: Meeting[] }[] =
    [];
  for (const key of ["today", "yesterday", "earlier"] as const) {
    const items = visible.filter((m) => dayGroup(m.created_at) === key);
    if (items.length > 0) groups.push({ key, items });
  }

  return (
    <div className="split meeting-layout">
      <aside className="card meeting-filters">
        <div className="card-head">
          <h2>会议库</h2>
          <button
            type="button"
            className="icon-btn"
            disabled={loading}
            onClick={() => void refresh(query)}
            title="刷新"
            aria-label="刷新"
          >
            <Icon name="refresh" size={16} />
          </button>
        </div>
        <label className="meeting-search">
          <Icon name="search" size={15} />
          <input
            className="meeting-search-input"
            type="search"
            value={query}
            placeholder="搜索标题…"
            onChange={(e) => setQuery(e.target.value)}
          />
        </label>
        <nav className="meeting-filter-list" aria-label="会议过滤">
          {FILTERS.map((f) => (
            <button
              key={f.id}
              type="button"
              className={`meeting-filter ${filter === f.id ? "active" : ""}`}
              onClick={() => setFilter(f.id)}
            >
              <span>{f.label}</span>
              <span className="meeting-filter-count">{counts[f.id]}</span>
            </button>
          ))}
        </nav>
        <p className="muted-text meeting-filter-note">
          收藏与标签暂未开放（需后端 favorite 字段，推迟到后续迭代）。
        </p>
      </aside>

      <section className="card meeting-list-pane">
        <div className="meeting-start-bar">
          {recording ? (
            <RecordingBar
              meetingId={recording.id}
              startedAtMs={recording.startedAtMs}
              onStopped={onStopped}
              onError={onError}
              models={models}
            />
          ) : (
            <form
              className="meeting-start"
              onSubmit={(e) => {
                e.preventDefault();
                void startMeeting();
              }}
            >
              <input
                className="meeting-start-title"
                type="text"
                value={titleDraft}
                placeholder="会议标题（可选）"
                disabled={starting}
                onChange={(e) => setTitleDraft(e.target.value)}
              />
              <button
                type="submit"
                className="btn meeting-start-btn"
                disabled={starting}
              >
                <Icon name="mic" size={16} />
                {starting ? "正在开始…" : "开始会议"}
              </button>
            </form>
          )}
        </div>
        <MeetingModelSetup models={models} />
        {visible.length === 0 ? (
          <div className="meeting-empty">
            <p className="empty-history-title">
              {loading ? "正在加载…" : "没有会议"}
            </p>
            <p className="muted-text">
              {query.trim()
                ? "没有匹配的会议标题。"
                : "点上方“开始会议”录一场会议，停止后它会按时间出现在这里。"}
            </p>
          </div>
        ) : (
          <div className="meeting-groups">
            {groups.map((g) => (
              <div key={g.key} className="meeting-group">
                <div className="meeting-group-head">{GROUP_LABEL[g.key]}</div>
                <ul className="meeting-rows">
                  {g.items.map((m) => (
                    <li key={m.id}>
                      <MeetingRow
                        meeting={m}
                        onOpen={() => onOpen(m.id)}
                        onRename={(title) => renameMeeting(m.id, title)}
                        onRequestDelete={() => setPendingDelete(m)}
                      />
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>
        )}
      </section>

      {pendingDelete && (
        <ConfirmDialog
          title="删除会议？"
          message={`「${meetingTitle(pendingDelete)}」将被永久删除，包括逐字稿、说话人、纪要与录音文件。此操作不可撤销。`}
          confirmLabel="删除"
          busy={deleting}
          onConfirm={() => void confirmDelete()}
          onCancel={() => setPendingDelete(null)}
        />
      )}
    </div>
  );
}

// ---- meeting model install (Paraformer) --------------------------------
// The download state lives app-level in `./meetingModels` (survives tab
// switches); these are just the cards/rows that render it. Streaming =
// record-time preview (~1GB); offline = word-timestamped final transcript +
// speaker alignment (optional — dictation engine still produces a transcript
// without it).

const PHASE_LABEL: Record<string, string> = {
  waiting: "排队中",
  downloading: "下载中",
  extracting: "解压中",
  done: "完成",
  error: "出错",
};

function ModelProgressBar({ progress }: { progress: ModelProgressState }) {
  const pct = progress.percent;
  const label = PHASE_LABEL[progress.phase] ?? progress.phase;
  return (
    <div className="meeting-model-progress">
      <div className="meeting-model-bar" aria-hidden>
        <div
          className={`meeting-model-bar-fill ${pct == null ? "indeterminate" : ""}`}
          style={{ width: pct == null ? "100%" : `${Math.min(100, pct)}%` }}
        />
      </div>
      <span className="muted-text meeting-model-progress-text">
        {label}
        {progress.message ? ` · ${progress.message}` : ""}
        {pct != null ? ` · ${pct.toFixed(0)}%` : ""}
      </span>
    </div>
  );
}

function ModelRow({
  title,
  desc,
  ready,
  target,
  models,
}: {
  title: string;
  desc: string;
  ready: boolean;
  target: ModelTarget;
  models: MeetingModels;
}) {
  const downloading = models.active === target;
  const otherBusy = models.active !== null && !downloading;
  return (
    <div className="meeting-model-row">
      <div className="meeting-model-row-main">
        <div className="meeting-model-row-title">
          <span>{title}</span>
          <span className={`meeting-model-badge ${ready ? "ok" : ""}`}>
            {ready ? "已安装" : "未安装"}
          </span>
        </div>
        <p className="muted-text meeting-model-row-desc">{desc}</p>
        {downloading && models.progress && (
          <ModelProgressBar progress={models.progress} />
        )}
      </div>
      <div className="meeting-model-row-actions">
        {ready ? (
          <span className="muted-text meeting-model-done">✓</span>
        ) : downloading ? (
          <button
            type="button"
            className="btn ghost small"
            onClick={() => void models.cancel()}
          >
            取消
          </button>
        ) : (
          <button
            type="button"
            className="btn small"
            disabled={otherBusy}
            onClick={() => void models.download(target)}
          >
            下载模型
          </button>
        )}
      </div>
    </div>
  );
}

/** Install card shown near the meeting start bar. Collapses to a one-line
 * "ready" note once both models are installed; hidden entirely until status
 * loads. */
function MeetingModelSetup({ models }: { models: MeetingModels }) {
  const { status } = models;
  if (!status) return null;
  const streamingReady = status.paraformerStreamingReady;
  const offlineReady = status.paraformerOfflineReady;
  if (streamingReady && offlineReady) {
    return (
      <p className="muted-text meeting-model-ready">会议模型已就绪</p>
    );
  }
  return (
    <div className="card meeting-model-setup">
      <div className="meeting-model-head">
        <Icon name="mic" size={14} />
        <span>会议模型</span>
        <span className="muted-text meeting-model-head-note">
          会议转写需要 Paraformer 模型（听写无需）
        </span>
      </div>
      <ModelRow
        title="实时预览 · Paraformer streaming"
        desc="录制中实时逐字稿预览。安装包较大（约 1GB），下载时可取消。"
        ready={streamingReady}
        target="streaming"
        models={models}
      />
      <ModelRow
        title="离线终稿 · Paraformer offline（可选）"
        desc="带词级时间戳的离线终稿 + 说话人对齐；不装也能靠听写引擎出稿。"
        ready={offlineReady}
        target="offline"
        models={models}
      />
      {models.error && (
        <p className="meeting-model-error">{models.error}</p>
      )}
    </div>
  );
}

// ---- inline recording state --------------------------------------------
// A deliberately restrained recording strip: ● 正在录制 + elapsed timer + stop,
// plus a rolling **live transcript** (P3) beneath it when the streaming
// Paraformer model is installed (macOS). The live text is an unpolished,
// speaker-less preview — the authoritative, speaker-attributed transcript is
// produced by the offline pipeline after stop and shown on the detail page. No
// in-meeting editing and no mic level (there is no meeting-recorder level
// interface yet).
//
// Intentionally **start/stop only — no pause/resume here**. This bar is
// reconstructed from the backend `recording` row on remount (only the meeting
// id + start time survive), so it cannot know a pause state; showing a pause
// button would let "paused" be silently lost across a tab switch and mislead
// the user with a running clock. Proper pause/resume (with backend-reported
// elapsed that excludes paused gaps) belongs to the full recording window; the
// `pause_meeting_recording` / `resume_meeting_recording` commands stay wired in
// `api.ts` for it. The elapsed clock here is plain wall-clock since start, which
// is exact without pauses; the authoritative duration is set at stop.
function RecordingBar({
  meetingId,
  startedAtMs,
  onStopped,
  onError,
  models,
}: {
  meetingId: string;
  startedAtMs: number;
  onStopped: () => void;
  onError: (e: string | null) => void;
  models: MeetingModels;
}) {
  // Seconds elapsed since start. Seed from the (possibly reconstructed) start
  // time so a remount mid-recording shows the right clock.
  const initial = Number.isFinite(startedAtMs)
    ? Math.max(0, Math.floor((Date.now() - startedAtMs) / 1000))
    : 0;
  const [seconds, setSeconds] = useState(initial);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const id = window.setInterval(() => setSeconds((s) => s + 1), 1000);
    return () => window.clearInterval(id);
  }, []);

  async function stop() {
    setBusy(true);
    onError(null);
    try {
      await api.stopMeetingRecording(meetingId);
      onStopped();
    } catch (e) {
      onError(String(e));
      // Even on a stop error the backend restores the mic/hotkey; clear the
      // recording UI so the user is not stuck with a dead bar.
      onStopped();
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="meeting-recording-wrap">
      <div className="meeting-recording">
        <span className="meeting-rec-dot" aria-hidden />
        <span className="meeting-rec-label">正在录制</span>
        <span className="meeting-rec-timer" aria-live="off">
          {formatClock(seconds)}
        </span>
        <span
          className="meeting-rec-hint muted-text"
          title="会议模式独占麦克风，已暂挂全局听写热键以免录制时误触；停止后自动恢复。"
        >
          录制中 · 听写快捷键暂停
        </span>
        <span className="meeting-rec-actions">
          <button
            type="button"
            className="btn danger small"
            disabled={busy}
            onClick={() => void stop()}
          >
            <Icon name="stop" size={15} />
            停止
          </button>
        </span>
      </div>
      <LiveTranscript meetingId={meetingId} models={models} onError={onError} />
    </div>
  );
}

// Rolling live transcript (P3). Listens for `meeting-live-transcript` events
// emitted by the streaming worker while recording. Events are **revisable
// segments** on the meeting's unified timeline: each carries a stable
// `segmentId` per utterance per track (mic = 现场, system = 远端), partials
// re-emit the same id with an increasing `revision` (rendered greyed,
// updated in place), and the finalizing event fixes the line (an empty final
// retracts it). Rendering keeps a Map of the latest event per segment,
// ordered by `startSeconds`. If the streaming model is not installed (or the
// platform is not macOS), no events ever arrive and a muted hint is shown
// instead — the recording and the offline final transcript are unaffected
// either way.
/** Voiceprint speaker attribution appended by the live verifier (L3) as an
 * extra revision of a finalized segment. Manual chip annotations (L2) always
 * take display precedence over this. */
type LiveSpeakerAttribution = {
  /** Enrolled identity id. Absent for a session-voiceprint hit (L3.5): the
   * name was seeded by a manual annotation this meeting only, with no
   * permanent identity behind it. */
  identityId?: string;
  displayName: string;
  source: "voiceprint";
  /** true → tentative ("李明?"), false → auto-verified. */
  provisional: boolean;
};

type LiveEvent = {
  meetingId: string;
  segmentId: string;
  revision: number;
  track: "mic" | "system";
  startSeconds: number;
  endSeconds?: number;
  text: string;
  isFinal: boolean;
  /** Set only on the verifier's attribution revision; absent otherwise. */
  speaker?: LiveSpeakerAttribution;
};

const LIVE_TRACK_LABEL: Record<LiveEvent["track"], string> = {
  mic: "现场",
  system: "远端",
};

// Front-end mirror of the reconciliation matching rules (annotate.rs). Used
// only to label chips locally — the authoritative application happens offline
// after stop.
//
// A closed annotation (仅此句) covers a line when their overlap reaches 50%
// of the line's span OR of the annotation's own span (symmetric — matches the
// backend rule that survives diarization merging lines into longer turns).
function closedAnnotationCoversLine(
  a: LiveAnnotation,
  seg: LiveEvent,
): boolean {
  if (a.channel !== seg.track || a.end_seconds == null) return false;
  const start = seg.startSeconds;
  const end = seg.endSeconds ?? seg.startSeconds;
  const length = end - start;
  if (!(length > 0)) return false;
  const overlap =
    Math.min(a.end_seconds, end) - Math.max(a.start_seconds, start);
  if (!(overlap > 0)) return false;
  const aLength = a.end_seconds - a.start_seconds;
  return overlap / length >= 0.5 || (aLength > 0 && overlap / aLength >= 0.5);
}

/** The annotation currently labeling a live line, if any. A closed annotation
 * covering the line wins (newest first); otherwise the line inherits the
 * open-ended (此句及之后) annotation with the greatest start at or before it
 * on the same track — i.e. "this person speaks from here until the next
 * open-ended mark". */
function annotationForLine(
  annotations: LiveAnnotation[],
  seg: LiveEvent,
): LiveAnnotation | null {
  let closed: LiveAnnotation | null = null;
  for (const a of annotations) {
    if (!closedAnnotationCoversLine(a, seg)) continue;
    if (!closed || a.created_at >= closed.created_at) closed = a;
  }
  if (closed) return closed;
  const end = seg.endSeconds ?? seg.startSeconds;
  let open: LiveAnnotation | null = null;
  for (const a of annotations) {
    if (a.channel !== seg.track || a.end_seconds != null) continue;
    if (a.start_seconds > end) continue;
    if (
      !open ||
      a.start_seconds > open.start_seconds ||
      (a.start_seconds === open.start_seconds &&
        a.created_at >= open.created_at)
    ) {
      open = a;
    }
  }
  return open;
}

/** The annotations anchored ON this line (for 清除): closed ones covering it
 * plus open-ended ones whose start falls inside it. Inherited open ranges
 * from earlier lines are excluded — clearing a line must not silently strip
 * a mark the user made elsewhere; clearing an open annotation's own line
 * removes its whole onward range, which is the intent there. */
function annotationsAnchoredOnLine(
  annotations: LiveAnnotation[],
  seg: LiveEvent,
): LiveAnnotation[] {
  const start = seg.startSeconds;
  const end = seg.endSeconds ?? seg.startSeconds;
  return annotations.filter((a) => {
    if (a.channel !== seg.track) return false;
    if (a.end_seconds == null) {
      return a.start_seconds >= start && a.start_seconds <= end;
    }
    return closedAnnotationCoversLine(a, seg);
  });
}

/** Scope of a new annotation: this line only, or from this line onward. */
type AnnotateScope = "onward" | "line";

function LiveTranscript({
  meetingId,
  models,
  onError,
}: {
  meetingId: string;
  models: MeetingModels;
  onError: (e: string | null) => void;
}) {
  // Latest event per segmentId (revision-guarded).
  const [segments, setSegments] = useState<Map<string, LiveEvent>>(
    () => new Map(),
  );
  // Whether we've received any event at all (distinguishes "no model" from
  // "recording started, nothing said yet").
  const [active, setActive] = useState(false);
  const scrollRef = useRef<HTMLDivElement | null>(null);

  // ---- live speaker annotation chips (L2) -------------------------------
  // Enrolled identities feed the chip menu; stored annotations label the
  // chips (and survive a remount mid-recording). Every pick is persisted
  // immediately; the offline pipeline applies it to the final transcript.
  const [identities, setIdentities] = useState<EnrolledSpeaker[]>([]);
  const [annotations, setAnnotations] = useState<LiveAnnotation[]>([]);
  const [selfIdentityId, setSelfIdentityId] = useState<string | null>(null);
  const [menuFor, setMenuFor] = useState<string | null>(null);
  // Viewport (fixed) coordinates of the open menu, measured from the chip at
  // open time. Fixed positioning escapes the scroll container's clipping;
  // the menu opens downward by default and flips upward only when the space
  // below is too tight — so the first and last lines are both fully usable.
  const [menuPos, setMenuPos] = useState<{
    left: number;
    top?: number;
    bottom?: number;
  } | null>(null);
  // "此句及之后" (default: consecutive speech is the common case) vs 仅此句.
  const [scope, setScope] = useState<AnnotateScope>("onward");
  const [customFor, setCustomFor] = useState<string | null>(null);
  const [customName, setCustomName] = useState("");

  useEffect(() => {
    let disposed = false;
    // An empty/missing identity library is a normal state — the chip still
    // offers ad-hoc names — so a failed list only degrades the menu.
    void api
      .listEnrolledSpeakers()
      .then((list) => {
        if (!disposed) setIdentities(list);
      })
      .catch(() => {});
    // "这是我" rendering hint; a failed read just shows the real name.
    void api
      .getSelfIdentity()
      .then((id) => {
        if (!disposed) setSelfIdentityId(id);
      })
      .catch(() => {});
    void api
      .listLiveAnnotations(meetingId)
      .then((rows) => {
        if (!disposed) setAnnotations(rows);
      })
      .catch((e) => onError(String(e)));
    return () => {
      disposed = true;
    };
  }, [meetingId, onError]);

  const closeMenus = useCallback(() => {
    setMenuFor(null);
    setMenuPos(null);
    setScope("onward");
    setCustomFor(null);
    setCustomName("");
  }, []);

  /** Open the annotate menu for a line, positioned from the chip's viewport
   * rect: downward by default, upward only when the space below is tight. */
  const openMenu = useCallback((seg: LiveEvent, chip: HTMLElement) => {
    const rect = chip.getBoundingClientRect();
    // Generous estimate of the tallest menu (scope row + names + input).
    const estimatedHeight = 260;
    const spaceBelow = window.innerHeight - rect.bottom;
    const openDown = spaceBelow >= estimatedHeight || spaceBelow >= rect.top;
    const left = Math.max(
      8,
      Math.min(rect.left, window.innerWidth - 8 - 160),
    );
    setMenuFor(seg.segmentId);
    setMenuPos(
      openDown
        ? { left, top: rect.bottom + 4 }
        : { left, bottom: window.innerHeight - rect.top + 4 },
    );
    setScope("onward");
    setCustomFor(null);
    setCustomName("");
  }, []);

  const annotate = useCallback(
    async (
      seg: LiveEvent,
      displayName: string,
      annotateScope: AnnotateScope,
      identityId?: string,
    ) => {
      closeMenus();
      try {
        const row = await api.annotateLiveSegment({
          meetingId,
          segmentId: seg.segmentId,
          startSeconds: seg.startSeconds,
          // "此句及之后" stores an open-ended annotation (no end): it holds
          // from this line's start until the next open-ended mark on the
          // track — later lines inherit it (see `annotationForLine`).
          endSeconds:
            annotateScope === "line" ? (seg.endSeconds ?? null) : null,
          channel: seg.track,
          identityId: identityId ?? null,
          displayName,
        });
        // The chip shows the name immediately from local state — no backend
        // event round trip.
        setAnnotations((prev) => [...prev, row]);
      } catch (e) {
        onError(String(e));
      }
    },
    [meetingId, onError, closeMenus],
  );

  const clearAnnotation = useCallback(
    async (seg: LiveEvent) => {
      closeMenus();
      // Remove every annotation anchored on this line, so an older mark
      // cannot resurface at reconciliation time. Open-ended ranges inherited
      // from earlier lines are left alone (clear them on their own line).
      const covering = annotationsAnchoredOnLine(annotations, seg);
      const results = await Promise.allSettled(
        covering.map((a) => api.deleteLiveAnnotation(a.id)),
      );
      const firstFailure = results.find(
        (r): r is PromiseRejectedResult => r.status === "rejected",
      );
      if (!firstFailure) {
        // Every row is gone from the store — mirror that locally.
        setAnnotations((prev) =>
          prev.filter((a) => !covering.some((c) => c.id === a.id)),
        );
        return;
      }
      // Partial failure: some rows may already be deleted while others
      // survived. Never guess — re-list so the chips converge on the store's
      // real state, and surface the failure.
      onError(String(firstFailure.reason));
      try {
        setAnnotations(await api.listLiveAnnotations(meetingId));
      } catch (e) {
        // Keep the (stale) local state; the next mount re-lists anyway.
        onError(String(e));
      }
    },
    [annotations, meetingId, onError, closeMenus],
  );

  useEffect(() => {
    let un: (() => void) | undefined;
    let disposed = false;
    void listen<LiveEvent>("meeting-live-transcript", (e) => {
      const p = e.payload;
      if (p.meetingId !== meetingId) return; // stale worker / other recording
      setActive(true);
      setSegments((prev) => {
        const current = prev.get(p.segmentId);
        if (current && current.revision >= p.revision) return prev; // out of order
        const next = new Map(prev);
        if (p.isFinal && !p.text.trim()) {
          next.delete(p.segmentId); // retracted segment
        } else {
          next.set(p.segmentId, p);
        }
        return next;
      });
    }).then((fn) => {
      if (disposed) fn();
      else un = fn;
    });
    return () => {
      disposed = true;
      un?.();
    };
  }, [meetingId]);

  // Unified-timeline order across both tracks (id as a stable tiebreak).
  const ordered = useMemo(
    () =>
      [...segments.values()].sort(
        (a, b) =>
          a.startSeconds - b.startSeconds ||
          a.segmentId.localeCompare(b.segmentId),
      ),
    [segments],
  );

  // Keep the newest text in view — but never yank the viewport (and the
  // fixed-positioned menu with it) while the user has a menu open.
  useEffect(() => {
    if (menuFor !== null) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [ordered, menuFor]);

  return (
    <div className="meeting-live" aria-live="polite">
      <div className="meeting-live-head">
        <Icon name="mic" size={12} />
        <span>实时预览</span>
        <span className="muted-text meeting-live-note">
          点行尾「标注」可记录谁在说 · 停止后生成带说话人最终稿
        </span>
      </div>
      <div
        className="meeting-live-body"
        ref={scrollRef}
        // The menu is fixed-positioned from the chip's rect at open time; any
        // scroll would leave it floating detached, so close it instead.
        onScroll={closeMenus}
      >
        {active ? (
          <div className="meeting-live-text">
            {ordered.map((seg) => {
              const named = seg.isFinal
                ? annotationForLine(annotations, seg)
                : null;
              // Chip display priority: manual annotation (L2, including an
              // inherited open-ended range) > voiceprint attribution (L3,
              // "我" when it is the self identity, "?" when provisional) >
              // the bare 标注 affordance.
              const voiceprint = named ? null : (seg.speaker ?? null);
              const voiceprintName = voiceprint
                ? (voiceprint.identityId === selfIdentityId
                    ? "我"
                    : voiceprint.displayName) +
                  (voiceprint.provisional ? "?" : "")
                : null;
              const anchored = seg.isFinal
                ? annotationsAnchoredOnLine(annotations, seg)
                : [];
              return (
                <p
                  key={seg.segmentId}
                  className={
                    seg.isFinal ? "meeting-live-final" : "meeting-live-partial"
                  }
                >
                  <span
                    className={`meeting-live-track meeting-live-track-${seg.track}`}
                  >
                    {LIVE_TRACK_LABEL[seg.track]}
                  </span>
                  {seg.text}
                  {seg.isFinal && (
                    <span className="meeting-live-annotate">
                      <button
                        type="button"
                        className={`meeting-live-chip${
                          named
                            ? " named"
                            : voiceprint
                              ? ` voiceprint${voiceprint.provisional ? " provisional" : ""}`
                              : ""
                        }`}
                        title="标注这句话是谁在说"
                        onClick={(e) => {
                          if (menuFor === seg.segmentId) closeMenus();
                          else openMenu(seg, e.currentTarget);
                        }}
                      >
                        {named ? named.display_name : (voiceprintName ?? "标注")}
                      </button>
                      {menuFor === seg.segmentId && menuPos && (
                        <span
                          className="meeting-live-annotate-menu"
                          role="menu"
                          style={{
                            left: menuPos.left,
                            top: menuPos.top,
                            bottom: menuPos.bottom,
                          }}
                        >
                          <span
                            className="meeting-live-annotate-scope"
                            role="radiogroup"
                            aria-label="标注范围"
                          >
                            <button
                              type="button"
                              role="radio"
                              aria-checked={scope === "onward"}
                              className={scope === "onward" ? "active" : ""}
                              onClick={() => setScope("onward")}
                            >
                              此句及之后
                            </button>
                            <button
                              type="button"
                              role="radio"
                              aria-checked={scope === "line"}
                              className={scope === "line" ? "active" : ""}
                              onClick={() => setScope("line")}
                            >
                              仅此句
                            </button>
                          </span>
                          {identities.map((p) => (
                            <button
                              key={p.id}
                              type="button"
                              onClick={() =>
                                void annotate(seg, p.name, scope, p.id)
                              }
                            >
                              {p.name}
                            </button>
                          ))}
                          {customFor === seg.segmentId ? (
                            <input
                              className="meeting-live-annotate-input"
                              autoFocus
                              value={customName}
                              placeholder="输入名字后回车"
                              onChange={(e) => setCustomName(e.target.value)}
                              onKeyDown={(e) => {
                                if (e.key === "Enter" && customName.trim()) {
                                  void annotate(seg, customName.trim(), scope);
                                } else if (e.key === "Escape") {
                                  setCustomFor(null);
                                  setCustomName("");
                                }
                              }}
                            />
                          ) : (
                            <button
                              type="button"
                              onClick={() => setCustomFor(seg.segmentId)}
                            >
                              自定义名字…
                            </button>
                          )}
                          {anchored.length > 0 && (
                            <button
                              type="button"
                              onClick={() => void clearAnnotation(seg)}
                            >
                              清除
                            </button>
                          )}
                        </span>
                      )}
                    </span>
                  )}
                </p>
              );
            })}
            {ordered.length === 0 && (
              <p className="muted-text meeting-live-listening">正在聆听…</p>
            )}
          </div>
        ) : models.status?.paraformerStreamingReady ? (
          <p className="muted-text meeting-live-empty">正在聆听…</p>
        ) : (
          <div className="meeting-live-empty">
            <p className="muted-text">
              安装 Paraformer streaming 模型可在录制中实时预览逐字稿（停止后仍会生成带说话人的最终稿）。
            </p>
            {models.active === "streaming" && models.progress ? (
              <ModelProgressBar progress={models.progress} />
            ) : (
              <button
                type="button"
                className="btn small meeting-live-install"
                disabled={models.active !== null}
                onClick={() => void models.download("streaming")}
              >
                下载模型
              </button>
            )}
            {models.error && (
              <p className="meeting-model-error">{models.error}</p>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

// ---- two-pane recording workspace (Granola-style) ----------------------
// While a meeting is `recording`, the detail page becomes a two-pane
// workspace. Left (the star, wider): the live transcript — reusing
// `RecordingBar`, which already stacks the ● 录制 strip + stop button on top of
// the auto-scrolling `LiveTranscript`. Right: a free-form notes editor whose
// text autosaves and is fused into the minutes LLM pass after stop, so what the
// user jots here shapes the generated 纪要.
function RecordingWorkspace({
  meeting,
  onStopped,
  onError,
  models,
}: {
  meeting: Meeting;
  onStopped: () => void;
  onError: (e: string | null) => void;
  models: MeetingModels;
}) {
  return (
    <div className="meeting-rec">
      <div className="meeting-rec-left">
        <RecordingBar
          meetingId={meeting.id}
          startedAtMs={Date.parse(meeting.created_at)}
          onStopped={onStopped}
          onError={onError}
          models={models}
        />
      </div>
      <div className="meeting-rec-right">
        <MeetingNotesEditor
          meetingId={meeting.id}
          initialNotes={meeting.notes}
          onError={onError}
        />
      </div>
    </div>
  );
}

// Free-form notes editor with debounced autosave. The latest typed value is
// mirrored into a ref so `blur` and unmount (e.g. when the recording stops and
// this view is torn down) can flush the final value even if the debounce timer
// has not yet fired — no lost characters. Saves are last-write-wins on the
// backend; a failed save rolls the "persisted" marker back so a later edit
// retries.
const NOTES_DEBOUNCE_MS = 800;

function MeetingNotesEditor({
  meetingId,
  initialNotes,
  onError,
}: {
  meetingId: string;
  initialNotes: string;
  onError: (e: string | null) => void;
}) {
  const [value, setValue] = useState(initialNotes);
  const [saved, setSaved] = useState(false);
  // Latest typed value (for flush) and the value last persisted (to skip
  // no-op saves and to detect what still needs writing).
  const latestRef = useRef(initialNotes);
  const savedValueRef = useRef(initialNotes);
  const timerRef = useRef<number | null>(null);
  const savedFadeRef = useRef<number | null>(null);

  const flush = useCallback(async () => {
    if (timerRef.current != null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    const next = latestRef.current;
    if (next === savedValueRef.current) return;
    const prev = savedValueRef.current;
    savedValueRef.current = next; // optimistic — blocks a duplicate concurrent save
    try {
      await api.saveMeetingNotes(meetingId, next);
      setSaved(true);
      if (savedFadeRef.current != null) window.clearTimeout(savedFadeRef.current);
      savedFadeRef.current = window.setTimeout(() => setSaved(false), 1500);
    } catch (e) {
      savedValueRef.current = prev; // let a later flush retry the failed write
      onError(String(e));
    }
  }, [meetingId, onError]);

  const onChange = useCallback(
    (e: ChangeEvent<HTMLTextAreaElement>) => {
      const next = e.target.value;
      setValue(next);
      latestRef.current = next;
      setSaved(false);
      if (timerRef.current != null) window.clearTimeout(timerRef.current);
      timerRef.current = window.setTimeout(() => void flush(), NOTES_DEBOUNCE_MS);
    },
    [flush],
  );

  // Flush the final value on unmount (covers the recording→processing teardown
  // when the user hits stop). `flush` is stable, so this runs only on unmount.
  useEffect(() => {
    return () => {
      if (savedFadeRef.current != null) window.clearTimeout(savedFadeRef.current);
      void flush();
    };
  }, [flush]);

  return (
    <div className="card meeting-rec-notes">
      <div className="meeting-rec-notes-head">
        <Icon name="clipboard" size={13} />
        <span>我的笔记</span>
        {saved && <span className="meeting-rec-saved">已保存</span>}
      </div>
      <textarea
        className="meeting-rec-notes-input"
        value={value}
        onChange={onChange}
        onBlur={() => void flush()}
        placeholder="随手记要点…（停止后 AI 会结合逐字稿整理成纪要）"
      />
    </div>
  );
}

function StatusBadge({
  status,
  title,
}: {
  status: MeetingStatus;
  title?: string;
}) {
  const meta = STATUS_META[status];
  return (
    <span className={`meeting-badge ${meta.tone}`} title={title}>
      {isInProgress(status) && <span className="mtg-spinner" aria-hidden />}
      {meta.label}
    </span>
  );
}

function MeetingRow({
  meeting,
  onOpen,
  onRename,
  onRequestDelete,
}: {
  meeting: Meeting;
  onOpen: () => void;
  /** Persist a new title. Resolves when the backend write + refresh finish. */
  onRename: (title: string) => Promise<void>;
  /** Ask the library to open the delete-confirmation dialog for this meeting. */
  onRequestDelete: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(meeting.title ?? "");
  const [saving, setSaving] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  // A recording meeting is still being captured; renaming/deleting it mid-flight
  // is confusing, so hide the row actions until it leaves `recording`.
  const locked = meeting.status === "recording";

  const beginEdit = useCallback(() => {
    setDraft(meeting.title ?? "");
    setEditing(true);
  }, [meeting.title]);

  useEffect(() => {
    if (editing) inputRef.current?.select();
  }, [editing]);

  const commit = useCallback(async () => {
    const next = draft.trim();
    // No change → just close the editor (empty ↔ untitled counts as no change).
    if (next === (meeting.title ?? "").trim()) {
      setEditing(false);
      return;
    }
    setSaving(true);
    try {
      await onRename(next);
      setEditing(false);
    } finally {
      setSaving(false);
    }
  }, [draft, meeting.title, onRename]);

  if (editing) {
    return (
      <div className="meeting-row meeting-row-editing">
        <input
          ref={inputRef}
          className="meeting-row-rename-input"
          type="text"
          value={draft}
          placeholder="会议标题"
          disabled={saving}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void commit();
            else if (e.key === "Escape") setEditing(false);
          }}
        />
        <div className="meeting-row-actions">
          <button
            type="button"
            className="btn small"
            disabled={saving}
            onClick={() => void commit()}
          >
            {saving ? "保存中…" : "保存"}
          </button>
          <button
            type="button"
            className="btn ghost small"
            disabled={saving}
            onClick={() => setEditing(false)}
          >
            取消
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="meeting-row">
      <button type="button" className="meeting-row-open" onClick={onOpen}>
        <div className="meeting-row-main">
          <span className="meeting-row-title">{meetingTitle(meeting)}</span>
          <span className="meeting-row-meta">
            {formatTimeOfDay(meeting.created_at)}
            <span className="dotsep">·</span>
            {formatDuration(meeting.duration_seconds)}
            {meeting.language ? (
              <>
                <span className="dotsep">·</span>
                {meeting.language}
              </>
            ) : null}
          </span>
        </div>
        <StatusBadge
          status={meeting.status}
          title={
            meeting.status === "failed"
              ? (meeting.failure_reason ?? undefined)
              : undefined
          }
        />
      </button>
      {!locked && (
        <div className="meeting-row-actions">
          <button
            type="button"
            className="icon-btn"
            title="重命名"
            aria-label="重命名会议"
            onClick={beginEdit}
          >
            <Icon name="pencil" size={15} />
          </button>
          <button
            type="button"
            className="icon-btn danger"
            title="删除"
            aria-label="删除会议"
            onClick={onRequestDelete}
          >
            <Icon name="delete" size={15} />
          </button>
        </div>
      )}
    </div>
  );
}

/** A small centered confirmation dialog for destructive actions. Reuses the
 * meeting modal chrome; Esc / overlay-click cancels, so an accidental open is
 * cheap to back out of. The confirm button is `danger` and auto-focused. */
function ConfirmDialog({
  title,
  message,
  confirmLabel,
  busy,
  onConfirm,
  onCancel,
}: {
  title: string;
  message: string;
  confirmLabel: string;
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const confirmRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    confirmRef.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onCancel();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onCancel]);

  return (
    <div
      className="meeting-modal-overlay"
      role="presentation"
      onClick={() => {
        if (!busy) onCancel();
      }}
    >
      <div
        className="card meeting-confirm"
        role="alertdialog"
        aria-modal="true"
        aria-label={title}
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="meeting-confirm-title">{title}</h3>
        <p className="meeting-confirm-message muted-text">{message}</p>
        <div className="meeting-confirm-actions">
          <button
            type="button"
            className="btn ghost small"
            disabled={busy}
            onClick={onCancel}
          >
            取消
          </button>
          <button
            ref={confirmRef}
            type="button"
            className="btn small danger"
            disabled={busy}
            onClick={onConfirm}
          >
            {busy ? "处理中…" : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

// ---- detail (transcript-first; minutes as a left index) -----------------
// dogfood revision (docs/MEETING_M4_UX.md, 2026-07-29): the transcript is the
// default, always-visible main area; the structured minutes collapse into a
// narrow left index whose items scroll+flash the matching transcript turn. No
// more 纪要/逐字稿 tab toggle. A "查看完整纪要" entry still opens the full minutes.

function MeetingDetailView({
  meetingId,
  onBack,
  onError,
  onNavigate,
  models,
}: {
  meetingId: string;
  onBack: () => void;
  onError: (e: string | null) => void;
  onNavigate?: (tab: TabId) => void;
  models: MeetingModels;
}) {
  const [detail, setDetail] = useState<MeetingDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [exporting, setExporting] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  // Whether the full-minutes reading overlay is open.
  const [fullOpen, setFullOpen] = useState(false);
  // A jump request from a minutes index item → transcript. The token forces the
  // transcript to (re)scroll even when the target seconds repeat.
  const [jump, setJump] = useState<{ seconds: number; token: number } | null>(
    null,
  );
  // Inline title editing on the detail header.
  const [editingTitle, setEditingTitle] = useState(false);
  const [titleDraft, setTitleDraft] = useState("");
  const [savingTitle, setSavingTitle] = useState(false);
  const titleInputRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    try {
      const d = await api.getMeetingDetail(meetingId);
      setDetail(d);
    } catch (e) {
      onError(String(e));
    } finally {
      setLoading(false);
    }
  }, [meetingId, onError]);

  useEffect(() => {
    setLoading(true);
    void load();
  }, [load]);

  const status = detail?.meeting.status;
  useEffect(() => {
    if (!status || !isInProgress(status)) return;
    const id = window.setInterval(() => void load(), 6000);
    return () => window.clearInterval(id);
  }, [status, load]);

  const minutes = useMemo(
    () => (detail ? parseMinutes(detail) : null),
    [detail],
  );

  const jumpToSource = useCallback((src: SourceRef) => {
    // Transcript is already visible; just (re)scroll it to the source turn.
    setJump({ seconds: src.start, token: Date.now() });
  }, []);

  // ---- audio playback (review mode) --------------------------------------
  // A single <audio> element (rendered by the bottom player bar) is shared with
  // the transcript so clicking a sentence can seek it and playback can highlight
  // the current sentence. `currentTime` is the playhead in seconds.
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const [currentTime, setCurrentTime] = useState(0);

  // Only expose a player when the meeting is done and has a recorded WAV. The
  // asset: URL is produced by Tauri's asset protocol (scoped to the meetings
  // dir in tauri.conf.json) so <audio> can stream + seek the file directly
  // without shuttling megabytes of PCM over IPC.
  const audioSrc = useMemo(() => {
    const path = detail?.meeting.audio_path;
    if (!path || detail?.meeting.status !== "ready") return null;
    return convertFileSrc(path);
  }, [detail?.meeting.audio_path, detail?.meeting.status]);

  // Reset the playhead when the audio source changes (switching meetings reuses
  // this component instance, so state would otherwise carry over).
  useEffect(() => {
    setCurrentTime(0);
  }, [audioSrc]);

  const seekTo = useCallback((seconds: number) => {
    const el = audioRef.current;
    if (!el) return;
    el.currentTime = seconds;
    void el.play().catch(() => {});
  }, []);

  // Patch one segment's text in place after an inline edit — cheaper (and less
  // jarring) than a full reload, which would reset scroll and the playhead.
  const applySegmentText = useCallback((segmentId: string, text: string) => {
    setDetail((prev) =>
      prev
        ? {
            ...prev,
            segments: prev.segments.map((s) =>
              s.id === segmentId ? { ...s, text } : s,
            ),
          }
        : prev,
    );
  }, []);

  // Patch one speaker's display name in place after a rename. Because the
  // transcript turns and the participant list both read the speaker off
  // `detail.speakers`, this one update re-labels the speaker everywhere at once
  // (participants → "已确认", every attributed turn shows the real name) without
  // a reload that would reset scroll and the playhead. A blank name clears back
  // to `null` so the speaker reverts to its engine label / "未确认" — mirroring
  // how the store normalizes it.
  const applySpeakerName = useCallback(
    (speakerId: string, displayName: string) => {
      const trimmed = displayName.trim();
      setDetail((prev) =>
        prev
          ? {
              ...prev,
              speakers: prev.speakers.map((s) =>
                s.id === speakerId
                  ? { ...s, display_name: trimmed.length > 0 ? trimmed : null }
                  : s,
              ),
            }
          : prev,
      );
    },
    [],
  );

  const beginTitleEdit = useCallback(() => {
    setTitleDraft(detail?.meeting.title ?? "");
    setEditingTitle(true);
  }, [detail?.meeting.title]);

  useEffect(() => {
    if (editingTitle) titleInputRef.current?.select();
  }, [editingTitle]);

  const commitTitle = useCallback(async () => {
    const next = titleDraft.trim();
    const current = (detail?.meeting.title ?? "").trim();
    if (next === current) {
      setEditingTitle(false);
      return;
    }
    setSavingTitle(true);
    onError(null);
    try {
      await api.renameMeeting(meetingId, next);
      setEditingTitle(false);
      await load();
    } catch (e) {
      onError(String(e));
    } finally {
      setSavingTitle(false);
    }
  }, [titleDraft, detail?.meeting.title, meetingId, load, onError]);

  async function doExport(preset: ExportPreset) {
    setExportOpen(false);
    setExporting(true);
    onError(null);
    try {
      const out = await api.exportMeeting(meetingId, preset);
      const blob = new Blob([out.content], {
        type: "text/plain;charset=utf-8",
      });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = out.filename;
      document.body.appendChild(a);
      a.click();
      a.remove();
      // Revoke only after the download has had a chance to start; revoking
      // synchronously can cut the download off before the webview reads it.
      window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
    } catch (e) {
      onError(String(e));
    } finally {
      setExporting(false);
    }
  }

  const meeting = detail?.meeting;
  const speakerCount = detail?.speakers.length ?? 0;

  return (
    <div className="meeting-detail">
      <header className="meeting-detail-head">
        <button
          type="button"
          className="btn ghost small meeting-back"
          onClick={onBack}
        >
          ← 会议库
        </button>
        <div className="meeting-detail-titles">
          {editingTitle && meeting ? (
            <div className="meeting-detail-title-edit">
              <input
                ref={titleInputRef}
                className="meeting-detail-title-input"
                type="text"
                value={titleDraft}
                placeholder="会议标题"
                disabled={savingTitle}
                onChange={(e) => setTitleDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void commitTitle();
                  else if (e.key === "Escape") setEditingTitle(false);
                }}
              />
              <button
                type="button"
                className="btn small"
                disabled={savingTitle}
                onClick={() => void commitTitle()}
              >
                {savingTitle ? "保存中…" : "保存"}
              </button>
              <button
                type="button"
                className="btn ghost small"
                disabled={savingTitle}
                onClick={() => setEditingTitle(false)}
              >
                取消
              </button>
            </div>
          ) : (
            <h2 className="meeting-detail-title">
              {meeting ? meetingTitle(meeting) : "加载中…"}
              {meeting && meeting.status !== "recording" && (
                <button
                  type="button"
                  className="icon-btn meeting-detail-title-edit-btn"
                  title="重命名"
                  aria-label="重命名会议"
                  onClick={beginTitleEdit}
                >
                  <Icon name="pencil" size={15} />
                </button>
              )}
            </h2>
          )}
          {meeting && (
            <div className="meeting-detail-sub muted-text">
              {formatFullDate(meeting.created_at)}
              <span className="dotsep">·</span>
              {formatDuration(meeting.duration_seconds)}
              <span className="dotsep">·</span>
              {speakerCount} 位说话人
              <StatusBadge status={meeting.status} />
            </div>
          )}
        </div>
        <div className="meeting-detail-actions">
          <div className="meeting-export">
            <button
              type="button"
              className="btn ghost small"
              disabled={exporting || !detail}
              onClick={() => setExportOpen((v) => !v)}
            >
              导出
            </button>
            {exportOpen && (
              <div className="meeting-export-menu" role="menu">
                <button type="button" onClick={() => void doExport("minutes_md")}>
                  会议纪要.md
                </button>
                <button
                  type="button"
                  onClick={() => void doExport("transcript_md")}
                >
                  完整逐字稿.md
                </button>
                <button
                  type="button"
                  onClick={() => void doExport("subtitles_srt")}
                >
                  字幕.srt
                </button>
                <button type="button" onClick={() => void doExport("data_json")}>
                  会议数据.json
                </button>
              </div>
            )}
          </div>
        </div>
      </header>

      {loading && !detail ? (
        <div className="card meeting-empty">
          <p className="muted-text">正在读取会议…</p>
        </div>
      ) : !detail ? (
        <div className="card meeting-empty">
          <p className="muted-text">无法读取这场会议。</p>
        </div>
      ) : detail.meeting.status === "recording" ? (
        // Granola-style two-pane recording view: live transcript (star, left)
        // + free-form notes editor (right). Replaces the old recording banner;
        // once stopped the status leaves `recording` and the review body below
        // renders instead.
        <RecordingWorkspace
          meeting={detail.meeting}
          onStopped={() => void load()}
          onError={onError}
          models={models}
        />
      ) : (
        <>
          <div className="meeting-detail-body">
            <aside className="card meeting-index">
              <MinutesIndex
                detail={detail}
                minutes={minutes}
                onJump={jumpToSource}
                onNavigate={onNavigate}
                onOpenFull={() => setFullOpen(true)}
                onSpeakerRenamed={applySpeakerName}
                onError={onError}
              />
            </aside>
            <TranscriptView
              detail={detail}
              jump={jump}
              currentTime={currentTime}
              playable={audioSrc != null}
              onSeek={seekTo}
              onSegmentEdited={applySegmentText}
              onError={onError}
            />
          </div>
          {audioSrc && (
            <MeetingAudioBar
              src={audioSrc}
              audioRef={audioRef}
              currentTime={currentTime}
              onTime={setCurrentTime}
              durationHint={detail.meeting.duration_seconds ?? null}
              onError={onError}
            />
          )}
        </>
      )}

      {fullOpen && detail && minutes && (
        <MinutesFullModal
          minutes={minutes}
          onJump={(src) => {
            setFullOpen(false);
            jumpToSource(src);
          }}
          onClose={() => setFullOpen(false)}
        />
      )}
    </div>
  );
}

// ---- status note (processing / failed) ----------------------------------

/** A failed meeting: shows the concrete failure reason plus, when the reason
 * points at diarization, an actionable hint (install models / macOS-only). */
function FailureNote({
  detail,
  kind,
}: {
  detail: MeetingDetail;
  kind: "summary" | "transcript";
}) {
  const reason = detail.meeting.failure_reason?.trim() || null;
  const guidance = diarGuidance(reason);
  return (
    <div className="meeting-statusnote bad">
      <div>
        <strong>处理失败</strong>
        <p className="muted-text">
          这场会议的{kind === "summary" ? "纪要" : "逐字稿"}没有生成成功。
        </p>
        {reason && <p className="meeting-fail-reason">原因：{reason}</p>}
        {guidance === "install_models" && (
          <p className="meeting-fail-guide">
            说话人分离模型未安装。请将 <code>seg.onnx</code>、
            <code>emb.onnx</code> 和 <code>plda</code> 放入应用模型目录的{" "}
            <code>diar/</code> 子目录（上面的路径即缺失位置），然后在“录音”里重试。
          </p>
        )}
        {guidance === "macos_only" && (
          <p className="meeting-fail-guide">
            离线说话人分离目前仅在 macOS 上支持，此平台无法生成逐字稿。
          </p>
        )}
        {!guidance && (
          <p className="muted-text">可在“录音”里重试，或查看原始音频。</p>
        )}
      </div>
    </div>
  );
}

function StatusNote({
  detail,
  kind,
}: {
  detail: MeetingDetail;
  kind: "summary" | "transcript";
}) {
  const status = detail.meeting.status;
  if (status === "failed") {
    return <FailureNote detail={detail} kind={kind} />;
  }
  // A terminal (ready) meeting that simply has no content must not show a
  // "transcribing…" spinner — the pipeline is done, there is just nothing here.
  if (!isInProgress(status)) {
    return (
      <div className="meeting-statusnote">
        <div>
          <strong>{kind === "summary" ? "暂无纪要" : "暂无逐字稿"}</strong>
          <p className="muted-text">
            这场会议没有可用的{kind === "summary" ? "结构化纪要" : "逐字稿内容"}。
          </p>
        </div>
      </div>
    );
  }
  const label = STATUS_META[status].label;
  return (
    <div className="meeting-statusnote">
      <span className="mtg-spinner big" aria-hidden />
      <div>
        <strong>{label}…</strong>
        <p className="muted-text">
          {kind === "summary"
            ? "转录完成后会自动生成结构化纪要。"
            : "离线转录进行中，完成后逐字稿会出现在这里。"}
        </p>
      </div>
    </div>
  );
}

// ---- summary view (structured minutes) ----------------------------------

function SourceChip({
  source,
  onJump,
}: {
  source?: SourceRef | null;
  onJump: (src: SourceRef) => void;
}) {
  if (!source) return null;
  return (
    <button
      type="button"
      className="meeting-source"
      title="跳到逐字稿对应位置"
      onClick={() => onJump(source)}
    >
      {formatClock(source.start)}
    </button>
  );
}

/** One compact, clickable index row: the minutes text plus a small timestamp.
 * The whole row jumps to the matching transcript turn when it has a source. */
function IndexItem({
  text,
  sub,
  source,
  onJump,
}: {
  text: string;
  sub?: string | null;
  source?: SourceRef | null;
  onJump: (src: SourceRef) => void;
}) {
  const body = (
    <>
      <span className="meeting-index-item-text">{text}</span>
      {sub && <span className="meeting-index-item-sub muted-text">{sub}</span>}
    </>
  );
  if (!source) {
    return <li className="meeting-index-item static">{body}</li>;
  }
  return (
    <li>
      <button
        type="button"
        className="meeting-index-item"
        title="跳到逐字稿对应位置"
        onClick={() => onJump(source)}
      >
        {body}
        <span className="meeting-index-item-time">
          {formatClock(source.start)}
        </span>
      </button>
    </li>
  );
}

function actionSub(item: ActionItem): string | null {
  const parts: string[] = [];
  if (item.owner?.trim()) parts.push(`负责人：${item.owner.trim()}`);
  if (item.due?.trim()) parts.push(`截止：${item.due.trim()}`);
  return parts.length > 0 ? parts.join(" · ") : null;
}

/** The narrow left column: structured minutes rendered as a compact, clickable
 * index into the transcript, plus participant/meeting info folded in at the
 * bottom. Restrained styling (muted, small, grouped) so it stays a secondary
 * index and the transcript keeps the stage. */
function MinutesIndex({
  detail,
  minutes,
  onJump,
  onNavigate,
  onOpenFull,
  onSpeakerRenamed,
  onError,
}: {
  detail: MeetingDetail;
  minutes: Minutes | null;
  onJump: (src: SourceRef) => void;
  onNavigate?: (tab: TabId) => void;
  onOpenFull: () => void;
  onSpeakerRenamed: (speakerId: string, displayName: string) => void;
  onError: (e: string | null) => void;
}) {
  const hasMinutes = minutes != null && !minutesEmpty(minutes);
  const status = detail.meeting.status;

  let guide: ReactNode = null;
  if (!hasMinutes) {
    if (status === "failed") {
      guide = (
        <p className="muted-text meeting-index-note">
          纪要未生成（处理失败）。详情见右侧。
        </p>
      );
    } else if (isInProgress(status)) {
      guide = (
        <p className="muted-text meeting-index-note">
          <span className="mtg-spinner" aria-hidden />{" "}
          {status === "summarizing"
            ? "正在生成纪要…"
            : "转录完成后会自动生成纪要索引。"}
        </p>
      );
    } else if (noLlmMinutes(detail)) {
      guide = (
        <div className="meeting-index-note">
          <strong>未配置 LLM</strong>
          <p className="muted-text">
            逐字稿已生成，但尚未配置语言模型，无法自动生成会议纪要
            （摘要 / 决策 / 行动项）。配置后即可在此看到纪要索引。
          </p>
          {onNavigate && (
            <button
              type="button"
              className="btn small meeting-config-llm"
              onClick={() => onNavigate("settings")}
            >
              去设置配置 LLM
            </button>
          )}
        </div>
      );
    } else {
      guide = (
        <p className="muted-text meeting-index-note">
          这场会议没有可用的结构化纪要。
        </p>
      );
    }
  }

  return (
    <div className="meeting-index-inner">
      <div className="meeting-index-head">
        <h3 className="meeting-index-heading">纪要索引</h3>
        {hasMinutes && (
          <button
            type="button"
            className="btn ghost small meeting-index-more"
            onClick={onOpenFull}
          >
            查看完整纪要
          </button>
        )}
      </div>

      {guide}

      {hasMinutes && (
        <>
          {minutes!.one_liner.trim() && (
            <p className="meeting-index-oneliner">{minutes!.one_liner}</p>
          )}

          {minutes!.decisions.length > 0 && (
            <section className="meeting-index-sec">
              <h4 className="meeting-index-title">决策</h4>
              <ul className="meeting-index-list">
                {minutes!.decisions.map((d, i) => (
                  <IndexItem
                    key={i}
                    text={d.text}
                    source={d.source}
                    onJump={onJump}
                  />
                ))}
              </ul>
            </section>
          )}

          {minutes!.action_items.length > 0 && (
            <section className="meeting-index-sec">
              <h4 className="meeting-index-title">行动项</h4>
              <ul className="meeting-index-list">
                {minutes!.action_items.map((a, i) => (
                  <IndexItem
                    key={i}
                    text={a.text}
                    sub={actionSub(a)}
                    source={a.source}
                    onJump={onJump}
                  />
                ))}
              </ul>
            </section>
          )}

          {minutes!.discussion.length > 0 && (
            <section className="meeting-index-sec">
              <h4 className="meeting-index-title">关键讨论</h4>
              <ul className="meeting-index-list">
                {minutes!.discussion.map((d, i) => (
                  <IndexItem
                    key={i}
                    text={d.topic}
                    source={d.source}
                    onJump={onJump}
                  />
                ))}
              </ul>
            </section>
          )}

          {minutes!.open_questions.length > 0 && (
            <section className="meeting-index-sec">
              <h4 className="meeting-index-title">未决问题</h4>
              <ul className="meeting-index-list">
                {minutes!.open_questions.map((q, i) => (
                  <IndexItem
                    key={i}
                    text={q.text}
                    source={q.source}
                    onJump={onJump}
                  />
                ))}
              </ul>
            </section>
          )}
        </>
      )}

      <MeetingSideInfo
        detail={detail}
        onRenamed={onSpeakerRenamed}
        onError={onError}
      />
    </div>
  );
}

/** Participants + meeting facts, folded into the left column bottom (kept
 * compact so it never competes with the minutes index above it).
 *
 * Each diarized speaker can be given a real name inline: click the name to edit,
 * Enter/保存 to commit (calls `rename_speaker`), Escape to cancel. A speaker with
 * a name reads as "已确认"; a blank name reverts it to the engine label + "未确认".
 * The commit patches the parent detail in place (`onRenamed`) so the transcript
 * and this list re-label the speaker at once, without a reload.
 *
 * Voiceprint enrollment (M5): a confirmed speaker whose cluster has a stored
 * embedding gets a "注册声纹" action — enrolling stores the voiceprint in the
 * local identity library so later meetings auto-assign the name. Speakers whose
 * name is already enrolled show a "已注册声纹" badge instead (auto-identified
 * speakers therefore arrive pre-badged). A small collapsible 声纹库 list allows
 * removing enrolled identities. */
function MeetingSideInfo({
  detail,
  onRenamed,
  onError,
}: {
  detail: MeetingDetail;
  onRenamed: (speakerId: string, displayName: string) => void;
  onError: (e: string | null) => void;
}) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [saving, setSaving] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  /** speakerId → has a stored voiceprint embedding (enrollable). */
  const [voiceprints, setVoiceprints] = useState<Record<string, boolean>>({});
  const [enrolled, setEnrolled] = useState<EnrolledSpeaker[]>([]);
  const [enrollingId, setEnrollingId] = useState<string | null>(null);
  const [libraryOpen, setLibraryOpen] = useState(false);

  const meetingId = detail.meeting.id;

  const [selfIdentityId, setSelfIdentityId] = useState<string | null>(null);

  const refreshEnrollment = useCallback(async () => {
    // Supplementary info: each read degrades independently (allSettled) so one
    // failure only hides its own affordance — the participants list and the
    // other voiceprint data still render; the first failure is surfaced.
    const [prints, identities, selfId] = await Promise.allSettled([
      api.getMeetingVoiceprints(meetingId),
      api.listEnrolledSpeakers(),
      api.getSelfIdentity(),
    ]);
    if (prints.status === "fulfilled") {
      setVoiceprints(
        Object.fromEntries(
          prints.value.map((p) => [p.speakerId, p.hasEmbedding]),
        ),
      );
    }
    if (identities.status === "fulfilled") setEnrolled(identities.value);
    if (selfId.status === "fulfilled") setSelfIdentityId(selfId.value);
    const failed = [prints, identities, selfId].find(
      (r): r is PromiseRejectedResult => r.status === "rejected",
    );
    if (failed) onError(String(failed.reason));
  }, [meetingId, onError]);

  /** Mark an enrolled identity as the user themself (or clear the mark).
   * Live captions and future meetings then render this person as "我". */
  const toggleSelf = useCallback(
    async (identityId: string) => {
      onError(null);
      try {
        const next = selfIdentityId === identityId ? null : identityId;
        setSelfIdentityId(await api.setSelfIdentity(next));
      } catch (e) {
        onError(String(e));
      }
    },
    [selfIdentityId, onError],
  );

  useEffect(() => {
    void refreshEnrollment();
  }, [refreshEnrollment]);

  const enrolledNames = useMemo(
    () => new Set(enrolled.map((i) => i.name)),
    [enrolled],
  );

  const enroll = useCallback(
    async (speaker: Speaker) => {
      setEnrollingId(speaker.id);
      onError(null);
      try {
        await api.enrollSpeaker(meetingId, speaker.id);
        await refreshEnrollment();
      } catch (e) {
        onError(String(e));
      } finally {
        setEnrollingId(null);
      }
    },
    [meetingId, refreshEnrollment, onError],
  );

  const removeEnrolled = useCallback(
    async (identityId: string) => {
      onError(null);
      try {
        await api.removeEnrolledSpeaker(identityId);
        await refreshEnrollment();
      } catch (e) {
        onError(String(e));
      }
    },
    [refreshEnrollment, onError],
  );

  useEffect(() => {
    if (editingId) inputRef.current?.select();
  }, [editingId]);

  const beginEdit = useCallback((speaker: Speaker) => {
    setDraft(speaker.display_name?.trim() ?? "");
    setEditingId(speaker.id);
  }, []);

  const commit = useCallback(
    async (speakerId: string) => {
      const next = draft.trim();
      const current =
        detail.speakers.find((s) => s.id === speakerId)?.display_name?.trim() ??
        "";
      // No change (including opening then closing an already-blank name) → just
      // leave edit mode without a needless write.
      if (next === current) {
        setEditingId(null);
        return;
      }
      setSaving(true);
      onError(null);
      try {
        await api.renameSpeaker(speakerId, next);
        onRenamed(speakerId, next);
        setEditingId(null);
      } catch (e) {
        onError(String(e));
      } finally {
        setSaving(false);
      }
    },
    [draft, detail.speakers, onRenamed, onError],
  );

  return (
    <div className="meeting-index-info">
      <section className="meeting-side-sec">
        <h4 className="meeting-index-title">参与者</h4>
        {detail.speakers.length === 0 ? (
          <p className="muted-text meeting-index-note">尚未识别到说话人。</p>
        ) : (
          <ul className="meeting-participants">
            {detail.speakers.map((s) => {
              const { name, confirmed } = speakerDisplay(s);
              const editing = editingId === s.id;
              return (
                <li key={s.id} className="meeting-participant">
                  <span className="meeting-avatar sm" aria-hidden>
                    {name.slice(0, 1)}
                  </span>
                  {editing ? (
                    <>
                      <input
                        ref={inputRef}
                        className="meeting-participant-input"
                        type="text"
                        value={draft}
                        placeholder={s.label}
                        disabled={saving}
                        onChange={(e) => setDraft(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") void commit(s.id);
                          else if (e.key === "Escape") setEditingId(null);
                        }}
                      />
                      <button
                        type="button"
                        className="btn small"
                        disabled={saving}
                        onClick={() => void commit(s.id)}
                      >
                        {saving ? "…" : "保存"}
                      </button>
                    </>
                  ) : (
                    <>
                      <button
                        type="button"
                        className="meeting-participant-name meeting-participant-edit"
                        title="点击设置真实姓名"
                        onClick={() => beginEdit(s)}
                      >
                        {name}
                      </button>
                      {confirmed ? (
                        <span className="meeting-confirmed">已确认</span>
                      ) : (
                        <span className="meeting-unconfirmed">未确认</span>
                      )}
                      {confirmed && enrolledNames.has(name) ? (
                        <span
                          className="meeting-voiceprint-badge"
                          title="此人已注册声纹，新会议会自动识别"
                        >
                          已注册声纹
                        </span>
                      ) : confirmed && voiceprints[s.id] ? (
                        <button
                          type="button"
                          className="btn small meeting-enroll-btn"
                          title="注册此人的声纹，之后的会议将自动识别（仅保存在本机）"
                          disabled={enrollingId === s.id}
                          onClick={() => void enroll(s)}
                        >
                          {enrollingId === s.id ? "…" : "注册声纹"}
                        </button>
                      ) : null}
                    </>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </section>
      {enrolled.length > 0 && (
        <section className="meeting-side-sec">
          <button
            type="button"
            className="meeting-index-title meeting-voiceprint-toggle"
            aria-expanded={libraryOpen}
            onClick={() => setLibraryOpen((open) => !open)}
          >
            声纹库（{enrolled.length}）{libraryOpen ? "▾" : "▸"}
          </button>
          {libraryOpen && (
            <ul className="meeting-voiceprint-list">
              {enrolled.map((identity) => (
                <li key={identity.id} className="meeting-voiceprint-item">
                  <span className="meeting-voiceprint-name">
                    {identity.name}
                    {selfIdentityId === identity.id && (
                      <span
                        className="meeting-voiceprint-self"
                        title="这是你自己：实时字幕与识别结果将显示为「我」"
                      >
                        我
                      </span>
                    )}
                  </span>
                  <button
                    type="button"
                    className="btn small"
                    title={
                      selfIdentityId === identity.id
                        ? "取消「这是我」标记"
                        : "把这个声纹标记为你自己，识别到时显示「我」"
                    }
                    onClick={() => void toggleSelf(identity.id)}
                  >
                    {selfIdentityId === identity.id ? "取消我" : "这是我"}
                  </button>
                  <button
                    type="button"
                    className="btn small"
                    title="删除此声纹（已识别的会议不受影响，之后的会议不再自动识别）"
                    onClick={() => void removeEnrolled(identity.id)}
                  >
                    删除
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>
      )}
      <section className="meeting-side-sec">
        <h4 className="meeting-index-title">会议信息</h4>
        <dl className="meeting-info">
          <dt>时长</dt>
          <dd>{formatDuration(detail.meeting.duration_seconds)}</dd>
          <dt>日期</dt>
          <dd>{formatFullDate(detail.meeting.created_at)}</dd>
          <dt>来源</dt>
          <dd>本地麦克风</dd>
        </dl>
      </section>
    </div>
  );
}

/** On-demand full minutes reading view (opened from "查看完整纪要"). Source
 * chips still jump into the transcript behind it, closing the overlay first. */
function MinutesFullModal({
  minutes,
  onJump,
  onClose,
}: {
  minutes: Minutes;
  onJump: (src: SourceRef) => void;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      className="meeting-modal-overlay"
      role="presentation"
      onClick={onClose}
    >
      <div
        className="card meeting-modal"
        role="dialog"
        aria-modal="true"
        aria-label="完整纪要"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="meeting-modal-head">
          <h3 className="meeting-modal-title">完整纪要</h3>
          <button
            type="button"
            className="icon-btn meeting-modal-close"
            onClick={onClose}
            aria-label="关闭"
            title="关闭"
          >
            ×
          </button>
        </div>
        <div className="meeting-modal-body">
          {minutes.one_liner.trim() && (
            <section className="meeting-sec">
              <p className="meeting-oneliner">{minutes.one_liner}</p>
            </section>
          )}

          {minutes.decisions.length > 0 && (
            <section className="meeting-sec">
              <h3 className="meeting-sec-title">决策</h3>
              <ul className="meeting-itemlist">
                {minutes.decisions.map((d, i) => (
                  <li key={i} className="meeting-item">
                    <span className="meeting-item-text">{d.text}</span>
                    <SourceChip source={d.source} onJump={onJump} />
                  </li>
                ))}
              </ul>
            </section>
          )}

          {minutes.action_items.length > 0 && (
            <section className="meeting-sec">
              <h3 className="meeting-sec-title">行动项</h3>
              <ul className="meeting-itemlist">
                {minutes.action_items.map((a, i) => (
                  <li key={i} className="meeting-item">
                    <div className="meeting-item-body">
                      <span className="meeting-item-text">{a.text}</span>
                      <ActionMeta item={a} />
                    </div>
                    <SourceChip source={a.source} onJump={onJump} />
                  </li>
                ))}
              </ul>
            </section>
          )}

          {minutes.discussion.length > 0 && (
            <section className="meeting-sec">
              <h3 className="meeting-sec-title">关键讨论</h3>
              <ul className="meeting-itemlist">
                {minutes.discussion.map((d, i) => (
                  <li key={i} className="meeting-item">
                    <span className="meeting-item-text">{d.topic}</span>
                    <SourceChip source={d.source} onJump={onJump} />
                  </li>
                ))}
              </ul>
            </section>
          )}

          {minutes.open_questions.length > 0 && (
            <section className="meeting-sec">
              <h3 className="meeting-sec-title">未决问题</h3>
              <ul className="meeting-itemlist">
                {minutes.open_questions.map((q, i) => (
                  <li key={i} className="meeting-item">
                    <span className="meeting-item-text">{q.text}</span>
                    <SourceChip source={q.source} onJump={onJump} />
                  </li>
                ))}
              </ul>
            </section>
          )}
        </div>
      </div>
    </div>
  );
}

function minutesEmpty(m: Minutes): boolean {
  return (
    m.one_liner.trim() === "" &&
    m.decisions.length === 0 &&
    m.action_items.length === 0 &&
    m.discussion.length === 0 &&
    m.open_questions.length === 0
  );
}

function ActionMeta({ item }: { item: ActionItem }) {
  const parts: string[] = [];
  if (item.owner?.trim()) parts.push(`负责人：${item.owner.trim()}`);
  if (item.due?.trim()) parts.push(`截止：${item.due.trim()}`);
  if (parts.length === 0) return null;
  return <span className="meeting-item-sub muted-text">{parts.join(" · ")}</span>;
}

// ---- transcript view (segment-level: seek + highlight + inline edit) -----
// Consecutive same-speaker segments merge into one turn (one avatar/timestamp),
// but each segment stays its own row so it can be clicked to seek the audio,
// highlighted while it plays, and edited in place. The minutes index still
// scrolls here via `jump`.

type Turn = {
  speakerId: string | null;
  segments: TranscriptSegment[];
};

function buildTurns(segments: TranscriptSegment[]): Turn[] {
  const turns: Turn[] = [];
  for (const seg of segments) {
    const sid = seg.speaker_id ?? null;
    const last = turns[turns.length - 1];
    // Only merge consecutive segments that share the *same known* speaker.
    // Unattributed segments (no speaker id) each stand alone — merging them
    // would wrongly collapse distinct unknown speakers into one turn.
    if (last && sid !== null && last.speakerId === sid) {
      last.segments.push(seg);
    } else {
      turns.push({ speakerId: sid, segments: [seg] });
    }
  }
  return turns;
}

/** Grow a textarea to fit its content so an edited sentence is fully visible. */
function autosizeTextarea(el: HTMLTextAreaElement) {
  el.style.height = "auto";
  el.style.height = `${el.scrollHeight}px`;
}

/** One transcript sentence: click the text to seek the audio, click ✎ to edit
 * the words in place. Owns its own edit state so a keyed re-render (e.g. the
 * playback highlight ticking) never drops an in-progress edit.
 *
 * Wrapped in `memo`: the playhead ticks ~4×/s during playback, but only the two
 * rows whose `active` flips (leaving one sentence, entering the next) actually
 * change props — every other row (and its edit state) is skipped. The callback
 * props are stable (`useCallback`), and `segment` keeps its identity unless its
 * own text was edited, so the memo comparison is cheap and correct. */
const SegmentRow = memo(function SegmentRow({
  segment,
  playable,
  active,
  flash,
  onSeek,
  onEdited,
  onError,
  registerRef,
}: {
  segment: TranscriptSegment;
  playable: boolean;
  active: boolean;
  flash: boolean;
  onSeek: (seconds: number) => void;
  onEdited: (segmentId: string, text: string) => void;
  onError: (e: string | null) => void;
  registerRef: (segmentId: string, el: HTMLElement | null) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(segment.text);
  const [saving, setSaving] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  const begin = useCallback(() => {
    setDraft(segment.text);
    setEditing(true);
  }, [segment.text]);

  useEffect(() => {
    if (!editing) return;
    const el = textareaRef.current;
    if (!el) return;
    el.focus();
    el.setSelectionRange(el.value.length, el.value.length);
    autosizeTextarea(el);
  }, [editing]);

  const commit = useCallback(async () => {
    if (saving) return;
    const next = draft.trim();
    // Refuse an empty save: a blank sentence would persist as "", render as a
    // ~0-height row whose hover-only ✎ is unreachable, and be effectively
    // unrecoverable. A transcript sentence should never be empty, so treat a
    // cleared line as a cancel — restore the original text and leave edit mode.
    if (next === "") {
      setDraft(segment.text);
      setEditing(false);
      return;
    }
    // Nothing changed → just leave edit mode without a round trip.
    if (next === segment.text.trim()) {
      setEditing(false);
      return;
    }
    setSaving(true);
    onError(null);
    try {
      await api.editMeetingSegment(segment.id, next);
      onEdited(segment.id, next);
      setEditing(false);
    } catch (e) {
      onError(String(e));
    } finally {
      setSaving(false);
    }
  }, [draft, saving, segment.id, segment.text, onEdited, onError]);

  const cancel = useCallback(() => {
    setDraft(segment.text);
    setEditing(false);
  }, [segment.text]);

  if (editing) {
    return (
      <div
        className="meeting-seg editing"
        ref={(el) => registerRef(segment.id, el)}
      >
        <textarea
          ref={textareaRef}
          className="meeting-seg-edit"
          value={draft}
          disabled={saving}
          onChange={(e) => {
            setDraft(e.currentTarget.value);
            autosizeTextarea(e.currentTarget);
          }}
          // Blur saves (per spec). Cancel/Save buttons preventDefault on
          // mousedown so they don't blur-save before their click runs.
          onBlur={() => void commit()}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              e.preventDefault();
              cancel();
            } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              void commit();
            }
          }}
        />
        <div className="meeting-seg-actions">
          <button
            type="button"
            className="btn small"
            disabled={saving}
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => void commit()}
          >
            保存
          </button>
          <button
            type="button"
            className="btn ghost small"
            disabled={saving}
            onMouseDown={(e) => e.preventDefault()}
            onClick={cancel}
          >
            取消
          </button>
        </div>
      </div>
    );
  }

  const cls = `meeting-seg${active ? " active" : ""}${flash ? " flash" : ""}`;
  return (
    <div className={cls} ref={(el) => registerRef(segment.id, el)}>
      {playable ? (
        <button
          type="button"
          className="meeting-seg-text seekable"
          title="跳到此处播放"
          onClick={() => onSeek(segment.start_seconds)}
        >
          {segment.text}
        </button>
      ) : (
        <span className="meeting-seg-text">{segment.text}</span>
      )}
      <button
        type="button"
        className="meeting-seg-edit-btn"
        title="编辑这句"
        aria-label="编辑这句"
        onClick={begin}
      >
        ✎
      </button>
    </div>
  );
});

function TranscriptView({
  detail,
  jump,
  currentTime,
  playable,
  onSeek,
  onSegmentEdited,
  onError,
}: {
  detail: MeetingDetail;
  jump: { seconds: number; token: number } | null;
  /** Current audio playhead (seconds) used to highlight the playing sentence. */
  currentTime: number;
  /** Whether an audio player is available (ready meeting with a recording). */
  playable: boolean;
  onSeek: (seconds: number) => void;
  onSegmentEdited: (segmentId: string, text: string) => void;
  onError: (e: string | null) => void;
}) {
  const turns = useMemo(() => buildTurns(detail.segments), [detail.segments]);
  const speakerById = useMemo(() => {
    const map = new Map<string, Speaker>();
    for (const s of detail.speakers) map.set(s.id, s);
    return map;
  }, [detail.speakers]);

  // Per-segment DOM refs, so a minutes-index jump can scroll to the right line.
  const segRefs = useRef<Map<string, HTMLElement>>(new Map());
  const registerRef = useCallback((id: string, el: HTMLElement | null) => {
    if (el) segRefs.current.set(id, el);
    else segRefs.current.delete(id);
  }, []);
  const [flashSegId, setFlashSegId] = useState<string | null>(null);

  // The sentence the playhead is currently inside (start ≤ t < end). Linear
  // scan is fine for a meeting's segment count and keeps the mapping obvious.
  const activeSegId = useMemo(() => {
    if (!playable) return null;
    for (const s of detail.segments) {
      if (currentTime >= s.start_seconds && currentTime < s.end_seconds) {
        return s.id;
      }
    }
    return null;
  }, [detail.segments, currentTime, playable]);

  // Scroll to (and briefly flash) the segment covering the requested seconds
  // when a jump arrives from the minutes index.
  useEffect(() => {
    if (!jump) return;
    const segs = detail.segments;
    let target = segs.find(
      (s) => jump.seconds >= s.start_seconds && jump.seconds < s.end_seconds,
    );
    if (!target) {
      // Fall back to the last segment that starts at/before the target.
      let best: TranscriptSegment | undefined;
      for (const s of segs) {
        if (s.start_seconds <= jump.seconds) best = s;
      }
      target = best;
    }
    if (!target) return;
    segRefs.current.get(target.id)?.scrollIntoView({
      behavior: "smooth",
      block: "center",
    });
    setFlashSegId(target.id);
    const t = window.setTimeout(() => setFlashSegId(null), 1600);
    return () => window.clearTimeout(t);
  }, [jump, detail.segments]);

  if (turns.length === 0) {
    return (
      <div className="card meeting-empty">
        <StatusNote detail={detail} kind="transcript" />
      </div>
    );
  }

  return (
    <div className="card meeting-transcript">
      <p className="muted-text meeting-transcript-note">
        {playable
          ? "逐字稿（按说话轮次分段）。点句子跳到对应录音位置，点 ✎ 修改文字。"
          : "逐字稿（按说话轮次分段）。点 ✎ 修改文字。"}
      </p>
      <ul className="meeting-turns">
        {turns.map((turn) => {
          const speaker = turn.speakerId
            ? speakerById.get(turn.speakerId)
            : null;
          const { name, confirmed } = speakerDisplay(speaker);
          const head = turn.segments[0];
          return (
            <li key={head.id} className="meeting-turn">
              <div className="meeting-turn-head">
                <span className="meeting-avatar sm" aria-hidden>
                  {name.slice(0, 1)}
                </span>
                <span className="meeting-turn-name">{name}</span>
                {head.channel === "system" && (
                  <span
                    className="meeting-channel-system"
                    title="来自系统音频（远端参会者）"
                  >
                    对方
                  </span>
                )}
                {!confirmed && (
                  <span className="meeting-unconfirmed">未确认</span>
                )}
                <span className="meeting-turn-time">
                  {formatClock(head.start_seconds)}
                </span>
              </div>
              <div className="meeting-turn-body">
                {turn.segments.map((seg) => (
                  <SegmentRow
                    key={seg.id}
                    segment={seg}
                    playable={playable}
                    active={activeSegId === seg.id}
                    flash={flashSegId === seg.id}
                    onSeek={onSeek}
                    onEdited={onSegmentEdited}
                    onError={onError}
                    registerRef={registerRef}
                  />
                ))}
              </div>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

/** Bottom playback bar for the review page: play/pause, scrubber, current /
 * total time. Owns the single shared <audio>; its `onTime` lifts the playhead
 * up to the detail view so the transcript can highlight the playing sentence.
 * The WAV loads over Tauri's asset protocol, so scrubbing is a real range seek
 * rather than a full download. */
function MeetingAudioBar({
  src,
  audioRef,
  currentTime,
  onTime,
  durationHint,
  onError,
}: {
  src: string;
  audioRef: MutableRefObject<HTMLAudioElement | null>;
  currentTime: number;
  onTime: (seconds: number) => void;
  durationHint: number | null;
  onError: (e: string | null) => void;
}) {
  const [duration, setDuration] = useState(durationHint ?? 0);
  const [playing, setPlaying] = useState(false);

  const total = duration > 0 ? duration : durationHint ?? 0;

  const toggle = useCallback(() => {
    const a = audioRef.current;
    if (!a) return;
    if (a.paused) void a.play().catch(() => {});
    else a.pause();
  }, [audioRef]);

  return (
    <div className="meeting-audiobar">
      <audio
        ref={audioRef}
        src={src}
        preload="metadata"
        onLoadedMetadata={(e) => {
          const d = e.currentTarget.duration;
          if (Number.isFinite(d) && d > 0) setDuration(d);
        }}
        onTimeUpdate={(e) => onTime(e.currentTarget.currentTime)}
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onEnded={() => setPlaying(false)}
        // The asset: URL can fail silently (scope/ACL reject, missing/renamed
        // WAV) — surface it so the bar isn't a dead control that swallows the
        // reason. `toggle`/`seekTo` also swallow play() rejections by design;
        // this is the one visible signal.
        onError={() => {
          setPlaying(false);
          onError("无法加载会议录音，音频可能缺失或无法访问。");
        }}
      />
      <button
        type="button"
        className="btn small meeting-audio-toggle"
        onClick={toggle}
      >
        {playing ? "暂停" : "播放"}
      </button>
      <span className="meeting-audio-time">{formatClock(currentTime)}</span>
      <input
        type="range"
        className="meeting-audio-scrub"
        min={0}
        max={total || 0}
        step={0.1}
        value={Math.min(currentTime, total || 0)}
        aria-label="播放进度"
        onChange={(e) => {
          const t = Number(e.currentTarget.value);
          onTime(t);
          const a = audioRef.current;
          if (a) a.currentTime = t;
        }}
      />
      <span className="meeting-audio-time">{formatClock(total)}</span>
    </div>
  );
}
