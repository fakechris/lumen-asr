import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api";
import { Icon } from "./Icons";
import { diarGuidance, isNoLlmMarker } from "./meetingGuidance";
import type {
  ActionItem,
  ExportPreset,
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

  if (selectedId) {
    return (
      <MeetingDetailView
        meetingId={selectedId}
        onBack={() => setSelectedId(null)}
        onError={onError}
        onNavigate={onNavigate}
      />
    );
  }
  return <MeetingLibrary onOpen={setSelectedId} onError={onError} />;
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
}: {
  onOpen: (id: string) => void;
  onError: (e: string | null) => void;
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
    } catch (e) {
      onError(String(e));
    } finally {
      setStarting(false);
    }
  }, [titleDraft, refresh, query, onError]);

  const onStopped = useCallback(() => {
    setActive(null);
    void refresh(query);
  }, [refresh, query]);

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
                      <MeetingRow meeting={m} onOpen={() => onOpen(m.id)} />
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}

// ---- inline recording state --------------------------------------------
// A deliberately restrained recording strip: ● 正在录制 + elapsed timer +
// pause/resume + stop. No in-meeting editing, no live transcript, and no mic
// level (there is no meeting-recorder level interface yet). Speaker separation
// and the transcript all happen offline after stop.
function RecordingBar({
  meetingId,
  startedAtMs,
  onStopped,
  onError,
}: {
  meetingId: string;
  startedAtMs: number;
  onStopped: () => void;
  onError: (e: string | null) => void;
}) {
  // Seconds elapsed since start; frozen while paused. Seed from the (possibly
  // reconstructed) start time so a remount mid-recording shows the right clock.
  const initial = Number.isFinite(startedAtMs)
    ? Math.max(0, Math.floor((Date.now() - startedAtMs) / 1000))
    : 0;
  const [seconds, setSeconds] = useState(initial);
  const [paused, setPaused] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (paused) return;
    const id = window.setInterval(() => setSeconds((s) => s + 1), 1000);
    return () => window.clearInterval(id);
  }, [paused]);

  async function togglePause() {
    setBusy(true);
    onError(null);
    try {
      if (paused) {
        await api.resumeMeetingRecording();
        setPaused(false);
      } else {
        await api.pauseMeetingRecording();
        setPaused(true);
      }
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
    }
  }

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
    <div className={`meeting-recording ${paused ? "paused" : ""}`}>
      <span className="meeting-rec-dot" aria-hidden />
      <span className="meeting-rec-label">
        {paused ? "已暂停" : "正在录制"}
      </span>
      <span className="meeting-rec-timer" aria-live="off">
        {formatClock(seconds)}
      </span>
      <span className="meeting-rec-hint muted-text">听写已暂停</span>
      <span className="meeting-rec-actions">
        <button
          type="button"
          className="btn ghost small"
          disabled={busy}
          onClick={() => void togglePause()}
        >
          {paused ? "继续" : "暂停"}
        </button>
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
}: {
  meeting: Meeting;
  onOpen: () => void;
}) {
  return (
    <button type="button" className="meeting-row" onClick={onOpen}>
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
  );
}

// ---- detail (shell: summary / transcript, default summary) --------------

type DetailView = "summary" | "transcript";

