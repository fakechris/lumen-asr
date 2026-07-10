import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "./api";
import {
  absorbKeyDown,
  chordToShortcut,
  emptyChord,
  formatChordLive,
  formatHotkeyLabel,
  HOTKEY_PRESETS,
  isValidChord,
  type ChordState,
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
 * Click → press the whole combination naturally → release to confirm.
 * Supports modifier+key (⌥Space) and modifier-only (⌥⇧).
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
  const [live, setLive] = useState<ChordState>(emptyChord());
  const chordRef = useRef<ChordState>(emptyChord());
  const downCodesRef = useRef<Set<string>>(new Set());
  const committedRef = useRef(false);

  const stopRecording = useCallback(async (resume: boolean) => {
    setRecording(false);
    setLive(emptyChord());
    chordRef.current = emptyChord();
    downCodesRef.current.clear();
    committedRef.current = false;
    if (resume) {
      try {
        await api.resumeHotkeys();
      } catch {
        /* ignore */
      }
    }
  }, []);

  const applyShortcut = useCallback(
    async (shortcut: string) => {
      // During an active capture gesture, ignore double-commit from keydown+keyup.
      if (committedRef.current) return;
      committedRef.current = true;
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
        setLive(emptyChord());
        chordRef.current = emptyChord();
        downCodesRef.current.clear();
        setHint(`已设置为 ${formatHotkeyLabel(shortcut)}`);
        onSaved();
        // Keep committed=true until next startRecording so residual keyup is ignored.
      } catch (e) {
        onError(String(e));
        committedRef.current = false;
        try {
          await api.resumeHotkeys();
        } catch {
          /* ignore */
        }
        setRecording(false);
        setLive(emptyChord());
      } finally {
        onBusy(false);
      }
    },
    [enabled, showCapsule, onBusy, onError, onChange, onSaved]
  );

  const startRecording = useCallback(async () => {
    onError(null);
    committedRef.current = false;
    chordRef.current = emptyChord();
    downCodesRef.current.clear();
    setLive(emptyChord());
    setHint("按下你要用的快捷键，松开后生效 · Esc 取消");
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
      if (e.repeat) return;

      if (e.key === "Escape") {
        void stopRecording(true);
        setHint("已取消");
        return;
      }

      downCodesRef.current.add(e.code);
      const next = absorbKeyDown(chordRef.current, e);
      chordRef.current = next;
      setLive(next);
      setHint(`按住中：${formatChordLive(next)}  · 松开确认`);

      // With a main key (e.g. Space), commit as soon as the full combo is down —
      // same moment the user "presses the shortcut".
      if (next.key && isValidChord(next)) {
        const sc = chordToShortcut(next);
        if (sc) void applyShortcut(sc);
      }
    };

    const onKeyUp = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      downCodesRef.current.delete(e.code);

      // When all keys released: if we only had modifiers (⌥⇧), commit now.
      if (downCodesRef.current.size === 0) {
        const chord = chordRef.current;
        if (!chord.key && isValidChord(chord)) {
          const sc = chordToShortcut(chord);
          if (sc) {
            void applyShortcut(sc);
            return;
          }
        }
        // Incomplete gesture (e.g. only Alt) — reset and keep listening.
        if (!committedRef.current) {
          chordRef.current = emptyChord();
          setLive(emptyChord());
          setHint("按下你要用的快捷键，松开后生效 · Esc 取消");
        }
      }
    };

    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("keyup", onKeyUp, true);
    return () => {
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("keyup", onKeyUp, true);
    };
  }, [recording, applyShortcut, stopRecording]);

  useEffect(() => {
    return () => {
      if (recording) {
        void api.resumeHotkeys().catch(() => undefined);
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recording]);

  const chipLabel = recording
    ? live.alt || live.shift || live.control || live.command || live.key
      ? formatChordLive(live)
      : "按下快捷键…"
    : formatHotkeyLabel(toggle);

  return (
    <section className="card settings-section">
      <h2>全局热键</h2>
      <p className="muted-text">
        在任意 App 中切换录音/停止。点「录制」后，
        <strong>像平时使用一样一次按好组合键</strong>
        （支持 ⌥Space，也支持仅修饰键如 ⌥⇧）。
        默认 <span className="kbd">⌥Space</span>，避开 Spotlight 的{" "}
        <span className="kbd">⌘Space</span>。
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
              setHint("已取消");
            } else {
              void startRecording();
            }
          }}
        >
          {chipLabel}
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
            onClick={() => {
              committedRef.current = false;
              void applyShortcut(p.value);
            }}
          >
            {p.label}
          </button>
        ))}
      </div>

      <p className="muted-text" style={{ marginTop: 12, fontSize: "0.85rem" }}>
        仅修饰键（如 ⌥⇧）在 macOS 上通过系统修饰键状态监听，无需再按字母键。
        裸 <code>Fn</code> / 右 ⌘ 按住说话仍需后续底层钩子。
      </p>
    </section>
  );
}
