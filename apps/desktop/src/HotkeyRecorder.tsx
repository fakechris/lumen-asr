import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import {
  eventToShortcut,
  formatHotkeyLabel,
  HOTKEY_PRESETS,
} from "./hotkeyFormat";

type Props = {
  enabled: boolean;
  toggle: string;
  showCapsule: boolean;
  busy: boolean;
  onBusy: (b: boolean) => void;
  onError: (e: string | null) => void;
  onChange: (next: {
    enabled: boolean;
    toggle: string;
    showCapsule: boolean;
  }) => void;
  onSaved: () => void;
};

/**
 * Competitor-style hotkey setup: click → press keys → save.
 * Never asks the user to type "CommandOrControl+…".
 */
export function HotkeyRecorder({
  enabled,
  toggle,
  showCapsule,
  busy,
  onBusy,
  onError,
  onChange,
  onSaved,
}: Props) {
  const [recording, setRecording] = useState(false);
  const [hint, setHint] = useState<string | null>(null);

  const stopRecording = useCallback(
    async (resume: boolean) => {
      setRecording(false);
      setHint(null);
      if (resume) {
        try {
          await api.resumeHotkeys();
        } catch {
          /* ignore */
        }
      }
    },
    []
  );

  const applyShortcut = useCallback(
    async (shortcut: string) => {
      onBusy(true);
      onError(null);
      try {
        await api.saveHotkeyConfig({
          enabled,
          toggle: shortcut,
          showCapsule,
        });
        onChange({ enabled, toggle: shortcut, showCapsule });
        setRecording(false);
        setHint(`已设置为 ${formatHotkeyLabel(shortcut)}`);
        onSaved();
      } catch (e) {
        onError(String(e));
        // Re-bind previous on failure
        try {
          await api.resumeHotkeys();
        } catch {
          /* ignore */
        }
        setRecording(false);
      } finally {
        onBusy(false);
      }
    },
    [enabled, showCapsule, onBusy, onError, onChange, onSaved]
  );

  const startRecording = useCallback(async () => {
    onError(null);
    setHint("请按下新的组合键… Esc 取消");
    setRecording(true);
    try {
      await api.pauseHotkeys();
    } catch (e) {
      onError(String(e));
      setRecording(false);
      setHint(null);
    }
  }, [onError]);

  useEffect(() => {
    if (!recording) return;

    const onKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();

      if (e.key === "Escape") {
        void stopRecording(true);
        setHint("已取消");
        return;
      }

      const sc = eventToShortcut(e);
      if (!sc) {
        setHint("请按住修饰键（⌥ ⌃ ⌘ ⇧）再按主键，或按 F 键");
        return;
      }
      void applyShortcut(sc);
    };

    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [recording, applyShortcut, stopRecording]);

  // If user navigates away mid-record, resume hotkeys
  useEffect(() => {
    return () => {
      if (recording) {
        void api.resumeHotkeys().catch(() => undefined);
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recording]);

  return (
    <section className="card settings-section">
      <h2>全局热键</h2>
      <p className="muted-text">
        在任意 App 中切换录音/停止。与竞品一致：点击后直接按下组合键，无需手输字符串。
        默认 <span className="kbd">⌥Space</span>（避开 Spotlight 的{" "}
        <span className="kbd">⌘Space</span>）。
      </p>

      <div className="form-row" style={{ marginBottom: 12 }}>
        <label className="muted-text">
          <input
            type="checkbox"
            checked={enabled}
            disabled={busy || recording}
            onChange={(e) => {
              const v = e.target.checked;
              onChange({ enabled: v, toggle, showCapsule });
              void (async () => {
                onBusy(true);
                try {
                  await api.saveHotkeyConfig({
                    enabled: v,
                    toggle,
                    showCapsule,
                  });
                  onSaved();
                } catch (err) {
                  onError(String(err));
                } finally {
                  onBusy(false);
                }
              })();
            }}
          />{" "}
          启用热键
        </label>
      </div>

      <div className="form-row" style={{ marginBottom: 12 }}>
        <label className="muted-text">
          <input
            type="checkbox"
            checked={showCapsule}
            disabled={busy || recording}
            onChange={(e) => {
              const v = e.target.checked;
              onChange({ enabled, toggle, showCapsule: v });
              void (async () => {
                onBusy(true);
                try {
                  await api.saveHotkeyConfig({
                    enabled,
                    toggle,
                    showCapsule: v,
                  });
                  onSaved();
                } catch (err) {
                  onError(String(err));
                } finally {
                  onBusy(false);
                }
              })();
            }}
          />{" "}
          显示浮动胶囊
        </label>
      </div>

      <div className="hotkey-row">
        <button
          type="button"
          className={`hotkey-chip ${recording ? "recording" : ""}`}
          disabled={busy}
          onClick={() => {
            if (recording) {
              void stopRecording(true);
            } else {
              void startRecording();
            }
          }}
        >
          {recording ? "按下组合键…" : formatHotkeyLabel(toggle)}
        </button>
        <button
          type="button"
          className={`btn ${recording ? "ghost" : ""}`}
          disabled={busy}
          onClick={() => {
            if (recording) {
              void stopRecording(true);
              setHint("已取消");
            } else {
              void startRecording();
            }
          }}
        >
          {recording ? "取消" : "点击录制"}
        </button>
      </div>

      {hint && (
        <p
          className={`hotkey-hint ${recording ? "recording" : ""}`}
          role="status"
        >
          {hint}
        </p>
      )}

      <div className="preset-row">
        <span className="muted-text" style={{ fontSize: 12 }}>
          常用
        </span>
        {HOTKEY_PRESETS.map((p) => (
          <button
            key={p.value}
            type="button"
            className={`preset-chip ${toggle === p.value ? "active" : ""}`}
            disabled={busy || recording}
            onClick={() => void applyShortcut(p.value)}
          >
            {p.label}
          </button>
        ))}
      </div>

      <p className="muted-text" style={{ marginTop: 12, fontSize: "0.85rem" }}>
        若录制失败：组合键可能被系统或其他软件占用。裸 <code>Fn</code>{" "}
        / 右 ⌘ 按住说话需更底层钩子，后续版本支持。
      </p>
    </section>
  );
}
