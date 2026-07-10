import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type Health = {
  app: string;
  version: string;
  data_dir: string;
  db_ok: boolean;
};

export default function App() {
  const [health, setHealth] = useState<Health | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<Health>("app_health")
      .then(setHealth)
      .catch((e) => setError(String(e)));
  }, []);

  return (
    <div className="shell">
      <header>
        <h1>Lumen ASR</h1>
        <p className="tagline">Local-first voice dictation · macOS</p>
      </header>

      <main>
        <section className="card">
          <h2>Scaffold</h2>
          <p>
            Product workspace is up. Session state machine, dictionary learning,
            paste-first inject ports, and SQLite store are in{" "}
            <code>crates/</code>.
          </p>
          {error && <p className="error">{error}</p>}
          {health && (
            <dl className="meta">
              <dt>App</dt>
              <dd>
                {health.app} v{health.version}
              </dd>
              <dt>Data dir</dt>
              <dd>
                <code>{health.data_dir}</code>
              </dd>
              <dt>Database</dt>
              <dd>{health.db_ok ? "ready" : "failed"}</dd>
            </dl>
          )}
        </section>

        <section className="card muted">
          <h2>Roadmap</h2>
          <ol>
            <li>M1 — store + dictionary IPC</li>
            <li>M2 — SenseVoice (sherpa) + mic capture</li>
            <li>M3 — Ollama / OpenAI-compatible corrector</li>
            <li>M4 — paste-first inject + permissions</li>
            <li>M5 — hotkey + floating capsule UI</li>
            <li>M6 — edit learning UX</li>
          </ol>
        </section>
      </main>
    </div>
  );
}
