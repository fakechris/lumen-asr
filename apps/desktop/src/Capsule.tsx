import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

type Phase = "idle" | "listening" | "processing" | "error";

type DictationEvent =
  | { phase: "idle" }
  | {
      phase: "listening";
      message: string;
      intent?: string;
      targetLanguage?: string | null;
    }
  | {
      phase: "processing";
      message: string;
      intent?: string;
      targetLanguage?: string | null;
    }
  | { phase: "done"; outcome: { text: string } }
  | { phase: "error"; message: string }
  | { phase: "cancelled" };

export default function Capsule() {
  const [phase, setPhase] = useState<Phase>("idle");
  const [message, setMessage] = useState("就绪");
  const [seconds, setSeconds] = useState(0);
  const [intent, setIntent] = useState("default");
  const [targetLang, setTargetLang] = useState<string | null>(null);

  useEffect(() => {
    let un: (() => void) | undefined;
    listen<DictationEvent>("dictation", (e) => {
      const p = e.payload;
      if (p.phase === "listening") {
        setPhase("listening");
        setMessage(p.message || "录音中");
        setIntent(p.intent || "default");
        setTargetLang(p.targetLanguage ?? null);
        setSeconds(0);
      } else if (p.phase === "processing") {
        setPhase("processing");
        setMessage(p.message || "处理中");
        setIntent(p.intent || intent);
        setTargetLang(p.targetLanguage ?? targetLang);
      } else if (p.phase === "error") {
        setPhase("error");
        setMessage(p.message);
      } else if (p.phase === "done") {
        setPhase("idle");
        setMessage("完成");
        setIntent("default");
        setTargetLang(null);
      } else {
        setPhase("idle");
        setMessage("就绪");
        setIntent("default");
        setTargetLang(null);
      }
    }).then((fn) => {
      un = fn;
    });
    return () => {
      un?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
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

  const isTranslate = intent === "translate";
  const label =
    phase === "listening"
      ? isTranslate
        ? `翻译→${targetLang || "en"} ${seconds}s`
        : intent === "raw"
          ? `原文 ${seconds}s`
          : `录音 ${seconds}s`
      : phase === "processing"
        ? message
        : message;

  return (
    <div
      className={`capsule-root phase-${phase}${isTranslate ? " intent-translate" : ""}${
        intent === "raw" ? " intent-raw" : ""
      }`}
    >
      <span className="capsule-dot" />
      <span className="capsule-text">{label}</span>
      {phase === "listening" && (
        <button type="button" className="capsule-btn" onClick={() => void stop()}>
          停止
        </button>
      )}
    </div>
  );
}
