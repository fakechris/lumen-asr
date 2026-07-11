import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import { HotkeyRecorder } from "./HotkeyRecorder";
import { OnboardingWizard } from "./OnboardingWizard";
import { formatHotkeyLabel } from "./hotkeyFormat";
import { Icon, type IconName } from "./Icons";
import { ChordCaptureChip } from "./ChordCaptureChip";
import type {
  AsrStatus,
  AudioDevice,
  CorrectorStatus,
  DictionaryEntry,
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

const NAV: { id: TabId; label: string; icon: IconName; title: string; blurb: string }[] = [
  {
    id: "record",
    label: "录音",
    icon: "mic",
    title: "录音",
    blurb: "本地转写 · 热键或按钮开始",
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
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [hotkeyLabel, setHotkeyLabel] = useState("⌥Space");
  const [hotkeyEnabled, setHotkeyEnabledUi] = useState(true);
  const [showOnboarding, setShowOnboarding] = useState(false);
  const [onboardingIncomplete, setOnboardingIncomplete] = useState(false);

  const [sessions, setSessions] = useState<SessionRecord[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);

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
          <div className="sidebar-brand">
            <div className="sidebar-brand-mark" aria-hidden />
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

  async function onEngineChange(providerId: string) {
    onBusy(true);
    onError(null);
    try {
      // Single source of truth: Settings ASR config + local EngineKind for local models.
      await api.saveAsrServiceConfig({ provider: providerId });
      await api.setAsrEngine(providerId);
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

  // Synced with 设置 → 语音识别 (config.asr.provider).
  const provider =
    status?.provider ||
    (status?.engine === "whisper" ? "local_whisper" : "local_sensevoice");
  const ready = status?.activeReady ?? false;
  const isLocal = provider.startsWith("local") || provider === "sensevoice" || provider === "whisper";

  return (
    <>
      <section className="card">
        <h2>录音转写</h2>
        <p className="muted-text">
          引擎与「设置 → 语音识别」为同一配置。全局热键{" "}
          <span className="kbd">{hotkeyLabel}</span>{" "}
          在任意 App 按住说话。当前：
          <strong> {status?.providerLabel || provider}</strong>
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
        {!ready && isLocal && (
          <p className="muted-text" style={{ marginTop: 12 }}>
            当前本地引擎未就绪。将 SenseVoice 的{" "}
            <code>model.int8.onnx</code> + <code>tokens.txt</code> 放到模型目录，或到「设置 →
            语音识别」切换其它 ASR。
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
  const [asrBaseUrl, setAsrBaseUrl] = useState("");
  const [asrModel, setAsrModel] = useState("");
  const [asrApiKey, setAsrApiKey] = useState("");
  const [asrLanguage, setAsrLanguage] = useState("");
  const [asrHasKey, setAsrHasKey] = useState(false);
  const [cleanup, setCleanup] = useState("medium");
  const [style, setStyle] = useState("neutral");
  const [casing, setCasing] = useState("sentence");
  const [punctuation, setPunctuation] = useState("standard");
  const [polish, setPolish] = useState<string[]>([]);
  const [customEnabled, setCustomEnabled] = useState(false);
  const [customInstruction, setCustomInstruction] = useState("");
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

  useEffect(() => {
    void (async () => {
      try {
        const [c, presets, asrP, asrC] = await Promise.all([
          api.getCorrectorConfig(),
          api.listLlmPresets(),
          api.listAsrPresets(),
          api.getAsrServiceConfig(),
        ]);
        setCfg(c);
        setLlmPresets(presets);
        setAsrPresets(asrP);
        setAsrProvider(asrC.provider);
        setAsrBaseUrl(asrC.baseUrl);
        setAsrModel(asrC.model);
        setAsrLanguage(asrC.language || "");
        setAsrHasKey(asrC.hasApiKey);
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
        cleanup,
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
      setCleanup(c.cleanup || cleanup);
      setStyle(c.style || style);
      setCasing(c.casing || casing);
      setPunctuation(c.punctuation || punctuation);
      setPolish(c.polish || polish);
      setCustomEnabled(!!c.customEnabled);
      setCustomInstruction(c.customInstruction || "");
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
          麦克风：系统弹窗授权。辅助功能：系统<strong>不会</strong>弹授权窗，必须在「系统设置 →
          隐私与安全性 → 辅助功能」里打开<strong>当前进程</strong>的开关。检测用的是系统 API
          <code>AXIsProcessTrusted</code>，不是猜的。
        </p>
        {perm && (
          <>
            <dl className="meta">
              <dt>麦克风</dt>
              <dd>{perm.microphone}</dd>
              <dt>辅助功能</dt>
              <dd>
                {perm.accessibilityTrusted ? "已开启" : "需要开启"}
                {perm.accessibilityTrusted ? "" : "（未开启时只能复制到剪贴板）"}
              </dd>
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
            {!perm.accessibilityTrusted && (
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
        <h2>语音识别（ASR）</h2>
        <p className="muted-text">
          默认本地 SenseVoice。也可选用 OpenAI 兼容的云端识别；部分厂商入口已预置，完整流式接入会分阶段开放。
        </p>
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
            Provider
          </label>
          <select
            className="input"
            value={asrProvider}
            disabled={busy}
            onChange={(e) => {
              const id = e.target.value;
              setAsrProvider(id);
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
        {!asrProvider.startsWith("local") && (
          <>
            <div className="form-row" style={{ marginBottom: 10 }}>
              <label className="muted-text" style={{ minWidth: 72 }}>
                Base URL
              </label>
              <input
                className="input"
                value={asrBaseUrl}
                disabled={busy}
                onChange={(e) => setAsrBaseUrl(e.target.value)}
              />
            </div>
            <div className="form-row" style={{ marginBottom: 10 }}>
              <label className="muted-text" style={{ minWidth: 72 }}>
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
            <div className="form-row" style={{ marginBottom: 10 }}>
              <label className="muted-text" style={{ minWidth: 72 }}>
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
            <div className="form-row" style={{ marginBottom: 10 }}>
              <label className="muted-text" style={{ minWidth: 72 }}>
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
                    baseUrl: asrBaseUrl,
                    model: asrModel,
                    language: asrLanguage,
                  };
                  if (asrApiKey.trim()) input.apiKey = asrApiKey.trim();
                  const s = await api.saveAsrServiceConfig(input);
                  setAsrProvider(s.provider);
                  setAsrBaseUrl(s.baseUrl);
                  setAsrModel(s.model);
                  setAsrLanguage(s.language);
                  setAsrHasKey(s.hasApiKey);
                  setAsrApiKey("");
                  // Keep Record tab engine dropdown in sync (local EngineKind + provider).
                  try {
                    await api.setAsrEngine(s.provider);
                  } catch {
                    /* cloud-only providers keep local engine kind */
                  }
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
        <h2>翻译快捷键</h2>
        <p className="muted-text">
          另一组键，专门「说话 → 整理 → <strong>译成目标语言</strong>」。
          和上面的录音热键一样：{hotkeyMode === "toggle" ? "再按一次结束" : "按住说话，松手结束"}
          （触发方式只在上面「全局热键」里改，这里不重复设）。
        </p>
        {(() => {
          const tr =
            intents.find((i) => i.intent === "translate") ||
            intents[0] || {
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

          async function saveTranslate(next: {
            enabled: boolean;
            chord: string;
            targetLanguage: string;
          }) {
            onBusy(true);
            onError(null);
            try {
              const list = [
                {
                  id: "translate",
                  chord: next.chord || "Alt+Shift+T",
                  mode: hotkeyMode === "toggle" ? "toggle" : "hold",
                  intent: "translate",
                  targetLanguage: next.targetLanguage || "en",
                  enabled: next.enabled,
                },
              ];
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
                          enabled: !!tr.enabled,
                        },
                      ])
                    }
                    onBlur={() =>
                      void saveTranslate({
                        enabled: !!tr.enabled,
                        chord: tr.chord || "Alt+Shift+T",
                        targetLanguage: tr.targetLanguage || "en",
                      })
                    }
                    placeholder="语言代码"
                  />
                )}
              </div>

              <p className="muted-text intent-hint">
                {tr.enabled
                  ? hotkeyMode === "toggle"
                    ? `按一下开始录音，再按结束 → 整理后译成「${tr.targetLanguage || "en"}」。应弹出录音胶囊。`
                    : `按住 ${tr.chord || "⌥⇧T"} 说话，松手 → 整理后译成「${tr.targetLanguage || "en"}」。应弹出录音胶囊。`
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
        <div className="cleanup-level-block">
          <div className="field-label">自动整理强度</div>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
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
        <div className="form-row" style={{ marginBottom: 10 }}>
          <label className="muted-text" style={{ minWidth: 72 }}>
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
          <label className="muted-text" style={{ minWidth: 72 }}>
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
            placeholder="qwen3.5:9b"
            list="llm-model-list"
          />
          <datalist id="llm-model-list">
            {(llmPresets.find((p) => p.id === provider)?.models || []).map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
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

/** Quality of a session result — drives recovery UI, not decoration. */
function sessionQuality(s: SessionRecord): "ok" | "weak" | "empty" {
  const t = (s.corrected || s.pasted || s.asr_raw || "").trim();
  if (!t) return "empty";
  if (t.length <= 2 || t === "。" || t === "." || t === "…") return "weak";
  return "ok";
}

function sessionMainText(s: SessionRecord): string {
  return (s.corrected || s.pasted || s.asr_raw || "").trim();
}

function HistoryPanel({
  sessions,
  selected,
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
  busy: boolean;
  onSelect: (id: string | null) => void;
  onRefresh: () => void;
  onBusy: (b: boolean) => void;
  onError: (e: string | null) => void;
  onUpdated: (s: SessionRecord) => void;
  onDelete: (id: string) => void;
}) {
  const [playing, setPlaying] = useState(false);
  const [copied, setCopied] = useState(false);
  const [showPipeline, setShowPipeline] = useState(false);
  const [retryNote, setRetryNote] = useState<string | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const blobUrlRef = useRef<string | null>(null);

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
    setCopied(false);
    setShowPipeline(false);
    setRetryNote(null);
  }, [selected?.id, stopAudio]);

  useEffect(() => () => stopAudio(), [stopAudio]);

  async function copyMain() {
    if (!selected) return;
    const text = sessionMainText(selected);
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    } catch (e) {
      onError(`复制失败: ${String(e)}`);
    }
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
      const after = sessionMainText(out.session);
      if (after && after !== before) {
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
              return (
                <li key={s.id}>
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
                    selected.asr_engine,
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
                  className={`icon-btn ${copied ? "copied" : ""}`}
                  disabled={busy || !text}
                  onClick={() => void copyMain()}
                  title={copied ? "已复制" : "复制文本（双击正文亦可）"}
                  aria-label={copied ? "已复制" : "复制文本"}
                >
                  <Icon name={copied ? "copy-check" : "copy"} size={16} />
                </button>
                {selected.asr_raw &&
                  selected.asr_raw.trim() &&
                  selected.asr_raw.trim() !== text && (
                    <button
                      type="button"
                      className="icon-btn"
                      disabled={busy}
                      onClick={() =>
                        void (async () => {
                          try {
                            await navigator.clipboard.writeText(selected.asr_raw!.trim());
                            setCopied(true);
                            setRetryNote("已复制识别原文（未整理）");
                            window.setTimeout(() => setCopied(false), 1600);
                          } catch (e) {
                            onError(String(e));
                          }
                        })()
                      }
                      title="复制识别原文（未整理）"
                      aria-label="复制原文"
                    >
                      <Icon name="clipboard" size={16} />
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
            {(selected.asr_raw || selected.corrected) && (
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
                      <div className="field-label">模型输出</div>
                      <pre className="field-value">{selected.asr_raw || "—"}</pre>
                    </div>
                    {selected.corrected && selected.corrected !== selected.asr_raw && (
                      <div>
                        <div className="field-label">修正后</div>
                        <pre className="field-value">{selected.corrected}</pre>
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
