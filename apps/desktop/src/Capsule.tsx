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
        // Prefer payload intent; keep previous if missing so translate stays visible.
        if (p.intent) setIntent(p.intent);
        if (p.targetLanguage != null) setTargetLang(p.targetLanguage);
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
  const isRaw = intent === "raw";
  const lang = targetLang || "en";

  // Strong visual copy — never use the same “录音” string for translate.
  let label: string;
  let badge: string;
  if (phase === "listening") {
    if (isTranslate) {
      badge = "译";
      label = `翻译 → ${lang} · ${seconds}s`;
    } else if (isRaw) {
      badge = "原";
      label = `仅原文 · ${seconds}s`;
    } else {
      badge = "录";
      label = `录音 · ${seconds}s`;
    }
  } else if (phase === "processing") {
    if (isTranslate) {
      badge = "译";
      label = message.includes("翻译") ? message : `正在翻译 → ${lang}…`;
    } else if (isRaw) {
      badge = "原";
      label = "转写中（不整理）…";
    } else {
      badge = "整";
      label = message.includes("修正") ? message : "转写与整理中…";
    }
  } else if (phase === "error") {
    badge = "!";
    label = message;
  } else {
    badge = "·";
    label = message;
  }

  return (
    <div
      className={[
        "capsule-root",
        `phase-${phase}`,
        isTranslate ? "intent-translate" : "",
        isRaw ? "intent-raw" : "",
      ]
        .filter(Boolean)
        .join(" ")}
    >
      <span className="capsule-badge" aria-hidden>
        {badge}
      </span>
      <span className="capsule-text">{label}</span>
      {phase === "listening" && (
        <button type="button" className="capsule-btn" onClick={() => void stop()}>
          停止
        </button>
      )}
    </div>
  );
}
