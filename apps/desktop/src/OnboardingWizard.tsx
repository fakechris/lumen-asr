import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  api,
  type PermissionStatus,
} from "./api";
import { HotkeyRecorder } from "./HotkeyRecorder";
import { formatHotkeyLabel } from "./hotkeyFormat";
import { Icon } from "./Icons";
import { chooseAudioDevice } from "./audioDeviceSelection";
import {
  useMeetingModels,
  type MeetingModels,
  type ModelTarget,
} from "./meetingModels";
import type { AudioDevice } from "./types";

type Props = {
  onDone: () => void;
};

const STEPS = ["欢迎", "权限", "热键", "就绪"] as const;
const LAST_STEP = STEPS.length - 1;
const PEAK_THRESHOLD = 0.04;
const IS_WINDOWS = navigator.userAgent.includes("Windows");

const MODEL_LABELS: Record<ModelTarget, string> = {
  sensevoice: "SenseVoice（听写）",
  offline: "Paraformer 离线（会议）",
  streaming: "Paraformer 流式（会议）",
};

export function OnboardingWizard({ onDone }: Props) {
  const [step, setStep] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [guide, setGuide] = useState<string | null>(null);
  const [doneFlash, setDoneFlash] = useState(false);
  const [perm, setPerm] = useState<PermissionStatus | null>(null);
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [device, setDevice] = useState<string>("");
  const [peak, setPeak] = useState(0);
  const [rms, setRms] = useState(0);
  const [heardVoice, setHeardVoice] = useState(false);
  const heardRef = useRef(false);
  const monitoring = useRef(false);

  const models = useMeetingModels();
  const autoQueuedRef = useRef(false);

  const [hkEnabled, setHkEnabled] = useState(true);
  const [hkToggle, setHkToggle] = useState(IS_WINDOWS ? "Ctrl+Shift+Space" : "Alt+Space");
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

  useEffect(() => {
    void (async () => {
      try {
        const s = await api.getOnboardingState();
        setStep(Math.min(s.step, LAST_STEP));
        const [list, preferred] = await Promise.all([
          api.listAudioDevices(),
          api.getAudioDevice(),
        ]);
        setDevices(list);
        setDevice(chooseAudioDevice(list, preferred));
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
    const id = window.setInterval(() => void refreshPerm(), 800);
    return () => window.clearInterval(id);
  }, [step, refreshPerm]);

  const micOk = perm?.canRecord ?? false;

  useEffect(() => {
    const shouldListen = step === 1 && micOk;
    if (!shouldListen) {
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
  }, [step, device, micOk]);

  useEffect(() => {
    if (models.status) {
      void api.setAsrEngine("sensevoice").catch(() => undefined);
    }
  }, [models.status]);

  useEffect(() => {
    const s = models.status;
    if (!s || autoQueuedRef.current) return;
    autoQueuedRef.current = true;
    const dictationReady =
      s.sensevoiceReady ||
      (s.activeEngine === "qwen" && s.qwenReady && s.qwenRuntimeSupported) ||
      (s.activeEngine === "whisper" && s.whisperReady);
    if (!dictationReady) models.enqueue(["sensevoice"]);
  }, [models]);

  useEffect(() => {
    if (step !== 2) return;
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
    if (step !== 3) return;
    void refreshPerm();
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
  }, [step, refreshPerm]);

  async function goStep(next: number) {
    setError(null);
    setGuide(null);
    setBusy(true);
    try {
      if (step === 1) {
        await api.stopVolumeMonitoring().catch(() => undefined);
      }
      const clamped = Math.max(0, Math.min(LAST_STEP, next));
      await api.setOnboardingStep(clamped);
      setStep(clamped);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function skipAll() {
    if (dismissing.current) return;
    dismissing.current = true;
    setBusy(true);
    try {
      try {
        await api.stopVolumeMonitoring();
      } catch {
        /* ignore if not monitoring */
      }
      void api.dismissAccessibilityDragOverlay().catch(() => undefined);
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
    if (dismissing.current) return;
    dismissing.current = true;
    setBusy(true);
    setDoneFlash(true);
    try {
      await api.stopVolumeMonitoring().catch(() => undefined);
      void api.dismissAccessibilityDragOverlay().catch(() => undefined);
      await api.completeOnboarding(true);
      window.setTimeout(() => onDone(), 420);
    } catch (e) {
      dismissing.current = false;
      setDoneFlash(false);
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

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

  const axOk = perm?.accessibilityTrusted ?? false;
  const canLeavePerms = micOk;
  const meterPct = Math.min(100, Math.round(Math.max(peak, rms * 2) * 200));
  const asrReady = models.status?.sensevoiceReady ?? false;

  return (
    <div
      className="onboard-overlay"
      role="dialog"
      aria-modal="true"
      aria-label="首次设置"
    >
      <div className={`onboard-card onboard-card-wide ${step === 0 || step === LAST_STEP ? "wide" : ""}`}>
        <div className="onboard-topbar">
          <span className="onboard-topbar-title">
            首次设置 · {step + 1}/{STEPS.length} · 大约一分钟
          </span>
          <div className="onboard-topbar-actions">
            <DownloadBadge models={models} />
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
            <div key={label} className="onboard-progress-item">
              {i > 0 && (
                <span
                  className={`onboard-line ${i < step ? "done" : i === step ? "current" : "pending"}`}
                  aria-hidden
                />
              )}
              <div
                className={`onboard-dot ${i === step ? "active" : ""} ${i < step ? "done" : ""}`}
                title={label}
              >
                <span className="onboard-dot-n">{i + 1}</span>
                <span className="onboard-dot-label">{label}</span>
              </div>
            </div>
          ))}
        </div>

        {guide && (
          <div className="onboard-notice warn" role="status">
            <span className="onboard-notice-title">操作指引</span>
            <span>{guide}</span>
          </div>
        )}
        {error && (
          <div className="onboard-notice error" role="alert">
            <span className="onboard-notice-title">出错了</span>
            <span>{error}</span>
          </div>
        )}

        <div className="onboard-step-body" key={step}>
        {step === 0 && (
          <section className="onboard-step">
            <div className="onboard-cols">
              <div className="onboard-demo">
                <div className="demo-kicker">演示 · 按住说话</div>
                <div className="demo-raw">「呃，明天下午，那个，三点跟设计组对一下初稿」</div>
                <div className="demo-arrow">↓ 松手后自动整理</div>
                <div className="demo-clean">明天下午三点跟设计组对一下初稿。</div>
                <p className="muted-text demo-note">
                  语气词会被修掉，文字直接进你正在写的输入框。
                </p>
              </div>
              <div>
                <h1>按住热键说话</h1>
                <p className="muted-text">
                  文字会出现在你正在写的地方。听写模型会在后台下载，你先完成权限即可。
                </p>
                <ul className="onboard-feature-list">
                  <li>
                    <span className="onboard-feature-icon accent">
                      <Icon name="mic" size={14} />
                    </span>
                    本地转写，不经过云端
                  </li>
                  <li>
                    <span className="onboard-feature-icon accent">
                      <Icon name="insert" size={14} />
                    </span>
                    松手后插入当前输入框
                  </li>
                  <li>
                    <span className="onboard-feature-icon warm">
                      <Icon name="sparkle-ai" size={14} />
                    </span>
                    AI 修正可稍后在设置里打开
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
              </div>
            </div>
          </section>
        )}

        {step === 1 && (
          <section className="onboard-step">
            <h1>需要两件事</h1>
            <p className="muted-text">
              {IS_WINDOWS
                ? "点卡片授权麦克风。转写完成后会写入当前窗口；失败时复制到剪贴板。"
                : "点卡片授权。麦克风会弹系统对话框；辅助功能打开系统设置后，把图标拖进列表并打开开关。"}
            </p>
            <div className="onboard-perm-grid">
              <div className={`onboard-perm-card ${micOk ? "ok" : "actionable"}`}>
                <div className="onboard-perm-title">
                  麦克风{" "}
                  <span className="onboard-pill">{micOk ? "已就绪" : "点一下授权"}</span>
                </div>
                <p className="muted-text onboard-perm-why">听写需要它听到你说话。</p>
                <div className="actions">
                  <button
                    type="button"
                    className="btn"
                    disabled={busy}
                    onClick={() =>
                      void (async () => {
                        setBusy(true);
                        setGuide(null);
                        setError(null);
                        try {
                          const p = await api.requestMicrophoneAccess();
                          setPerm(p);
                          if (!p.canRecord) {
                            setGuide(
                              IS_WINDOWS
                                ? "在系统弹窗里选「允许」；如果之前拒绝过，去「设置 → 隐私和安全性 → 麦克风」打开开关，这里会自动检测到。"
                                : "在系统弹窗里点「允许」；如果之前拒绝过，去「系统设置 → 隐私与安全性 → 麦克风」打开 Lumen 的开关，这里会自动检测到。",
                            );
                          }
                        } catch (e) {
                          setError(String(e));
                        } finally {
                          setBusy(false);
                        }
                      })()
                    }
                  >
                    {micOk ? "已授权" : "允许麦克风"}
                  </button>
                </div>
                {micOk && (
                  <div className="onboard-meter-wrap">
                    <div className="onboard-meter-label">
                      {heardVoice ? "已听到声音" : "对着电脑说一句，确认有声"}
                    </div>
                    <div className="onboard-meter" aria-hidden>
                      <div
                        className={`onboard-meter-fill ${heardVoice ? "ok" : ""}`}
                        style={{ width: `${meterPct}%` }}
                      />
                    </div>
                    {devices.length > 1 && (
                      <select
                        className="input"
                        style={{ marginTop: 10 }}
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
                    )}
                  </div>
                )}
              </div>

              {IS_WINDOWS ? (
                <div className="onboard-perm-card ok">
                  <div className="onboard-perm-title">
                    写入当前窗口 <span className="onboard-pill">自动插入</span>
                  </div>
                  <p className="muted-text onboard-perm-why">
                    用键盘/粘贴写入光标处。注入失败会复制到剪贴板，并在胶囊说明原因。不要以管理员身份运行目标程序。
                  </p>
                </div>
              ) : (
                <div className={`onboard-perm-card ${axOk ? "ok" : "actionable"}`}>
                  <div className="onboard-perm-title">
                    辅助功能{" "}
                    <span className="onboard-pill">{axOk ? "已开启" : "拖入系统设置"}</span>
                  </div>
                  <p className="muted-text onboard-perm-why">
                    {axOk
                      ? "可以直接把文字插入其它应用。"
                      : "点下面按钮会打开系统设置，并把 Lumen 图标浮在窗口下方。把它拖进列表，打开开关即可——不用点 + 去找应用。"}
                  </p>
                  <div className="actions">
                    <button
                      type="button"
                      className="btn"
                      disabled={busy}
                      onClick={() =>
                        void (async () => {
                          setBusy(true);
                          setGuide(null);
                          setError(null);
                          try {
                            const p = await api.requestAccessibilityAccess();
                            setPerm(p);
                            if (!p.accessibilityTrusted) {
                              setGuide(
                                "把浮层里的 Lumen 图标拖进「辅助功能」列表并打开开关；成功后这里会自动变绿。",
                              );
                            }
                          } catch (e) {
                            setError(String(e));
                          } finally {
                            setBusy(false);
                          }
                        })()
                      }
                    >
                      {axOk ? "已开启" : "打开并拖入"}
                    </button>
                  </div>
                  {perm && !axOk && (
                    <details className="onboard-tech-details">
                      <summary>列表里找不到？</summary>
                      <p className="muted-text">
                        把浮层里的图标拖进「辅助功能」列表。若仍无效，只打开名称是{" "}
                        <code>{perm.settingsListName || perm.processHint}</code>{" "}
                        的那一项。
                      </p>
                    </details>
                  )}
                </div>
              )}
            </div>
            <div className="mock-settings">
              <div className="ms-head">
                <Icon name="settings" size={14} />
                {IS_WINDOWS
                  ? "设置 → 隐私和安全性 → 麦克风"
                  : "系统设置 → 隐私与安全性 → 麦克风 / 辅助功能"}
              </div>
              <div className="ms-row highlight">
                <span>麦克风 · Lumen</span>
                <span className={`mock-toggle ${micOk ? "on" : ""}`} />
              </div>
              {!IS_WINDOWS && (
                <div className="ms-row highlight">
                  <span>辅助功能 · Lumen</span>
                  <span className={`mock-toggle ${axOk ? "on" : ""}`} />
                </div>
              )}
              <div className="ms-row">
                <span>其它应用…</span>
                <span className="muted-text">在系统设置中管理</span>
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
                {IS_WINDOWS ? "下一步" : axOk ? "下一步" : "继续（稍后补辅助功能）"}
              </button>
            </div>
          </section>
        )}

        {step === 2 && (
          <section className="onboard-step">
            <h1>按住说话</h1>
            <p className="muted-text">
              默认按住{" "}
              <span className="kbd">{formatHotkeyLabel(hkToggle)}</span>{" "}
              录音，松手插入。大多数人不用改。
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
              <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(1)}>
                上一步
              </button>
              <button type="button" className="btn" disabled={busy} onClick={() => void goStep(3)}>
                下一步
              </button>
            </div>
          </section>
        )}

        {step === 3 && (
          <section className="onboard-step">
            <div className="onboard-cols">
              <div className="onboard-demo">
                <div className="demo-kicker">状态总览</div>
                <div className="onboard-sum-list">
                  <span className={`onboard-sum-pill ${micOk ? "ok" : "warn"}`}>
                    麦克风权限 {micOk ? "已授权" : "未授权"}
                  </span>
                  {IS_WINDOWS ? (
                    <span className="onboard-sum-pill ok">写入方式 自动插入</span>
                  ) : (
                    <span className={`onboard-sum-pill ${axOk ? "ok" : "warn"}`}>
                      辅助功能 {axOk ? "已开启" : "未开启（可稍后补）"}
                    </span>
                  )}
                  <span className={`onboard-sum-pill ${hkEnabled ? "ok" : "warn"}`}>
                    热键 {hkEnabled ? formatHotkeyLabel(hkToggle) : "已关闭"}
                  </span>
                  <span className={`onboard-sum-pill ${asrReady ? "ok" : "warn"}`}>
                    听写模型 {asrReady ? "已就绪" : "后台下载中"}
                  </span>
                </div>
              </div>
              <div>
                <h1>可以开始了</h1>
                <p className="muted-text">
                  到任意输入框，按住{" "}
                  <span className="kbd">{formatHotkeyLabel(hkToggle)}</span>{" "}
                  说话，松手等文字出现。也可以在这里试一次。
                </p>
                {!asrReady && (
                  <p className="muted-text">
                    听写模型还在后台下载，完成后即可使用。可先结束设置。
                  </p>
                )}
                <textarea
                  className="input onboard-practice"
                  rows={4}
                  value={practice}
                  onChange={(e) => setPractice(e.target.value)}
                  placeholder="转写结果会出现在这里…"
                />
                <p className="muted-text">
                  {e2eOk ? "已收到结果" : e2ePhase === "idle" ? "可选：按住热键，或点按钮试听" : `状态：${e2ePhase}`}
                </p>
                <div className="actions">
                  <button
                    type="button"
                    className="btn ghost"
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
                    className="btn ghost"
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
                {doneFlash && (
                  <div className="onboard-success">
                    <Icon name="check" size={16} /> 设置完成，正在进入…
                  </div>
                )}
                <div className="onboard-actions">
                  <button type="button" className="btn ghost" disabled={busy} onClick={() => void goStep(2)}>
                    上一步
                  </button>
                  <button type="button" className="btn" disabled={busy || doneFlash} onClick={() => void finish()}>
                    开始使用
                  </button>
                </div>
              </div>
            </div>
          </section>
        )}
        </div>
      </div>
    </div>
  );
}

function DownloadBadge({ models }: { models: MeetingModels }) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement | null>(null);
  const { active, queued, progress, error, failed, cancelled } = models;
  const pct = progress?.percent ?? null;
  const downloading = active !== null;
  const show = downloading || error !== null || (cancelled && failed.length > 0);
  useEffect(() => {
    if (!show) setOpen(false);
  }, [show]);
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        setOpen(false);
      }
    };
    const onDown = (e: MouseEvent) => {
      if (
        rootRef.current &&
        e.target instanceof Node &&
        !rootRef.current.contains(e.target)
      ) {
        setOpen(false);
      }
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("mousedown", onDown);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("mousedown", onDown);
    };
  }, [open]);
  if (!show) return null;
  const label = downloading
    ? `模型下载中${pct != null ? ` ${pct.toFixed(0)}%` : "…"}`
    : error
      ? "模型下载失败"
      : "模型下载已取消";
  return (
    <div className="onboard-dl" ref={rootRef}>
      <button
        type="button"
        className={`onboard-dl-chip ${error ? "err" : ""}`}
        onClick={() => setOpen((v) => !v)}
        title="模型在后台下载，可继续其它步骤"
        aria-expanded={open}
      >
        {downloading && <span className="onboard-dl-spin" aria-hidden />}
        {label}
      </button>
      {open && (
        <div className="onboard-dl-pop" role="status">
          {downloading && active && (
            <>
              <div className="onboard-dl-row">
                <span>{MODEL_LABELS[active]}</span>
                <span className="muted-text">
                  {pct != null ? `${pct.toFixed(0)}%` : "下载中"}
                </span>
              </div>
              <div className="onboard-dl-bar" aria-hidden>
                <div
                  className="onboard-dl-fill"
                  style={{ width: `${Math.min(100, Math.max(2, pct ?? 2))}%` }}
                />
              </div>
              {progress?.message && (
                <p className="muted-text onboard-dl-msg">{progress.message}</p>
              )}
              {queued.map((t) => (
                <div key={t} className="onboard-dl-row muted-text">
                  <span>{MODEL_LABELS[t]}</span>
                  <span>排队中</span>
                </div>
              ))}
              <div className="actions">
                <button
                  type="button"
                  className="btn ghost small"
                  onClick={() => void models.cancel()}
                >
                  取消下载
                </button>
              </div>
            </>
          )}
          {!downloading && error && (
            <>
              <p className="muted-text onboard-dl-msg">{error}</p>
              <div className="actions">
                <button
                  type="button"
                  className="btn small"
                  onClick={() => models.retry()}
                >
                  重试
                </button>
              </div>
            </>
          )}
          {!downloading && !error && cancelled && failed.length > 0 && (
            <>
              <p className="muted-text onboard-dl-msg">
                已取消，未完成的模型可稍后在设置或会议页安装。
              </p>
              <div className="actions">
                <button
                  type="button"
                  className="btn ghost small"
                  onClick={() => models.retry()}
                >
                  重新下载
                </button>
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
