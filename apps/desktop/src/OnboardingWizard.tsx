import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  api,
  type AsrModelStatus,
  type CorrectorProbeResult,
  type PermissionStatus,
} from "./api";
import { HotkeyRecorder } from "./HotkeyRecorder";
import { formatHotkeyLabel } from "./hotkeyFormat";
import { Icon } from "./Icons";
import type { AudioDevice } from "./types";

type Props = {
  onDone: () => void;
};

const STEPS = ["欢迎", "权限", "麦克风", "模型", "修正", "热键", "试听"] as const;
const PEAK_THRESHOLD = 0.04;

export function OnboardingWizard({ onDone }: Props) {
  const [step, setStep] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [perm, setPerm] = useState<PermissionStatus | null>(null);
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [device, setDevice] = useState<string>("");
  const [peak, setPeak] = useState(0);
  const [rms, setRms] = useState(0);
  const [heardVoice, setHeardVoice] = useState(false);
  const heardRef = useRef(false);
  const monitoring = useRef(false);

  const [asr, setAsr] = useState<AsrModelStatus | null>(null);
  const [dlMsg, setDlMsg] = useState("");
  const [dlPct, setDlPct] = useState<number | null>(null);
  const [customPath, setCustomPath] = useState("");

  const [probe, setProbe] = useState<CorrectorProbeResult | null>(null);
  const [pullMsg, setPullMsg] = useState("");
  const [corrModel, setCorrModel] = useState("qwen2.5:7b");

  const [hkEnabled, setHkEnabled] = useState(true);
  const [hkToggle, setHkToggle] = useState("Alt+Space");
  const [hkMode, setHkMode] = useState("hold");
  const [hkCapsule, setHkCapsule] = useState(true);
  const [hkWarn, setHkWarn] = useState<string[]>([]);

  const [practice, setPractice] = useState("");
  const [e2ePhase, setE2ePhase] = useState("idle");
  const [e2eOk, setE2eOk] = useState(false);
  const dismissing = useRef(false);

  const refreshPerm = useCallback(async () => {
    try {
      setPerm(await api.pollPermissions());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refreshAsr = useCallback(async () => {
    try {
      setAsr(await api.checkAsrModelStatus());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refreshProbe = useCallback(async () => {
    try {
      const p = await api.probeCorrector();
      setProbe(p);
      setCorrModel(p.suggestedModel);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const s = await api.getOnboardingState();
        setStep(Math.min(s.step, 6));
        const list = await api.listAudioDevices();
        setDevices(list);
        const def = list.find((d) => d.is_default) ?? list[0];
        if (def) setDevice(def.name);
        try {
          const hk = await api.getHotkeyConfig();
          setHkEnabled(hk.enabled);
          setHkToggle(hk.toggle);
          setHkMode(hk.mode);
          setHkCapsule(hk.showCapsule);
        } catch {
          /* ignore */
        }
      } catch (e) {
        setError(String(e));
      }
    })();
  }, []);

  useEffect(() => {
    if (step !== 1) return;
    void refreshPerm();
    const id = window.setInterval(() => void refreshPerm(), 1000);
    return () => window.clearInterval(id);
  }, [step, refreshPerm]);

  useEffect(() => {
    if (step !== 2) {
      if (monitoring.current) {
        monitoring.current = false;
        void api.stopVolumeMonitoring();
      }
      return;
    }
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void (async () => {
      try {
        setHeardVoice(false);
        heardRef.current = false;
        setPeak(0);
        setRms(0);
        if (device) await api.setAudioDevice(device);
        await api.startVolumeMonitoring(device || null);
        monitoring.current = true;
        unlisten = await listen<{ rms: number; peak: number }>("volume-level", (e) => {
          if (cancelled) return;
          setPeak(e.payload.peak);
          setRms(e.payload.rms);
          if (e.payload.peak >= PEAK_THRESHOLD && !heardRef.current) {
            heardRef.current = true;
            setHeardVoice(true);
          }
        });
      } catch (e) {
        setError(String(e));
      }
    })();
    return () => {
      cancelled = true;
      unlisten?.();
      if (monitoring.current) {
        monitoring.current = false;
        void api.stopVolumeMonitoring();
      }
    };
  }, [step, device]);

  useEffect(() => {
    if (step === 3) void refreshAsr();
    if (step === 4) void refreshProbe();
  }, [step, refreshAsr, refreshProbe]);

  useEffect(() => {
    if (step !== 3) return;
    let un: (() => void) | undefined;
    listen<{
      phase: string;
      message: string;
      percent?: number | null;
    }>("asr-download-progress", (e) => {
      setDlMsg(e.payload.message);
      setDlPct(e.payload.percent ?? null);
    }).then((fn) => {
      un = fn;
    });
    return () => un?.();
  }, [step]);

  useEffect(() => {
    if (step !== 4) return;
    let un: (() => void) | undefined;
    listen<{ phase: string; message: string }>("ollama-pull-progress", (e) => {
      setPullMsg(e.payload.message);
    }).then((fn) => {
      un = fn;
    });
    return () => un?.();
  }, [step]);

  useEffect(() => {
    if (step !== 5) return;
    void (async () => {
      try {
        const v = await api.validateHotkey(hkToggle);
        setHkWarn([...v.errors, ...v.warnings]);
      } catch {
        /* ignore */
      }
    })();
  }, [step, hkToggle]);

  useEffect(() => {
    if (step !== 6) return;
    let un: (() => void) | undefined;
    listen<{
      phase: string;
      message?: string;
      outcome?: { text?: string };
    }>("dictation", (e) => {
      const p = e.payload;
      setE2ePhase(p.phase);
      if (p.phase === "done" && p.outcome?.text) {
        setPractice((prev) => (prev ? prev + p.outcome!.text! : p.outcome!.text!));
        setE2eOk(true);
      }
      if (p.phase === "error") {
        setError(p.message || "试听失败");
      }
    }).then((fn) => {
      un = fn;
    });
    return () => un?.();
  }, [step]);

  async function goStep(next: number) {
    setError(null);
    setBusy(true);
    try {
      if (step === 2) {
        await api.stopVolumeMonitoring();
      }
      await api.setOnboardingStep(next);
      setStep(next);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function skipAll() {
    // Always allow mid-wizard exit (even during long downloads / probes).
    if (dismissing.current) return;
    dismissing.current = true;
    setBusy(true);
    try {
      try {
        await api.stopVolumeMonitoring();
      } catch {
        /* ignore if not monitoring */
      }
      await api.skipOnboarding();
      onDone();
    } catch (e) {
      dismissing.current = false;
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function finish() {
    setBusy(true);
    try {
      await api.stopVolumeMonitoring();
      await api.completeOnboarding(true);
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // Escape / anytime dismiss — standard wizard exit, not only step 0.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        void skipAll();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [busy]);

  const micOk = perm?.canRecord ?? false;
  const axOk = perm?.accessibilityTrusted ?? false;
  const canLeavePerms = micOk;
  const meterPct = Math.min(100, Math.round(Math.max(peak, rms * 2) * 200));
  const asrReady = asr?.sensevoiceReady ?? false;

  return (
    <div
      className="onboard-overlay"
      role="dialog"
      aria-modal="true"
      aria-label="首次设置"
      onMouseDown={(e) => {
        // Click scrim (not the card) → dismiss, same as 稍后再说.
        if (e.target === e.currentTarget) void skipAll();
      }}
    >
      <div
        className="onboard-card onboard-card-wide"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="onboard-topbar">
          <span className="onboard-topbar-title">首次设置 · {step + 1}/{STEPS.length}</span>
          <div className="onboard-topbar-actions">
            <button
              type="button"
              className="onboard-skip-btn"
              onClick={() => void skipAll()}
            >
              稍后再说
            </button>
            <button
              type="button"
              className="onboard-close-btn"
              aria-label="关闭设置"
              title="关闭（Esc，可稍后在侧栏继续）"
              onClick={() => void skipAll()}
            >
              ×
            </button>
          </div>
        </div>

        <div className="onboard-progress">
          {STEPS.map((label, i) => (
            <div
              key={label}
              className={`onboard-dot ${i === step ? "active" : ""} ${i < step ? "done" : ""}`}
              title={label}
            >
              <span className="onboard-dot-n">{i + 1}</span>
              <span className="onboard-dot-label">{label}</span>
            </div>
          ))}
        </div>

        {error && <div className="onboard-error">{error}</div>}

        {step === 0 && (
          <section className="onboard-step">
            <h1>欢迎使用 Lumen ASR</h1>
            <p className="muted-text">
              在本地把语音转成文字，并插入到你正在输入的应用光标处。随时可点右上角关闭，稍后从侧栏继续。
            </p>
            <ul className="onboard-feature-list">
              <li>
                <span className="onboard-feature-icon accent">
                  <Icon name="mic" size={14} />
                </span>
                本地转写（SenseVoice）
              </li>
              <li>
                <span className="onboard-feature-icon accent">
                  <Icon name="hotkey" size={14} />
                </span>
                按住热键说话，松手插入
              </li>
              <li>
                <span className="onboard-feature-icon warm">
                  <Icon name="sparkle-ai" size={14} />
                </span>
                可选 AI 修正（Ollama / OpenAI 兼容）
              </li>
            </ul>
            <div className="onboard-actions">
              <button type="button" className="btn" disabled={busy} onClick={() => void goStep(1)}>
                开始设置
              </button>
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void skipAll()}>
                稍后再说
              </button>
            </div>
          </section>
        )}

        {step === 1 && (
          <section className="onboard-step">
            <h1>核心权限</h1>
            <p className="muted-text">
              麦克风会弹系统对话框。辅助功能<strong>不会弹窗</strong>，必须在系统设置里打开
              <strong>当前这份进程</strong>。列表里若出现两个 Lumen，是不同签名/路径（开发版 vs
              .app，或多次 adhoc 编译），只开对应当前路径的那一项；开完后建议完全退出再开一次。
            </p>
            <div className="onboard-perm-grid">
              <div className={`onboard-perm-card ${micOk ? "ok" : ""}`}>
                <div className="onboard-perm-title">
                  麦克风 <span className="onboard-pill">{micOk ? "已就绪" : "需要授权"}</span>
                </div>
                <div className="actions">
                  <button
                    type="button"
                    className="btn"
                    disabled={busy}
                    onClick={() =>
                      void (async () => {
                        setBusy(true);
                        try {
                          setPerm(await api.requestMicrophoneAccess());
                        } catch (e) {
                          setError(String(e));
                        } finally {
                          setBusy(false);
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
                    打开设置
                  </button>
                </div>
              </div>
              <div className={`onboard-perm-card ${axOk ? "ok" : ""}`}>
                <div className="onboard-perm-title">
                  辅助功能 <span className="onboard-pill">{axOk ? "已开启" : "需要开启"}</span>
                </div>
                {perm && (
                  <dl className="onboard-path-meta">
                    <dt>系统列表名称</dt>
                    <dd>
                      <code>{perm.settingsListName || perm.processHint}</code>
                    </dd>
                    <dt>可执行文件</dt>
                    <dd>
                      <code>{perm.processHint}</code>
                    </dd>
                    <dt>路径</dt>
                    <dd className="onboard-path">
                      <code>{perm.processPath}</code>
                    </dd>
                    {perm.codesignKind ? (
                      <>
                        <dt>签名</dt>
                        <dd>
                          <code>{perm.codesignKind}</code>
                          {perm.codesignAdhoc ? " · 重新编译后常需重开开关" : ""}
                        </dd>
                      </>
                    ) : null}
                  </dl>
                )}
                <div className="actions">
                  <button
                    type="button"
                    className="btn"
                    disabled={busy}
                    onClick={() =>
                      void (async () => {
                        setBusy(true);
                        try {
                          setPerm(await api.requestAccessibilityAccess());
                        } catch (e) {
                          setError(String(e));
                        } finally {
                          setBusy(false);
                        }
                      })()
                    }
                  >
                    打开辅助功能设置
                  </button>
                  <button type="button" className="btn ghost" disabled={busy} onClick={() => void refreshPerm()}>
                    刷新
                  </button>
                </div>
              </div>
            </div>
            <div className="onboard-actions">
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(0)}>
                上一步
              </button>
              <button
                type="button"
                className="btn"
                disabled={busy || !canLeavePerms}
                onClick={() => void goStep(2)}
              >
                {axOk ? "下一步" : "继续（稍后补辅助功能）"}
              </button>
            </div>
          </section>
        )}

        {step === 2 && (
          <section className="onboard-step">
            <h1>选择麦克风</h1>
            <p className="muted-text">选择输入设备，然后说一句话。</p>
            <div className="form-row" style={{ marginBottom: 16 }}>
              <label className="muted-text" style={{ minWidth: 72 }}>
                设备
              </label>
              <select
                className="input"
                value={device}
                disabled={busy}
                onChange={(e) => setDevice(e.target.value)}
              >
                {devices.map((d) => (
                  <option key={d.name} value={d.name}>
                    {d.name}
                    {d.is_default ? "（默认）" : ""}
                  </option>
                ))}
              </select>
            </div>
            <div className="onboard-meter-wrap">
              <div className="onboard-meter-label">
                {heardVoice ? "已检测到声音 ✓" : "请对着麦克风说一句话…"}
              </div>
              <div className="onboard-meter" aria-hidden>
                <div
                  className={`onboard-meter-fill ${heardVoice ? "ok" : ""}`}
                  style={{ width: `${meterPct}%` }}
                />
              </div>
            </div>
            <div className="onboard-actions">
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(1)}>
                上一步
              </button>
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(3)}>
                跳过
              </button>
              <button
                type="button"
                className="btn"
                disabled={busy || !heardVoice}
                onClick={() => void goStep(3)}
              >
                下一步
              </button>
            </div>
          </section>
        )}

        {step === 3 && (
          <section className="onboard-step">
            <h1>本地 ASR 模型</h1>
            <p className="muted-text">默认 SenseVoice。可使用本机已有模型，或下载官方 sherpa 包。</p>
            {asr && (
              <div className={`onboard-perm-card ${asrReady ? "ok" : ""}`} style={{ marginBottom: 12 }}>
                <div className="onboard-perm-title">
                  SenseVoice{" "}
                  <span className="onboard-pill">{asrReady ? "就绪" : "未就绪"}</span>
                </div>
                <p className="muted-text" style={{ wordBreak: "break-all" }}>
                  <code>{asr.sensevoiceDir}</code>
                </p>
              </div>
            )}
            {asr && asr.candidates.filter((c) => c.ready && c.engine === "sensevoice").length > 0 && (
              <div style={{ marginBottom: 12 }}>
                <div className="muted-text" style={{ marginBottom: 6 }}>
                  检测到的本地模型
                </div>
                {asr.candidates
                  .filter((c) => c.ready && c.engine === "sensevoice")
                  .map((c) => (
                    <div key={c.path} className="onboard-candidate">
                      <span>{c.label}</span>
                      <button
                        type="button"
                        className="btn ghost"
                        disabled={busy}
                        onClick={() =>
                          void (async () => {
                            setBusy(true);
                            try {
                              setAsr(await api.useExistingAsrModel(c.path, "sensevoice"));
                            } catch (e) {
                              setError(String(e));
                            } finally {
                              setBusy(false);
                            }
                          })()
                        }
                      >
                        使用
                      </button>
                    </div>
                  ))}
              </div>
            )}
            <div className="form-row" style={{ marginBottom: 10 }}>
              <input
                className="input"
                style={{ flex: 1 }}
                placeholder="或粘贴本地模型目录路径…"
                value={customPath}
                disabled={busy}
                onChange={(e) => setCustomPath(e.target.value)}
              />
              <button
                type="button"
                className="btn ghost"
                disabled={busy || !customPath.trim()}
                onClick={() =>
                  void (async () => {
                    setBusy(true);
                    try {
                      setAsr(await api.useExistingAsrModel(customPath.trim(), "sensevoice"));
                    } catch (e) {
                      setError(String(e));
                    } finally {
                      setBusy(false);
                    }
                  })()
                }
              >
                验证并使用
              </button>
            </div>
            <div className="actions" style={{ marginBottom: 8 }}>
              <button
                type="button"
                className="btn"
                disabled={busy || asrReady}
                onClick={() =>
                  void (async () => {
                    setBusy(true);
                    setError(null);
                    setDlMsg("开始下载…");
                    try {
                      setAsr(await api.startAsrModelDownload());
                      setDlMsg("完成");
                    } catch (e) {
                      setError(String(e));
                    } finally {
                      setBusy(false);
                    }
                  })()
                }
              >
                {asrReady ? "已就绪" : "下载 SenseVoice"}
              </button>
              <button
                type="button"
                className="btn ghost"
                disabled={!busy}
                onClick={() => void api.cancelAsrModelDownload()}
              >
                取消下载
              </button>
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void refreshAsr()}>
                刷新
              </button>
            </div>
            {(dlMsg || dlPct != null) && (
              <p className="muted-text">
                {dlMsg}
                {dlPct != null ? ` · ${dlPct.toFixed(0)}%` : ""}
              </p>
            )}
            <div className="onboard-actions">
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(2)}>
                上一步
              </button>
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(4)}>
                跳过（稍后配置）
              </button>
              <button
                type="button"
                className="btn"
                disabled={busy || !asrReady}
                onClick={() => void goStep(4)}
              >
                下一步
              </button>
            </div>
          </section>
        )}

        {step === 4 && (
          <section className="onboard-step">
            <h1>AI 修正（可选）</h1>
            <p className="muted-text">
              优先 Ollama 本地模型；也可使用环境变量中的 OpenAI 兼容接口。
            </p>
            {probe && (
              <div className={`onboard-perm-card ${probe.ollamaRunning ? "ok" : ""}`}>
                <div className="onboard-perm-title">
                  检测结果 <span className="onboard-pill">{probe.suggestedProvider}</span>
                </div>
                <p className="muted-text">{probe.message}</p>
                {probe.ollamaRunning && (
                  <p className="muted-text">
                    模型：{probe.ollamaModels.slice(0, 8).join(", ") || "（空）"}
                    {probe.ollamaModels.length > 8 ? "…" : ""}
                  </p>
                )}
                {probe.envOpenaiBase && (
                  <p className="muted-text">
                    Env base: <code>{probe.envOpenaiBase}</code>
                    {probe.envOpenaiKeySet ? " · key 已设置" : " · 无 key"}
                  </p>
                )}
              </div>
            )}
            {probe?.ollamaRunning && (
              <div className="form-row" style={{ margin: "12px 0" }}>
                <label className="muted-text" style={{ minWidth: 72 }}>
                  模型
                </label>
                <select
                  className="input"
                  value={corrModel}
                  disabled={busy}
                  onChange={(e) => setCorrModel(e.target.value)}
                >
                  {!probe.ollamaModels.includes(corrModel) && (
                    <option value={corrModel}>{corrModel}</option>
                  )}
                  {probe.ollamaModels.map((m) => (
                    <option key={m} value={m}>
                      {m}
                    </option>
                  ))}
                  {!probe.hasQwen257b && <option value="qwen2.5:7b">qwen2.5:7b（需拉取）</option>}
                </select>
              </div>
            )}
            {pullMsg && <p className="muted-text">{pullMsg}</p>}
            <div className="actions" style={{ marginTop: 10 }}>
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void refreshProbe()}>
                重新检测
              </button>
              {probe?.ollamaRunning && (
                <button
                  type="button"
                  className="btn"
                  disabled={busy}
                  onClick={() =>
                    void (async () => {
                      setBusy(true);
                      try {
                        await api.applyCorrectorSuggestion({
                          provider: "ollama",
                          baseUrl: probe.suggestedBaseUrl,
                          model: corrModel,
                          enabled: true,
                        });
                      } catch (e) {
                        setError(String(e));
                      } finally {
                        setBusy(false);
                      }
                    })()
                  }
                >
                  使用 Ollama
                </button>
              )}
              {probe?.ollamaRunning && !probe.hasQwen257b && (
                <button
                  type="button"
                  className="btn ghost"
                  disabled={busy}
                  onClick={() =>
                    void (async () => {
                      setBusy(true);
                      setPullMsg("拉取中…");
                      try {
                        const p = await api.ollamaPullModel("qwen2.5:7b");
                        setProbe(p);
                        setCorrModel("qwen2.5:7b");
                        await api.applyCorrectorSuggestion({
                          provider: "ollama",
                          baseUrl: p.suggestedBaseUrl,
                          model: "qwen2.5:7b",
                          enabled: true,
                        });
                      } catch (e) {
                        setError(String(e));
                      } finally {
                        setBusy(false);
                      }
                    })()
                  }
                >
                  拉取 qwen2.5:7b
                </button>
              )}
              {probe?.envOpenaiBase && (
                <button
                  type="button"
                  className="btn"
                  disabled={busy}
                  onClick={() =>
                    void (async () => {
                      setBusy(true);
                      try {
                        await api.applyCorrectorSuggestion({
                          provider: "openai_compatible",
                          baseUrl: probe.envOpenaiBase!,
                          model: probe.envLumenModel || probe.suggestedModel,
                          enabled: true,
                        });
                      } catch (e) {
                        setError(String(e));
                      } finally {
                        setBusy(false);
                      }
                    })()
                  }
                >
                  使用 Env OpenAI 兼容
                </button>
              )}
              <button
                type="button"
                className="btn ghost"
                disabled={busy}
                onClick={() =>
                  void (async () => {
                    setBusy(true);
                    try {
                      await api.applyCorrectorSuggestion({
                        provider: "none",
                        baseUrl: "http://127.0.0.1:11434/v1",
                        model: "none",
                        enabled: false,
                      });
                    } catch (e) {
                      setError(String(e));
                    } finally {
                      setBusy(false);
                    }
                  })()
                }
              >
                跳过修正
              </button>
            </div>
            <div className="onboard-actions">
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(3)}>
                上一步
              </button>
              <button type="button" className="btn" disabled={busy} onClick={() => void goStep(5)}>
                下一步
              </button>
            </div>
          </section>
        )}

        {step === 5 && (
          <section className="onboard-step">
            <h1>全局热键</h1>
            <p className="muted-text">
              默认按住说话。避免 ⌘Space（Spotlight）。当前：{" "}
              <span className="kbd">{formatHotkeyLabel(hkToggle)}</span>
            </p>
            {hkWarn.length > 0 && (
              <ul className="onboard-bullets" style={{ color: "var(--muted)" }}>
                {hkWarn.map((w) => (
                  <li key={w}>{w}</li>
                ))}
              </ul>
            )}
            <HotkeyRecorder
              enabled={hkEnabled}
              toggle={hkToggle}
              showCapsule={hkCapsule}
              mode={hkMode}
              busy={busy}
              onBusy={setBusy}
              onError={setError}
              onChange={(next) => {
                setHkEnabled(next.enabled);
                setHkToggle(next.toggle);
                setHkCapsule(next.showCapsule);
                setHkMode(next.mode);
              }}
              onSaved={() => {
                /* keep wizard */
              }}
            />
            <div className="onboard-actions">
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(4)}>
                上一步
              </button>
              <button type="button" className="btn" disabled={busy} onClick={() => void goStep(6)}>
                下一步 · 试听
              </button>
            </div>
          </section>
        )}

        {step === 6 && (
          <section className="onboard-step">
            <h1>端到端试听</h1>
            <p className="muted-text">
              按住 <span className="kbd">{formatHotkeyLabel(hkToggle)}</span>{" "}
              说话，松手等待结果出现在下方（也可点按钮）。
            </p>
            <textarea
              className="input onboard-practice"
              rows={5}
              value={practice}
              onChange={(e) => setPractice(e.target.value)}
              placeholder="转写结果会出现在这里…"
            />
            <p className="muted-text">
              状态：{e2ePhase}
              {e2eOk ? " · 已收到结果 ✓" : ""}
            </p>
            <div className="actions">
              <button
                type="button"
                className="btn"
                disabled={busy}
                onClick={() =>
                  void (async () => {
                    setBusy(true);
                    setError(null);
                    try {
                      await api.startRecording();
                      setE2ePhase("listening");
                    } catch (e) {
                      setError(String(e));
                    } finally {
                      setBusy(false);
                    }
                  })()
                }
              >
                开始录音
              </button>
              <button
                type="button"
                className="btn"
                disabled={busy}
                onClick={() =>
                  void (async () => {
                    setBusy(true);
                    try {
                      const out = await api.stopAndTranscribe(true);
                      setPractice((p) => (p ? p + out.text : out.text));
                      setE2eOk(true);
                      setE2ePhase("done");
                    } catch (e) {
                      setError(String(e));
                    } finally {
                      setBusy(false);
                    }
                  })()
                }
              >
                停止并转写
              </button>
            </div>
            <div className="onboard-actions">
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(5)}>
                上一步
              </button>
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void finish()}>
                跳过完成
              </button>
              <button type="button" className="btn" disabled={busy} onClick={() => void finish()}>
                {e2eOk ? "完成设置" : "完成设置"}
              </button>
            </div>
          </section>
        )}
      </div>
    </div>
  );
}
