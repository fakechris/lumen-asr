import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import { HotkeyRecorder } from "./HotkeyRecorder";
import { OnboardingWizard } from "./OnboardingWizard";
import { formatHotkeyLabel } from "./hotkeyFormat";
import type {
  AsrStatus,
  AudioDevice,
  CorrectorStatus,
  DictionaryEntry,
  EditEvent,
  Health,
  LearnCandidate,
  LearningConfig,
  SessionRecord,
  TabId,
} from "./types";

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

const NAV: { id: TabId; label: string; icon: string; title: string; blurb: string }[] = [
  {
    id: "record",
    label: "录音",
    icon: "🎙",
    title: "录音",
    blurb: "本地转写 · 热键或按钮开始",
  },
  {
    id: "history",
    label: "历史",
    icon: "🕐",
    title: "历史",
    blurb: "会话记录与编辑事件",
  },
  {
    id: "dictionary",
    label: "词典",
    icon: "📖",
    title: "词典",
    blurb: "术语与替换规则",
  },
  {
    id: "learn",
    label: "学习",
    icon: "✨",
    title: "编辑学习",
    blurb: "从改写生成词典候选",
  },
  {
    id: "settings",
    label: "设置",
    icon: "⚙",
    title: "设置",
    blurb: "权限 · 热键 · 插入 · 修正 · 学习",
  },
  {
    id: "overview",
    label: "概览",
    icon: "⌂",
    title: "概览",
    blurb: "状态与快捷入口",
  },
];

