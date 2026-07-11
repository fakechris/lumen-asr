import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "./api";
import {
  absorbKeyDown,
  chordToShortcut,
  emptyChord,
  formatChordLive,
  formatHotkeyLabel,
  isValidChord,
  type ChordState,
} from "./hotkeyFormat";

type Props = {
  value: string;
  disabled?: boolean;
  busy?: boolean;
  onBusy?: (b: boolean) => void;
  onError?: (e: string | null) => void;
  onChange: (shortcut: string) => void;
};

/**
 * Compact click-to-capture chip for secondary chords (intent hotkeys).
 * Pauses global hotkeys while listening so the chord is not fired.
 */
export function ChordCaptureChip({
  value,
  disabled,
  busy,
  onBusy,
  onError,
  onChange,
}: Props) {
  const [recording, setRecording] = useState(false);
  const [live, setLive] = useState<ChordState>(emptyChord());
  const chordRef = useRef<ChordState>(emptyChord());
  const downCodesRef = useRef<Set<string>>(new Set());
  const committedRef = useRef(false);

  const stop = useCallback(async (resume: boolean) => {
    setRecording(false);
    setLive(emptyChord());
    chordRef.current = emptyChord();
    downCodesRef.current.clear();
    if (resume) {
      try {
        await api.resumeHotkeys();
      } catch {
        /* ignore */
      }
    }
  }, []);

  const commit = useCallback(
    async (shortcut: string) => {
      if (committedRef.current) return;
      committedRef.current = true;
      onBusy?.(true);
      onError?.(null);
      try {
        onChange(shortcut);
        setRecording(false);
        setLive(emptyChord());
        chordRef.current = emptyChord();
        downCodesRef.current.clear();
        await api.resumeHotkeys();
      } catch (e) {
        onError?.(String(e));
        committedRef.current = false;
        try {
          await api.resumeHotkeys();
        } catch {
          /* ignore */
        }
        setRecording(false);
      } finally {
        onBusy?.(false);
      }
    },
    [onBusy, onError, onChange]
  );

  const start = useCallback(async () => {
    if (disabled || busy) return;
    onError?.(null);
    committedRef.current = false;
    chordRef.current = emptyChord();
    downCodesRef.current.clear();
    setLive(emptyChord());
    setRecording(true);
    try {
      await api.pauseHotkeys();
    } catch (e) {
      onError?.(String(e));
      setRecording(false);
    }
  }, [disabled, busy, onError]);

  useEffect(() => {
    if (!recording) return;
    const onKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.repeat) return;
      if (e.key === "Escape") {
        void stop(true);
        return;
      }
      downCodesRef.current.add(e.code);
      const next = absorbKeyDown(chordRef.current, e);
      chordRef.current = next;
      setLive(next);
      if (next.key && isValidChord(next)) {
        const sc = chordToShortcut(next);
        if (sc) void commit(sc);
      }
    };
    const onKeyUp = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      downCodesRef.current.delete(e.code);
      if (downCodesRef.current.size === 0) {
        const chord = chordRef.current;
        if (!chord.key && isValidChord(chord)) {
          const sc = chordToShortcut(chord);
          if (sc) {
            void commit(sc);
            return;
          }
        }
        if (!committedRef.current) {
          chordRef.current = emptyChord();
          setLive(emptyChord());
        }
      }
    };
    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("keyup", onKeyUp, true);
    return () => {
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("keyup", onKeyUp, true);
    };
  }, [recording, commit, stop]);

  useEffect(() => {
    return () => {
      if (recording) void api.resumeHotkeys().catch(() => undefined);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recording]);

  const label = recording
    ? live.alt || live.shift || live.control || live.command || live.key
      ? formatChordLive(live)
      : "按下快捷键…"
    : value
      ? formatHotkeyLabel(value)
      : "点击录制";

  return (
    <button
      type="button"
      className={`hotkey-chip intent-chord-chip ${recording ? "recording" : ""}`}
      disabled={disabled || busy}
      onClick={() => void (recording ? stop(true) : start())}
      title={recording ? "Esc 取消" : "点击后按下新快捷键"}
    >
      {label}
    </button>
  );
}
