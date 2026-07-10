import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, type PermissionStatus } from "./api";
import type { AudioDevice } from "./types";

type Props = {
  onDone: () => void;
};

const STEPS = ["欢迎", "权限", "麦克风"] as const;
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

  const refreshPerm = useCallback(async () => {
    try {
      setPerm(await api.pollPermissions());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  // Load step from backend + devices
  useEffect(() => {
    void (async () => {
      try {
        const s = await api.getOnboardingState();
        setStep(Math.min(s.step, 2));
        const list = await api.listAudioDevices();
        setDevices(list);
        const def = list.find((d) => d.is_default) ?? list[0];
        if (def) setDevice(def.name);
      } catch (e) {
        setError(String(e));
      }
    })();
  }, []);

  // Poll permissions on step 1
  useEffect(() => {
    if (step !== 1) return;
    void refreshPerm();
    const id = window.setInterval(() => void refreshPerm(), 1000);
    return () => window.clearInterval(id);
  }, [step, refreshPerm]);

  // Volume monitoring on step 2
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
        if (device) {
          await api.setAudioDevice(device);
        }
        await api.startVolumeMonitoring(device || null);
        monitoring.current = true;
        unlisten = await listen<{ rms: number; peak: number; device: string }>(
          "volume-level",
          (e) => {
            if (cancelled) return;
            setPeak(e.payload.peak);
            setRms(e.payload.rms);
            if (e.payload.peak >= PEAK_THRESHOLD && !heardRef.current) {
              heardRef.current = true;
              setHeardVoice(true);
            }
          }
        );
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

  async function goStep(next: number) {
    setError(null);
    setBusy(true);
    try {
      await api.setOnboardingStep(next);
      setStep(next);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function skipAll() {
    setBusy(true);
    try {
      await api.skipOnboarding();
      onDone();
    } catch (e) {
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

  const micOk = perm?.canRecord ?? false;
  const axOk = perm?.accessibilityTrusted ?? false;
  // Stage B: can proceed with clipboard-only if user accepts; soft-gate AX.
  const canLeavePerms = micOk;

  const meterPct = Math.min(100, Math.round(Math.max(peak, rms * 2) * 200));

  return (
    <div className="onboard-overlay" role="dialog" aria-modal="true" aria-label="首次设置">
      <div className="onboard-card">
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
              在本地把语音转成文字，并插入到你正在输入的应用光标处。
            </p>
            <ul className="onboard-bullets">
              <li>本地转写（SenseVoice / Whisper）</li>
              <li>按住热键说话，松手插入</li>
              <li>可选 AI 修正与词典学习</li>
            </ul>
            <div className="onboard-actions">
              <button
                type="button"
                className="btn"
                disabled={busy}
                onClick={() => void goStep(1)}
              >
                开始设置
              </button>
              <button
                type="button"
                className="btn ghost"
                disabled={busy}
                onClick={() => void skipAll()}
              >
                稍后再说
              </button>
            </div>
          </section>
        )}

        {step === 1 && (
          <section className="onboard-step">
            <h1>核心权限</h1>
            <p className="muted-text">
              麦克风用于录音。辅助功能用于把文字插入其他 App，并启用可靠的全局热键。
              辅助功能<strong>不会</strong>出现系统内授权弹窗，需在系统设置中手动打开。
            </p>

            <div className="onboard-perm-grid">
              <div className={`onboard-perm-card ${micOk ? "ok" : ""}`}>
                <div className="onboard-perm-title">
                  麦克风{" "}
                  <span className="onboard-pill">{micOk ? "已就绪" : "需要授权"}</span>
                </div>
                <p className="muted-text">第一次请求会弹出系统对话框。</p>
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
                    打开麦克风设置
                  </button>
                </div>
              </div>

              <div className={`onboard-perm-card ${axOk ? "ok" : ""}`}>
                <div className="onboard-perm-title">
                  辅助功能{" "}
                  <span className="onboard-pill">{axOk ? "已开启" : "需要开启"}</span>
                </div>
                <p className="muted-text">
                  请在列表中打开<strong>当前正在运行</strong>的这一项（开发版与正式包是两条不同记录）：
                </p>
                {perm && (
                  <dl className="onboard-path-meta">
                    <dt>名称</dt>
                    <dd>
                      <code>{perm.processHint}</code>
                    </dd>
                    <dt>路径</dt>
                    <dd className="onboard-path">
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
                  <button
                    type="button"
                    className="btn ghost"
                    disabled={busy}
                    onClick={() => void refreshPerm()}
                  >
                    刷新状态
                  </button>
                </div>
                {!axOk && (
                  <p className="muted-text onboard-hint">
                    打开开关后本页会自动变绿。若仍失败：关掉再打开该开关，或确认启用的是上面路径对应的条目。
                    也可先继续（仅剪贴板模式）。
                  </p>
                )}
              </div>
            </div>

            <div className="onboard-actions">
              <button
                type="button"
                className="btn ghost"
                disabled={busy}
                onClick={() => void goStep(0)}
              >
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
            <p className="muted-text">选择输入设备，然后说一句话，确认音量条会动。</p>

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
              <div className="muted-text onboard-meter-nums">
                peak {peak.toFixed(3)} · rms {rms.toFixed(3)}
              </div>
            </div>

            <div className="onboard-actions">
              <button
                type="button"
                className="btn ghost"
                disabled={busy}
                onClick={() => void goStep(1)}
              >
                上一步
              </button>
              <button
                type="button"
                className="btn ghost"
                disabled={busy}
                onClick={() => void finish()}
              >
                跳过音量检测
              </button>
              <button
                type="button"
                className="btn"
                disabled={busy || !heardVoice}
                onClick={() => void finish()}
              >
                完成设置
              </button>
            </div>
            <p className="muted-text onboard-hint">
              后续步骤（ASR 模型、AI 修正、热键 E2E）将在下一版向导中加入；你可随时在设置中重新打开引导。
            </p>
          </section>
        )}
      </div>
    </div>
  );
}