export default function App() {
  const [tab, setTab] = useState<TabId>("record");
  const [health, setHealth] = useState<Health | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [hotkeyLabel, setHotkeyLabel] = useState("⌥Space");
  const [hotkeyEnabled, setHotkeyEnabledUi] = useState(true);
  const [showOnboarding, setShowOnboarding] = useState(false);
  const [onboardingIncomplete, setOnboardingIncomplete] = useState(false);

  const [sessions, setSessions] = useState<SessionRecord[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [edits, setEdits] = useState<EditEvent[]>([]);

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
      setSessions(await api.listSessions(100));
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refreshDict = useCallback(async () => {
    try {
      setDict(await api.listDictionary());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  // Post-paste learn suggestions from native watch
  useEffect(() => {
    let un: (() => void) | undefined;
    listen<{
      sessionId: string;
      beforeText: string;
      afterText: string;
      candidates: LearnCandidate[];
      message: string;
    }>("learn-suggestion", (e) => {
      const p = e.payload;
      setLearnBefore(p.beforeText);
      setLearnAfter(p.afterText);
      setCandidates(p.candidates || []);
      setSessionLearn({
        sessionId: p.sessionId,
        baseline: p.beforeText,
        candidates: p.candidates || [],
      });
      setTab("learn");
      setError(null);
    }).then((fn) => {
      un = fn;
    });
    return () => un?.();
  }, []);

  // Global hotkey / capsule events
  useEffect(() => {
    let un: (() => void) | undefined;
    listen<{
      phase: string;
      message?: string;
      outcome?: {
        text: string;
        asrText?: string;
        asrEngine?: string;
        correctorEngine?: string;
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
  }, [refreshHealth, refreshSessions]);

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

  useEffect(() => {
    if (tab === "history" || tab === "overview") void refreshSessions();
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

  useEffect(() => {
    if (!selectedId) {
      setEdits([]);
      return;
    }
    api
      .listEditEvents(selectedId)
      .then(setEdits)
      .catch((e) => setError(String(e)));
  }, [selectedId]);

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

  return (
    <div className="app-frame">
      {showOnboarding && (
        <OnboardingWizard
          onDone={() => {
            setShowOnboarding(false);
            setOnboardingIncomplete(false);
            void refreshHealth();
          }}
        />
      )}
      {/* System titlebar (Visible) — native macOS drag / traffic lights */}
      <div className="app-body">
        <nav className="sidebar" aria-label="主导航">
          {NAV.map((item) => (
            <button
              key={item.id}
              type="button"
              className={`nav-item ${tab === item.id ? "active" : ""}`}
              onClick={() => setTab(item.id)}
            >
              <span className="nav-icon" aria-hidden>
                {item.icon}
              </span>
              <span>{item.label}</span>
            </button>
          ))}
          <div className="sidebar-spacer" />
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
          {error && (
            <div className="banner error" role="alert">
              {error}
              <button type="button" className="linkish" onClick={() => setError(null)}>
                关闭
              </button>
            </div>
          )}

          <div className="content-scroll">
            <div className="content-header">
              <div>
                <h1>{nav.title}</h1>
                <p>{nav.blurb}</p>
              </div>
              {health && (
                <div className="actions" style={{ marginTop: 0 }}>
                  <span className="chip">v{health.version}</span>
                </div>
              )}
            </div>

            {tab === "record" && (
              <RecordPanel
                busy={busy}
                onError={setError}
                onBusy={setBusy}
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

            {tab === "overview" && (
              <Overview
                health={health}
                sessions={sessions}
                dictCount={dict.length}
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
                edits={edits}
                busy={busy}
                onSelect={setSelectedId}
                onSeed={() =>
                  run("seed", async () => {
                    const s = await api.seedDemoSession();
                    await refreshSessions();
                    setSelectedId(s.id);
                    await refreshHealth();
                  })
                }
                onDelete={(id) =>
                  run("delete session", async () => {
                    await api.deleteSession(id);
                    if (selectedId === id) setSelectedId(null);
                    await refreshSessions();
                    await refreshHealth();
                  })
                }
                onRecordEdit={(sessionId, before, after) =>
                  run("record edit", async () => {
                    await api.recordEditEvent({
                      sessionId,
                      beforeText: before,
                      afterText: after,
                      source: "pre_insert_ui",
                    });
                    setEdits(await api.listEditEvents(sessionId));
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
            {health?.sensevoice_ready
              ? "SenseVoice"
              : health?.whisper_ready
                ? "Whisper?"
                : "模型未就绪"}
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
          {health ? `${health.session_count} 会话 · ${health.dictionary_count} 词条` : ""}
        </span>
      </footer>
    </div>
  );
}

function RecordPanel({
  busy,
  onError,
  onBusy,
  onSaved,
  onLearnCandidates,
  hotkeyLabel,
}: {
  busy: boolean;
  onError: (e: string | null) => void;
  onBusy: (b: boolean) => void;
  onSaved: () => Promise<void>;
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

  const refreshStatus = useCallback(async () => {
    try {
      const s = await api.getAsrStatus();
      setStatus(s);
      setRecording(s.recording);
    } catch (e) {
      onError(String(e));
    }
  }, [onError]);

  useEffect(() => {
    void (async () => {
      try {
        const list = await api.listAudioDevices();
        setDevices(list);
        const def = list.find((d) => d.is_default) ?? list[0];
        if (def) {
          setDevice(def.name);
          await api.setAudioDevice(def.name);
        }
      } catch (e) {
        onError(String(e));
      }
      await refreshStatus();
    })();
  }, [onError, refreshStatus]);

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
      };
      if (!detail) return;
      const finalText = detail.text || detail.correctedText || "";
      setText(finalText);
      setAsrText(detail.asrText || "");
      setBaseline(finalText);
      setSessionId(detail.session?.id ?? null);
      setLiveCandidates([]);
      setMeta(
        `hotkey · ASR ${detail.asrEngine || "?"} · ${detail.correctorEngine || ""}`
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

  async function onEngineChange(engine: string) {
    onBusy(true);
    onError(null);
    try {
      await api.setAsrEngine(engine);
      await refreshStatus();
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  }

  async function start() {
    onBusy(true);
    onError(null);
    setText("");
    setAsrText("");
    setMeta("");
    try {
      await api.startRecording();
      setRecording(true);
      await refreshStatus();
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
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
      const corr = out.modelApplied
        ? `corrector ${out.correctorEngine}`
        : `corrector fallback (${out.correctorEngine})`;
      setMeta(
        `ASR ${out.asrEngine} · ${corr} · ${(out.durationMs / 1000).toFixed(1)}s · ${out.numSamples} samples`
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

  const engine = status?.engine ?? "sensevoice";
  const ready = status?.activeReady ?? false;

  return (
    <>
      <section className="card">
        <h2>本地转写</h2>
        <p className="muted-text">
          默认 SenseVoice。全局热键{" "}
          <span className="kbd">{hotkeyLabel}</span> 可在任意 App
          切换录音/停止。模型就绪后即可用。
        </p>
        <div className="form-row" style={{ marginBottom: 12 }}>
          <label className="muted-text" style={{ minWidth: 64 }}>
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
        <div className="form-row" style={{ marginBottom: 12 }}>
          <label className="muted-text" style={{ minWidth: 64 }}>
            引擎
          </label>
          <select
            className="input"
            value={engine}
            disabled={recording || busy}
            onChange={(e) => void onEngineChange(e.target.value)}
          >
            <option value="sensevoice">
              SenseVoice {status?.sensevoice.ready ? "✓" : "（模型未就绪）"}
            </option>
            <option value="whisper">
              Whisper {status?.whisper.ready ? "✓" : "（模型未就绪）"}
            </option>
          </select>
        </div>
        {status && (
          <p className="muted-text" style={{ fontSize: "0.85rem" }}>
            模型目录：
            <code>
              {engine === "whisper"
                ? status.whisper.model_dir
                : status.sensevoice.model_dir}
            </code>
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
        {!ready && (
          <p className="muted-text" style={{ marginTop: 12 }}>
            当前引擎模型未就绪。将 SenseVoice 的{" "}
            <code>model.int8.onnx</code> + <code>tokens.txt</code> 放到上述目录，或设置环境变量{" "}
            <code>LUMEN_SENSEVOICE_DIR</code> / <code>LUMEN_WHISPER_DIR</code>。
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
  const [model, setModel] = useState("qwen2.5:7b");
  const [apiKey, setApiKey] = useState("");
  const [timeoutSecs, setTimeoutSecs] = useState(60);
  const [probe, setProbe] = useState<string>("");
  const [perm, setPerm] = useState<import("./api").PermissionStatus | null>(null);
  const [autoInsert, setAutoInsert] = useState(true);
  const [injectMode, setInjectMode] = useState("auto");
  const [preserveClip, setPreserveClip] = useState(true);
  const [hotkeyEnabled, setHotkeyEnabled] = useState(true);
  const [hotkeyToggle, setHotkeyToggle] = useState("Alt+Space");
  const [showCapsule, setShowCapsule] = useState(true);
  const [hotkeyMode, setHotkeyMode] = useState("hold");
  const [learning, setLearning] = useState<LearningConfig | null>(null);
  const [autoPromote, setAutoPromote] = useState(false);
  const [promoteN, setPromoteN] = useState(3);
  const [postPaste, setPostPaste] = useState(true);
  const [postPasteSecs, setPostPasteSecs] = useState(20);

  useEffect(() => {
    void (async () => {
      try {
        const c = await api.getCorrectorConfig();
        setCfg(c);
        setEnabled(c.enabled);
        setProvider(c.provider);
        setBaseUrl(c.baseUrl);
        setModel(c.model);
        setTimeoutSecs(c.timeoutSecs);
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
        const ln = await api.getLearningConfig();
        setLearning(ln);
        setAutoPromote(ln.autoPromote);
        setPromoteN(ln.autoPromoteThreshold);
        setPostPaste(ln.postPasteCapture);
        setPostPasteSecs(ln.postPasteSeconds);
      } catch (e) {
        onError(String(e));
      }
    })();
  }, [onError]);

  async function save() {
    onBusy(true);
    onError(null);
    try {
      const input: Parameters<typeof api.saveCorrectorConfig>[0] = {
        enabled,
        provider,
        baseUrl,
        model,
        timeoutSecs,
      };
      if (apiKey.trim()) {
        input.apiKey = apiKey.trim();
      }
      const c = await api.saveCorrectorConfig(input);
      setCfg(c);
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
        <p className="muted-text">
          麦克风：系统会弹窗授权。辅助功能：必须在系统设置里手动打开
          <strong>当前正在运行</strong>的那个进程（开发版与正式 .app 是两条独立记录）。
        </p>
        {perm && (
          <dl className="meta">
            <dt>麦克风</dt>
            <dd>{perm.microphone}</dd>
            <dt>辅助功能</dt>
            <dd>
              {perm.accessibilityTrusted ? "已开启" : "需要开启"}
              {perm.accessibilityTrusted ? "" : "（未开启时只能复制到剪贴板）"}
            </dd>
            <dt>列表中名称</dt>
            <dd>
              <code>{perm.processHint}</code>
            </dd>
            <dt>完整路径</dt>
            <dd style={{ wordBreak: "break-all" }}>
              <code>{perm.processPath}</code>
            </dd>
          </dl>
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
                  setPerm(await api.requestMicrophoneAccess());
                } catch (e) {
                  onError(String(e));
                } finally {
                  onBusy(false);
                }
              })()
            }
          >
            请求麦克风
          </button>
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
            className="btn"
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
            打开辅助功能设置
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
        <h2>插入策略</h2>
        <div className="form-row" style={{ marginBottom: 10 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
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
          默认 Ollama OpenAI-compatible 接口。失败时自动回退到规则预处理 + 词典替换，不中断会话。
        </p>
        <div className="form-row" style={{ marginBottom: 10 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
            Provider
          </label>
          <select
            className="input"
            value={provider}
            disabled={busy}
            onChange={(e) => setProvider(e.target.value)}
          >
            <option value="ollama">Ollama</option>
            <option value="openai_compatible">OpenAI-compatible</option>
            <option value="none">none（仅规则）</option>
          </select>
        </div>
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
            Model
          </label>
          <input
            className="input"
            value={model}
            disabled={busy}
            onChange={(e) => setModel(e.target.value)}
            placeholder="qwen2.5:7b"
          />
        </div>
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
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
          <button type="button" className="btn" disabled={busy} onClick={() => void save()}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
            监听秒数
          </label>
          <input
            className="input"
            type="number"
            min={5}
            max={120}
            value={postPasteSecs}
            disabled={busy}
            onChange={(e) => setPostPasteSecs(Number(e.target.value) || 20)}
          />
        </div>
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
    </>
  );
}

function Overview({
  health,
  sessions,
  dictCount,
  busy,
  onSeed,
  onGoto,
}: {
  health: Health | null;
  sessions: SessionRecord[];
  dictCount: number;
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
            <dt>会话</dt>
            <dd>{health.session_count}</dd>
            <dt>词典条目</dt>
            <dd>{health.dictionary_count}</dd>
            <dt>SenseVoice</dt>
            <dd>{health.sensevoice_ready ? "就绪" : "未就绪"}</dd>
            <dt>Whisper</dt>
            <dd>{health.whisper_ready ? "就绪" : "未就绪"}</dd>
            <dt>Corrector</dt>
            <dd>
              {health.corrector_enabled ? health.corrector_label : "关闭"}
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
                <span>{previewText(s.pasted || s.corrected || s.asr_raw)}</span>
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

function HistoryPanel({
  sessions,
  selected,
  edits,
  busy,
  onSelect,
  onSeed,
  onDelete,
  onRecordEdit,
}: {
  sessions: SessionRecord[];
  selected: SessionRecord | null;
  edits: EditEvent[];
  busy: boolean;
  onSelect: (id: string | null) => void;
  onSeed: () => void;
  onDelete: (id: string) => void;
  onRecordEdit: (sessionId: string, before: string, after: string) => void;
}) {
  const [editAfter, setEditAfter] = useState("");

  useEffect(() => {
    setEditAfter(selected?.pasted || selected?.corrected || selected?.asr_raw || "");
  }, [selected?.id, selected?.pasted, selected?.corrected, selected?.asr_raw]);

  return (
    <div className="split">
      <section className="card list-pane">
        <div className="card-head">
          <h2>历史</h2>
          <button type="button" className="btn small" disabled={busy} onClick={onSeed}>
            + 示例
          </button>
        </div>
        {sessions.length === 0 ? (
          <p className="muted-text">空</p>
        ) : (
          <ul className="session-list">
            {sessions.map((s) => (
              <li key={s.id}>
                <button
                  type="button"
                  className={`session-item ${selected?.id === s.id ? "active" : ""}`}
                  onClick={() => onSelect(s.id)}
                >
                  <span className="list-time">{formatTime(s.created_at)}</span>
                  <span className="session-preview">
                    {previewText(s.pasted || s.corrected || s.asr_raw, 48)}
                  </span>
                  <span className="chip">{s.status}</span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="card detail-pane">
        {!selected ? (
          <p className="muted-text">选择一条会话查看详情</p>
        ) : (
          <>
            <div className="card-head">
              <h2>详情</h2>
              <button
                type="button"
                className="btn small danger"
                disabled={busy}
                onClick={() => onDelete(selected.id)}
              >
                删除
              </button>
            </div>
            <dl className="meta">
              <dt>时间</dt>
              <dd>{formatTime(selected.created_at)}</dd>
              <dt>App</dt>
              <dd>{selected.focus?.app_name || "—"}</dd>
              <dt>ASR</dt>
              <dd>{selected.asr_engine || "—"}</dd>
              <dt>插入</dt>
              <dd>{selected.insert_strategy}</dd>
            </dl>
            <Field label="ASR 原文" value={selected.asr_raw} />
            <Field label="修正后" value={selected.corrected} />
            <Field label="最终粘贴" value={selected.pasted} />

            <h3 className="subhead">模拟预览编辑</h3>
            <p className="muted-text">
              改字后保存为 edit_event（M6 会从系统焦点框自动捕获）。
            </p>
            <textarea
              className="textarea"
              rows={4}
              value={editAfter}
              onChange={(e) => setEditAfter(e.target.value)}
            />
            <div className="actions">
              <button
                type="button"
                className="btn"
                disabled={busy}
                onClick={() => {
                  const before =
                    selected.corrected || selected.asr_raw || selected.pasted || "";
                  onRecordEdit(selected.id, before, editAfter);
                }}
              >
                记录编辑
              </button>
            </div>

            <h3 className="subhead">编辑事件 ({edits.length})</h3>
            {edits.length === 0 ? (
              <p className="muted-text">无</p>
            ) : (
              <ul className="list">
                {edits.map((e) => (
                  <li key={e.id} className="edit-item">
                    <span className="chip">{e.source}</span>
                    <div>
                      <div className="diff-before">{e.before_text}</div>
                      <div className="diff-after">{e.after_text}</div>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </>
        )}
      </section>
    </div>
  );
}

function Field({ label, value }: { label: string; value?: string | null }) {
  return (
    <div className="field-block">
      <div className="field-label">{label}</div>
      <pre className="field-value">{value || "—"}</pre>
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
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}
