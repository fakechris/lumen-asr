import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

type Phase = "idle" | "listening" | "processing" | "error";

type DictationEvent =
  | { phase: "idle" }
  | { phase: "listening"; message: string }
  | { phase: "processing"; message: string }
  | { phase: "done"; outcome: { text: string } }
  | { phase: "error"; message: string }
  | { phase: "cancelled" };

export default function Capsule() {
  const [phase, setPhase] = useState<Phase>("idle");
  const [message, setMessage] = useState("就绪");
  const [seconds, setSeconds] = useState(0);

  useEffect(() => {
    let un: (() => void) | undefined;
    listen<DictationEvent>("dictation", (e) => {
      const p = e.payload;
      if (p.phase === "listening") {
        setPhase("listening");
        setMessage(p.message || "录音中");
        setSeconds(0);
      } else if (p.phase === "processing") {
        setPhase("processing");
        setMessage(p.message || "处理中");
      } else if (p.phase === "error") {
        setPhase("error");
        setMessage(p.message);
      } else if (p.phase === "done") {
        setPhase("idle");
        setMessage("完成");
      } else {
        setPhase("idle");
        setMessage("就绪");
      }
    }).then((fn) => {
      un = fn;
    });
    return () => {
      un?.();
    };
  }, []);

  useEffect(() => {
    if (phase !== "listening") return;
    const t = setInterval(() => setSeconds((s) => s + 1), 1000);
    return () => clearInterval(t);
  }, [phase]);

  async function stop() {
    try {
      await invoke("toggle_dictation_cmd");
    } catch (e) {
      setPhase("error");
      setMessage(String(e));
    }
  }

  return (
    <div className={`capsule-root phase-${phase}`}>
      <span className="capsule-dot" />
      <span className="capsule-text">
        {phase === "listening"
          ? `录音 ${seconds}s`
          : phase === "processing"
            ? "处理中…"
            : message}
      </span>
      {phase === "listening" && (
        <button type="button" className="capsule-btn" onClick={() => void stop()}>
          停止
        </button>
      )}
    </div>
  );
}
