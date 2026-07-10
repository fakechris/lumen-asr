import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import type {
  DictionaryEntry,
  EditEvent,
  Health,
  LearnCandidate,
  SessionRecord,
  TabId,
} from "./types";

function previewText(s?: string | null, n = 72): string {
  if (!s) return "—";
  const t = s.replace(/\s+/g, " ").trim();
  return t.length > n ? t.slice(0, n) + "…" : t;
}

function formatTime(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

export default function App() {
  const [tab, setTab] = useState<TabId>("overview");
  const [health, setHealth] = useState<Health | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [sessions, setSessions] = useState<SessionRecord[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [edits, setEdits] = useState<EditEvent[]>([]);

  const [dict, setDict] = useState<DictionaryEntry[]>([]);
  const [termInput, setTermInput] = useState("");
  const [fromInput, setFromInput] = useState("");
  const [toInput, setToInput] = useState("");

  const [learnBefore, setLearnBefore] = useState("");
  const [learnAfter, setLearnAfter] = useState("");
  const [candidates, setCandidates] = useState<LearnCandidate[]>([]);

  const refreshHealth = useCallback(async () => {
    try {
      setHealth(await api.health());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refreshSessions = useCallback(async () => {
    try {
      setSessions(await api.listSessions(100));
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refreshDict = useCallback(async () => {
    try {
      setDict(await api.listDictionary());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void refreshHealth();
  }, [refreshHealth]);

  useEffect(() => {
    if (tab === "history" || tab === "overview") void refreshSessions();
    if (tab === "dictionary" || tab === "overview" || tab === "learn")
      void refreshDict();
  }, [tab, refreshSessions, refreshDict]);

  useEffect(() => {
    if (!selectedId) {
      setEdits([]);
      return;
    }
    api
      .listEditEvents(selectedId)
      .then(setEdits)
      .catch((e) => setError(String(e)));
  }, [selectedId]);

  const selected = sessions.find((s) => s.id === selectedId) ?? null;

  async function run(label: string, fn: () => Promise<void>) {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(`${label}: ${String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="shell">
      <header>
        <div className="header-row">
          <div>
            <h1>Lumen ASR</h1>
            <p className="tagline">Local-first voice dictation · macOS</p>
          </div>
          {health && (
            <div className="badge-row">
              <span className={`badge ${health.db_ok ? "ok" : "bad"}`}>
                DB {health.db_ok ? "ok" : "down"}
              </span>
              <span className="badge">v{health.version}</span>
            </div>
          )}
        </div>
        <nav className="tabs">
          {(
            [
              ["overview", "概览"],
              ["history", "历史"],
              ["dictionary", "词典"],
              ["learn", "编辑学习"],
            ] as const
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              className={`tab ${tab === id ? "active" : ""}`}
              onClick={() => setTab(id)}
            >
              {label}
            </button>
          ))}
        </nav>
      </header>

      {error && (
        <div className="banner error" role="alert">
          {error}
          <button type="button" className="linkish" onClick={() => setError(null)}>
            关闭
          </button>
        </div>
      )}

      <main>
        {tab === "overview" && (
          <Overview
            health={health}
            sessions={sessions}
            dictCount={dict.length}
            busy={busy}
            onSeed={() =>
              run("seed", async () => {
                await api.seedDemoSession();
                await refreshSessions();
                await refreshHealth();
              })
            }
            onGoto={(t) => setTab(t)}
          />
        )}

        {tab === "history" && (
          <HistoryPanel
            sessions={sessions}
            selected={selected}
            edits={edits}
            busy={busy}
            onSelect={setSelectedId}
            onSeed={() =>
              run("seed", async () => {
                const s = await api.seedDemoSession();
                await refreshSessions();
                setSelectedId(s.id);
                await refreshHealth();
              })
            }
            onDelete={(id) =>
              run("delete session", async () => {
                await api.deleteSession(id);
                if (selectedId === id) setSelectedId(null);
                await refreshSessions();
                await refreshHealth();
              })
            }
            onRecordEdit={(sessionId, before, after) =>
              run("record edit", async () => {
                await api.recordEditEvent({
                  sessionId,
                  beforeText: before,
                  afterText: after,
                  source: "pre_insert_ui",
                });
                setEdits(await api.listEditEvents(sessionId));
              })
            }
          />
        )}

        {tab === "dictionary" && (
          <DictionaryPanel
            entries={dict}
            termInput={termInput}
            fromInput={fromInput}
            toInput={toInput}
            busy={busy}
            onTermInput={setTermInput}
            onFromInput={setFromInput}
            onToInput={setToInput}
            onAddTerm={() =>
              run("add term", async () => {
                await api.addTerm(termInput);
                setTermInput("");
                await refreshDict();
                await refreshHealth();
              })
            }
            onAddReplacement={() =>
              run("add replacement", async () => {
                await api.addReplacement(fromInput, toInput);
                setFromInput("");
                setToInput("");
                await refreshDict();
                await refreshHealth();
              })
            }
            onDelete={(id) =>
              run("delete entry", async () => {
                await api.deleteDictionaryEntry(id);
                await refreshDict();
                await refreshHealth();
              })
            }
          />
        )}

        {tab === "learn" && (
          <LearnPanel
            before={learnBefore}
            after={learnAfter}
            candidates={candidates}
            busy={busy}
            onBefore={setLearnBefore}
            onAfter={setLearnAfter}
            onSuggest={() =>
              run("suggest", async () => {
                setCandidates(await api.suggestFromEdit(learnBefore, learnAfter));
              })
            }
            onConfirm={(c) =>
              run("confirm learn", async () => {
                await api.confirmLearn({
                  kind: c.kind,
                  term: c.term ?? undefined,
                  fromText: c.from_text ?? undefined,
                  toText: c.to_text ?? undefined,
                  beforeText: learnBefore,
                  afterText: learnAfter,
                });
                await refreshDict();
                await refreshHealth();
              })
            }
          />
        )}
      </main>
    </div>
  );
}

function Overview({
  health,
  sessions,
  dictCount,
  busy,
  onSeed,
  onGoto,
}: {
  health: Health | null;
  sessions: SessionRecord[];
  dictCount: number;
  busy: boolean;
  onSeed: () => void;
  onGoto: (t: TabId) => void;
}) {
  return (
    <>
      <section className="card">
        <h2>状态</h2>
        {health ? (
          <dl className="meta">
            <dt>数据目录</dt>
            <dd>
              <code>{health.data_dir}</code>
            </dd>
            <dt>数据库</dt>
            <dd>
              <code>{health.db_path}</code>
            </dd>
            <dt>会话</dt>
            <dd>{health.session_count}</dd>
            <dt>词典条目</dt>
            <dd>{health.dictionary_count}</dd>
          </dl>
        ) : (
          <p className="muted-text">加载中…</p>
        )}
        <div className="actions">
          <button type="button" className="btn" disabled={busy} onClick={onSeed}>
            写入示例会话
          </button>
          <button type="button" className="btn ghost" onClick={() => onGoto("history")}>
            查看历史
          </button>
          <button type="button" className="btn ghost" onClick={() => onGoto("dictionary")}>
            管理词典
          </button>
        </div>
      </section>

      <section className="card">
        <h2>最近会话</h2>
        {sessions.length === 0 ? (
          <p className="muted-text">暂无历史。可写入示例会话，或等 M2 录音管线接入。</p>
        ) : (
          <ul className="list">
            {sessions.slice(0, 5).map((s) => (
              <li key={s.id}>
                <span className="list-time">{formatTime(s.created_at)}</span>
                <span>{previewText(s.pasted || s.corrected || s.asr_raw)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="card muted">
        <h2>里程碑</h2>
        <ol>
          <li className="done">M0 — 架构骨架</li>
          <li className="done">M1 — Store / 词典 IPC + 本页 UI</li>
          <li>M2 — SenseVoice (sherpa) + 麦克风</li>
          <li>M3 — Ollama 修正</li>
          <li>M4 — paste-first 注入 + 权限</li>
          <li>M5 — 热键 + 胶囊</li>
          <li>M6 — 粘贴后编辑捕获</li>
        </ol>
        <p className="muted-text">词典条目数：{dictCount}</p>
      </section>
    </>
  );
}

function HistoryPanel({
  sessions,
  selected,
  edits,
  busy,
  onSelect,
  onSeed,
  onDelete,
  onRecordEdit,
}: {
  sessions: SessionRecord[];
  selected: SessionRecord | null;
  edits: EditEvent[];
  busy: boolean;
  onSelect: (id: string | null) => void;
  onSeed: () => void;
  onDelete: (id: string) => void;
  onRecordEdit: (sessionId: string, before: string, after: string) => void;
}) {
  const [editAfter, setEditAfter] = useState("");

  useEffect(() => {
    setEditAfter(selected?.pasted || selected?.corrected || selected?.asr_raw || "");
  }, [selected?.id, selected?.pasted, selected?.corrected, selected?.asr_raw]);

  return (
    <div className="split">
      <section className="card list-pane">
        <div className="card-head">
          <h2>历史</h2>
          <button type="button" className="btn small" disabled={busy} onClick={onSeed}>
            + 示例
          </button>
        </div>
        {sessions.length === 0 ? (
          <p className="muted-text">空</p>
        ) : (
          <ul className="session-list">
            {sessions.map((s) => (
              <li key={s.id}>
                <button
                  type="button"
                  className={`session-item ${selected?.id === s.id ? "active" : ""}`}
                  onClick={() => onSelect(s.id)}
                >
                  <span className="list-time">{formatTime(s.created_at)}</span>
                  <span className="session-preview">
                    {previewText(s.pasted || s.corrected || s.asr_raw, 48)}
                  </span>
                  <span className="chip">{s.status}</span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="card detail-pane">
        {!selected ? (
          <p className="muted-text">选择一条会话查看详情</p>
        ) : (
          <>
            <div className="card-head">
              <h2>详情</h2>
              <button
                type="button"
                className="btn small danger"
                disabled={busy}
                onClick={() => onDelete(selected.id)}
              >
                删除
              </button>
            </div>
            <dl className="meta">
              <dt>时间</dt>
              <dd>{formatTime(selected.created_at)}</dd>
              <dt>App</dt>
              <dd>{selected.focus?.app_name || "—"}</dd>
              <dt>ASR</dt>
              <dd>{selected.asr_engine || "—"}</dd>
              <dt>插入</dt>
              <dd>{selected.insert_strategy}</dd>
            </dl>
            <Field label="ASR 原文" value={selected.asr_raw} />
            <Field label="修正后" value={selected.corrected} />
            <Field label="最终粘贴" value={selected.pasted} />

            <h3 className="subhead">模拟预览编辑</h3>
            <p className="muted-text">
              改字后保存为 edit_event（M6 会从系统焦点框自动捕获）。
            </p>
            <textarea
              className="textarea"
              rows={4}
              value={editAfter}
              onChange={(e) => setEditAfter(e.target.value)}
            />
            <div className="actions">
              <button
                type="button"
                className="btn"
                disabled={busy}
                onClick={() => {
                  const before =
                    selected.corrected || selected.asr_raw || selected.pasted || "";
                  onRecordEdit(selected.id, before, editAfter);
                }}
              >
                记录编辑
              </button>
            </div>

            <h3 className="subhead">编辑事件 ({edits.length})</h3>
            {edits.length === 0 ? (
              <p className="muted-text">无</p>
            ) : (
              <ul className="list">
                {edits.map((e) => (
                  <li key={e.id} className="edit-item">
                    <span className="chip">{e.source}</span>
                    <div>
                      <div className="diff-before">{e.before_text}</div>
                      <div className="diff-after">{e.after_text}</div>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </>
        )}
      </section>
    </div>
  );
}

function Field({ label, value }: { label: string; value?: string | null }) {
  return (
    <div className="field-block">
      <div className="field-label">{label}</div>
      <pre className="field-value">{value || "—"}</pre>
    </div>
  );
}

function DictionaryPanel({
  entries,
  termInput,
  fromInput,
  toInput,
  busy,
  onTermInput,
  onFromInput,
  onToInput,
  onAddTerm,
  onAddReplacement,
  onDelete,
}: {
  entries: DictionaryEntry[];
  termInput: string;
  fromInput: string;
  toInput: string;
  busy: boolean;
  onTermInput: (v: string) => void;
  onFromInput: (v: string) => void;
  onToInput: (v: string) => void;
  onAddTerm: () => void;
  onAddReplacement: () => void;
  onDelete: (id: string) => void;
}) {
  return (
    <>
      <section className="card">
        <h2>添加术语</h2>
        <div className="form-row">
          <input
            className="input"
            placeholder="如 Morpho、GPT-4"
            value={termInput}
            onChange={(e) => onTermInput(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && onAddTerm()}
          />
          <button
            type="button"
            className="btn"
            disabled={busy || !termInput.trim()}
            onClick={onAddTerm}
          >
            添加 term
          </button>
        </div>
      </section>

      <section className="card">
        <h2>添加替换规则</h2>
        <div className="form-row">
          <input
            className="input"
            placeholder="from（识别错）"
            value={fromInput}
            onChange={(e) => onFromInput(e.target.value)}
          />
          <span className="arrow">→</span>
          <input
            className="input"
            placeholder="to（正确）"
            value={toInput}
            onChange={(e) => onToInput(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && onAddReplacement()}
          />
          <button
            type="button"
            className="btn"
            disabled={busy || !fromInput.trim() || !toInput.trim()}
            onClick={onAddReplacement}
          >
            添加 replacement
          </button>
        </div>
      </section>

      <section className="card">
        <h2>词条 ({entries.length})</h2>
        {entries.length === 0 ? (
          <p className="muted-text">词典为空。先添加术语或从「编辑学习」确认候选。</p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>类型</th>
                <th>内容</th>
                <th>来源</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {entries.map((e) => (
                <tr key={e.id}>
                  <td>
                    <span className="chip">{e.kind}</span>
                  </td>
                  <td>
                    {e.kind === "term"
                      ? e.term
                      : `${e.from_text ?? ""} → ${e.to_text ?? ""}`}
                  </td>
                  <td className="muted-text">{e.source}</td>
                  <td>
                    <button
                      type="button"
                      className="btn small danger"
                      disabled={busy}
                      onClick={() => onDelete(e.id)}
                    >
                      删除
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </>
  );
}

function LearnPanel({
  before,
  after,
  candidates,
  busy,
  onBefore,
  onAfter,
  onSuggest,
  onConfirm,
}: {
  before: string;
  after: string;
  candidates: LearnCandidate[];
  busy: boolean;
  onBefore: (v: string) => void;
  onAfter: (v: string) => void;
  onSuggest: () => void;
  onConfirm: (c: LearnCandidate) => void;
}) {
  return (
    <section className="card">
      <h2>从编辑生成候选</h2>
      <p className="muted-text">
        产品策略：只建议短词/短语；需你确认后才写入词典（默认不自动学）。
      </p>
      <div className="learn-grid">
        <label>
          <span>修改前（ASR / 修正稿）</span>
          <textarea
            className="textarea"
            rows={3}
            value={before}
            onChange={(e) => onBefore(e.target.value)}
            placeholder="脱肯"
          />
        </label>
        <label>
          <span>修改后（用户终稿）</span>
          <textarea
            className="textarea"
            rows={3}
            value={after}
            onChange={(e) => onAfter(e.target.value)}
            placeholder="Token"
          />
        </label>
      </div>
      <div className="actions">
        <button
          type="button"
          className="btn"
          disabled={busy || !before.trim() || !after.trim()}
          onClick={onSuggest}
        >
          生成候选
        </button>
      </div>

      {candidates.length > 0 && (
        <>
          <h3 className="subhead">候选</h3>
          <ul className="list">
            {candidates.map((c, i) => (
              <li key={i} className="candidate">
                <div>
                  <span className="chip">{c.kind}</span>{" "}
                  {c.kind === "term"
                    ? c.term
                    : `${c.from_text ?? ""} → ${c.to_text ?? ""}`}
                  <div className="muted-text">{c.reason}</div>
                </div>
                <button
                  type="button"
                  className="btn small"
                  disabled={busy}
                  onClick={() => onConfirm(c)}
                >
                  确认加入词典
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}