function MeetingDetailView({
  meetingId,
  onBack,
  onError,
  onNavigate,
}: {
  meetingId: string;
  onBack: () => void;
  onError: (e: string | null) => void;
  onNavigate?: (tab: TabId) => void;
}) {
  const [detail, setDetail] = useState<MeetingDetail | null>(null);
  const [view, setView] = useState<DetailView>("summary");
  const [loading, setLoading] = useState(true);
  const [exporting, setExporting] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  // A jump request from a minutes item → transcript. The token forces the
  // transcript to (re)scroll even when the target seconds repeat.
  const [jump, setJump] = useState<{ seconds: number; token: number } | null>(
    null,
  );

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
    setView("transcript");
    setJump({ seconds: src.start, token: Date.now() });
  }, []);

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
          <h2 className="meeting-detail-title">
            {meeting ? meetingTitle(meeting) : "加载中…"}
          </h2>
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
          <div className="meeting-viewtabs" role="tablist">
            <button
              type="button"
              role="tab"
              aria-selected={view === "summary"}
              className={`meeting-viewtab ${view === "summary" ? "active" : ""}`}
              onClick={() => setView("summary")}
            >
              纪要
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={view === "transcript"}
              className={`meeting-viewtab ${view === "transcript" ? "active" : ""}`}
              onClick={() => setView("transcript")}
            >
              逐字稿
            </button>
          </div>
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
      ) : view === "summary" ? (
        <SummaryView
          detail={detail}
          minutes={minutes}
          onJump={jumpToSource}
          onNavigate={onNavigate}
        />
      ) : (
        <TranscriptView detail={detail} jump={jump} />
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

function SummaryView({
  detail,
  minutes,
  onJump,
  onNavigate,
}: {
  detail: MeetingDetail;
  minutes: Minutes | null;
  onJump: (src: SourceRef) => void;
  onNavigate?: (tab: TabId) => void;
}) {
  const hasMinutes = minutes != null && !minutesEmpty(minutes);
  const noLlm = noLlmMinutes(detail);

  return (
    <div className="split meeting-summary-layout">
      <div className="card meeting-summary-main">
        {!hasMinutes ? (
          detail.meeting.status === "ready" ? (
            noLlm ? (
              <div className="meeting-statusnote">
                <div>
                  <strong>未配置 LLM</strong>
                  <p className="muted-text">
                    逐字稿已生成，但尚未配置语言模型，无法自动生成会议纪要
                    （摘要 / 决策 / 行动项）。在设置里配置一个 LLM 后即可自动生成。
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
              </div>
            ) : (
              <div className="meeting-statusnote">
                <div>
                  <strong>暂无纪要</strong>
                  <p className="muted-text">
                    这场会议没有可用的结构化纪要（可能是总结步骤未产出内容）。
                  </p>
                </div>
              </div>
            )
          ) : (
            <StatusNote detail={detail} kind="summary" />
          )
        ) : (
          <>
            {minutes!.one_liner.trim() && (
              <section className="meeting-sec">
                <p className="meeting-oneliner">{minutes!.one_liner}</p>
              </section>
            )}

            {minutes!.decisions.length > 0 && (
              <section className="meeting-sec">
                <h3 className="meeting-sec-title">决策</h3>
                <ul className="meeting-itemlist">
                  {minutes!.decisions.map((d, i) => (
                    <li key={i} className="meeting-item">
                      <span className="meeting-item-text">{d.text}</span>
                      <SourceChip source={d.source} onJump={onJump} />
                    </li>
                  ))}
                </ul>
              </section>
            )}

            {minutes!.action_items.length > 0 && (
              <section className="meeting-sec">
                <h3 className="meeting-sec-title">行动项</h3>
                <ul className="meeting-itemlist">
                  {minutes!.action_items.map((a, i) => (
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

            {minutes!.discussion.length > 0 && (
              <section className="meeting-sec">
                <h3 className="meeting-sec-title">关键讨论</h3>
                <ul className="meeting-itemlist">
                  {minutes!.discussion.map((d, i) => (
                    <li key={i} className="meeting-item">
                      <span className="meeting-item-text">{d.topic}</span>
                      <SourceChip source={d.source} onJump={onJump} />
                    </li>
                  ))}
                </ul>
              </section>
            )}

            {minutes!.open_questions.length > 0 && (
              <section className="meeting-sec">
                <h3 className="meeting-sec-title">未决问题</h3>
                <ul className="meeting-itemlist">
                  {minutes!.open_questions.map((q, i) => (
                    <li key={i} className="meeting-item">
                      <span className="meeting-item-text">{q.text}</span>
                      <SourceChip source={q.source} onJump={onJump} />
                    </li>
                  ))}
                </ul>
              </section>
            )}
          </>
        )}
      </div>

      <aside className="card meeting-side">
        <section className="meeting-side-sec">
          <h3 className="meeting-sec-title">参与者</h3>
          {detail.speakers.length === 0 ? (
            <p className="muted-text">尚未识别到说话人。</p>
          ) : (
            <ul className="meeting-participants">
              {detail.speakers.map((s) => {
                const { name, confirmed } = speakerDisplay(s);
                return (
                  <li key={s.id} className="meeting-participant">
                    <span className="meeting-avatar" aria-hidden>
                      {name.slice(0, 1)}
                    </span>
                    <span className="meeting-participant-name">{name}</span>
                    {!confirmed && (
                      <span className="meeting-unconfirmed">未确认</span>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
        </section>
        <section className="meeting-side-sec">
          <h3 className="meeting-sec-title">会议信息</h3>
          <dl className="meeting-info">
            <dt>时长</dt>
            <dd>{formatDuration(detail.meeting.duration_seconds)}</dd>
            <dt>日期</dt>
            <dd>{formatFullDate(detail.meeting.created_at)}</dd>
            <dt>来源</dt>
            <dd>本地麦克风</dd>
          </dl>
        </section>
      </aside>
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

// ---- transcript view (read-only, turn-merged) ---------------------------
// M4b ships the minimal read-only reader: consecutive same-speaker segments
// merge into one turn with a single timestamp, and minutes items can scroll
// here. The bottom player + speaker-correction UI are M4c (not in this stage).

type Turn = {
  speakerId: string | null;
  startSeconds: number;
  endSeconds: number;
  text: string;
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
      last.text = `${last.text} ${seg.text}`.trim();
      last.endSeconds = seg.end_seconds;
    } else {
      turns.push({
        speakerId: sid,
        startSeconds: seg.start_seconds,
        endSeconds: seg.end_seconds,
        text: seg.text,
      });
    }
  }
  return turns;
}

function TranscriptView({
  detail,
  jump,
}: {
  detail: MeetingDetail;
  jump: { seconds: number; token: number } | null;
}) {
  const turns = useMemo(() => buildTurns(detail.segments), [detail.segments]);
  const speakerById = useMemo(() => {
    const map = new Map<string, Speaker>();
    for (const s of detail.speakers) map.set(s.id, s);
    return map;
  }, [detail.speakers]);

  const rowRefs = useRef<(HTMLLIElement | null)[]>([]);
  const [highlight, setHighlight] = useState<number | null>(null);

  // Scroll to the turn covering the requested seconds when a jump arrives.
  useEffect(() => {
    if (!jump) return;
    const idx = turns.findIndex(
      (t) => jump.seconds >= t.startSeconds && jump.seconds < t.endSeconds,
    );
    const target =
      idx >= 0
        ? idx
        : // fall back to the last turn that starts at/before the target
          (() => {
            let best = -1;
            for (let i = 0; i < turns.length; i += 1) {
              if (turns[i].startSeconds <= jump.seconds) best = i;
            }
            return best;
          })();
    if (target < 0) return;
    rowRefs.current[target]?.scrollIntoView({
      behavior: "smooth",
      block: "center",
    });
    setHighlight(target);
    const t = window.setTimeout(() => setHighlight(null), 1600);
    return () => window.clearTimeout(t);
  }, [jump, turns]);

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
        只读逐字稿（按说话轮次分段）。播放器与说话人修正在下一阶段（M4c）加入。
      </p>
      <ul className="meeting-turns">
        {turns.map((turn, i) => {
          const speaker = turn.speakerId
            ? speakerById.get(turn.speakerId)
            : null;
          const { name, confirmed } = speakerDisplay(speaker);
          return (
            <li
              key={i}
              ref={(el) => {
                rowRefs.current[i] = el;
              }}
              className={`meeting-turn ${highlight === i ? "flash" : ""}`}
            >
              <div className="meeting-turn-head">
                <span className="meeting-avatar sm" aria-hidden>
                  {name.slice(0, 1)}
                </span>
                <span className="meeting-turn-name">{name}</span>
                {!confirmed && (
                  <span className="meeting-unconfirmed">未确认</span>
                )}
                <span className="meeting-turn-time">
                  {formatClock(turn.startSeconds)}
                </span>
              </div>
              <p className="meeting-turn-text">{turn.text}</p>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
