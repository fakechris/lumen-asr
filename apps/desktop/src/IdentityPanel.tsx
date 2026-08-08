import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { api } from "./api";
import type { EnrolledSpeaker, EnrollConflict, Meeting } from "./types";

/** Global voiceprint manager: the whole enrolled-identity library on one page.
 *
 * The per-meeting side panel only lets you enroll/delete/self-mark. This page
 * is the cross-meeting view — rename an identity, merge two that turned out to
 * be the same person (the fix for "labelled A in one meeting, B in another"),
 * prune a bad sample, and see which meetings each voiceprint came from. All of
 * it is local; embeddings never leave the machine. */
export function IdentityPanel({
  onError,
}: {
  onError: (message: string | null) => void;
}) {
  const [enrolled, setEnrolled] = useState<EnrolledSpeaker[]>([]);
  const [conflicts, setConflicts] = useState<EnrollConflict[]>([]);
  const [selfIdentityId, setSelfIdentityId] = useState<string | null>(null);
  const [titles, setTitles] = useState<Record<string, string>>({});
  const [loading, setLoading] = useState(true);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draftName, setDraftName] = useState("");
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [mergeInto, setMergeInto] = useState<Record<string, string>>({});
  const [selfName, setSelfName] = useState("我");
  const [selfBusy, setSelfBusy] = useState(false);
  const [selfMsg, setSelfMsg] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  const refresh = useCallback(async () => {
    // Each read degrades independently: a failed meeting-list still lets the
    // identity list render (titles just fall back to the raw id).
    const [identities, selfId, meetings, conflictList] = await Promise.allSettled(
      [
        api.listEnrolledSpeakers(),
        api.getSelfIdentity(),
        api.listMeetings(),
        api.listEnrollConflicts(),
      ],
    );
    if (identities.status === "fulfilled") setEnrolled(identities.value);
    if (selfId.status === "fulfilled") setSelfIdentityId(selfId.value);
    if (conflictList.status === "fulfilled") setConflicts(conflictList.value);
    if (meetings.status === "fulfilled") {
      setTitles(
        Object.fromEntries(
          meetings.value.map((m: Meeting) => [m.id, m.title ?? "未命名会议"]),
        ),
      );
    }
    const failed = [identities, selfId, meetings, conflictList].find(
      (r): r is PromiseRejectedResult => r.status === "rejected",
    );
    if (failed) onError(String(failed.reason));
    setLoading(false);
  }, [onError]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (editingId) inputRef.current?.select();
  }, [editingId]);

  const meetingLabel = useCallback(
    (id: string | null | undefined) =>
      id ? (titles[id] ?? "其它会议") : "手动注册",
    [titles],
  );

  const beginRename = useCallback((identity: EnrolledSpeaker) => {
    onError(null);
    setEditingId(identity.id);
    setDraftName(identity.name);
  }, [onError]);

  const commitRename = useCallback(async () => {
    const id = editingId;
    if (!id) return;
    const name = draftName.trim();
    setEditingId(null);
    const current = enrolled.find((i) => i.id === id);
    if (!name || !current || name === current.name) return;
    try {
      await api.renameEnrolledSpeaker(id, name);
      await refresh();
    } catch (e) {
      onError(String(e));
    }
  }, [editingId, draftName, enrolled, refresh, onError]);

  const toggleSelf = useCallback(
    async (identityId: string) => {
      onError(null);
      try {
        const next = selfIdentityId === identityId ? null : identityId;
        setSelfIdentityId(await api.setSelfIdentity(next));
      } catch (e) {
        onError(String(e));
      }
    },
    [selfIdentityId, onError],
  );

  const remove = useCallback(
    async (identity: EnrolledSpeaker) => {
      if (
        !window.confirm(
          `删除声纹「${identity.name}」？已识别的会议不受影响，之后的会议将不再自动识别此人。`,
        )
      )
        return;
      onError(null);
      try {
        await api.removeEnrolledSpeaker(identity.id);
        await refresh();
      } catch (e) {
        onError(String(e));
      }
    },
    [refresh, onError],
  );

  const merge = useCallback(
    async (from: EnrolledSpeaker) => {
      const intoId = mergeInto[from.id];
      const into = enrolled.find((i) => i.id === intoId);
      if (!into) return;
      if (
        !window.confirm(
          `把「${from.name}」的所有声纹样本合并到「${into.name}」，然后删除「${from.name}」？用于两个名字其实是同一个人。`,
        )
      )
        return;
      onError(null);
      try {
        await api.mergeEnrolledSpeakers(from.id, into.id);
        setMergeInto((prev) => {
          const next = { ...prev };
          delete next[from.id];
          return next;
        });
        await refresh();
      } catch (e) {
        onError(String(e));
      }
    },
    [mergeInto, enrolled, refresh, onError],
  );

  const enrollSelf = useCallback(async () => {
    setSelfBusy(true);
    setSelfMsg(null);
    onError(null);
    try {
      const r = await api.enrollSelfFromRecordings(selfName);
      setSelfMsg(
        `已从 ${r.enrolled} 段录音注册「${r.name}」并标记为「我」（扫描 ${r.scanned} 段）。`,
      );
      await refresh();
    } catch (e) {
      onError(String(e));
    } finally {
      setSelfBusy(false);
    }
  }, [selfName, refresh, onError]);

  const resolveConflict = useCallback(
    async (conflict: EnrollConflict, enrollAs: string | null) => {
      onError(null);
      try {
        await api.resolveEnrollConflict(conflict.id, enrollAs);
        await refresh();
      } catch (e) {
        onError(String(e));
      }
    },
    [refresh, onError],
  );

  const removeSample = useCallback(
    async (identity: EnrolledSpeaker, index: number) => {
      const last = identity.samples.length === 1;
      if (
        last &&
        !window.confirm(
          `这是「${identity.name}」最后一个声纹样本，删除后整条声纹将被移除。继续？`,
        )
      )
        return;
      onError(null);
      try {
        await api.removeSpeakerSample(identity.id, index);
        await refresh();
      } catch (e) {
        onError(String(e));
      }
    },
    [refresh, onError],
  );

  const toggleExpanded = useCallback((id: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const others = useCallback(
    (id: string) => enrolled.filter((i) => i.id !== id),
    [enrolled],
  );

  const dateLabel = useMemo(
    () =>
      new Intl.DateTimeFormat("zh-CN", {
        year: "numeric",
        month: "2-digit",
        day: "2-digit",
      }),
    [],
  );
  const formatDate = useCallback(
    (iso: string) => {
      const d = new Date(iso);
      return Number.isNaN(d.getTime()) ? "—" : dateLabel.format(d);
    },
    [dateLabel],
  );

  return (
    <div className="card identity-panel">
      <header className="identity-head">
        <h2>声纹管理</h2>
        <p className="muted">
          跨会议的全局声纹库：重命名、合并同一个人的两条声纹、删除某次样本，
          标记「这是我」。全部保存在本机，声纹数据不会离开设备。
        </p>
      </header>

      <section className="identity-self-enroll">
        <h3>这是我</h3>
        <p className="muted">
          从你最近的听写录音里提取声音，注册为「我」。之后会议里认出你时会直接显示「我」。
          需要几段有清晰说话的听写录音。
        </p>
        <div className="identity-self-row">
          <input
            className="identity-name-input"
            value={selfName}
            onChange={(e) => setSelfName(e.target.value)}
            aria-label="我的名字"
          />
          <button
            type="button"
            className="btn"
            disabled={selfBusy}
            onClick={() => void enrollSelf()}
          >
            {selfBusy ? "注册中…" : "从我的听写录音注册"}
          </button>
        </div>
        {selfMsg && (
          <p className="identity-self-msg" role="status" aria-live="polite">
            {selfMsg}
          </p>
        )}
      </section>

      {conflicts.length > 0 && (
        <section className="identity-conflicts">
          <h3>待处理冲突（{conflicts.length}）</h3>
          <p className="muted">
            这些会议里标注的名字，声纹却和库里另一个人高度相似，已暂缓自动注册，
            请确认是不是同一个人。
          </p>
          <ul className="identity-conflict-list">
            {conflicts.map((c) => (
              <li key={c.id} className="identity-conflict">
                <div className="identity-conflict-text">
                  会议「{titles[c.meetingId] ?? "未命名会议"}」中标注为「
                  <b>{c.labelName}</b>」，但声纹与已注册的「<b>{c.existingName}</b>
                  」相似度约 {(c.score * 100).toFixed(0)}%。
                </div>
                <div className="identity-conflict-actions">
                  <button
                    type="button"
                    className="btn small"
                    title={`同一个人：把这段声纹并入「${c.existingName}」`}
                    onClick={() => void resolveConflict(c, c.existingName)}
                  >
                    就是{c.existingName}
                  </button>
                  <button
                    type="button"
                    className="btn small"
                    title={`不同的人：单独注册为「${c.labelName}」`}
                    onClick={() => void resolveConflict(c, c.labelName)}
                  >
                    确实是{c.labelName}
                  </button>
                  <button
                    type="button"
                    className="btn small"
                    title="先不处理，仅从列表移除"
                    onClick={() => void resolveConflict(c, null)}
                  >
                    忽略
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </section>
      )}

      {loading ? (
        <p className="muted">加载中…</p>
      ) : enrolled.length === 0 ? (
        <p className="muted">
          还没有注册任何声纹。在会议详情里为说话人「注册声纹」后，就会出现在这里，
          并在之后的会议中自动识别。
        </p>
      ) : (
        <ul className="identity-list">
          {enrolled.map((identity) => {
            const isSelf = selfIdentityId === identity.id;
            const open = expanded.has(identity.id);
            const meetings = Array.from(
              new Set(
                identity.samples
                  .map((s) => s.sourceMeetingId)
                  .filter((v): v is string => !!v),
              ),
            );
            return (
              <li key={identity.id} className="identity-item">
                <div className="identity-row">
                  {editingId === identity.id ? (
                    <input
                      ref={inputRef}
                      className="identity-name-input"
                      value={draftName}
                      onChange={(e) => setDraftName(e.target.value)}
                      onBlur={() => void commitRename()}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") void commitRename();
                        if (e.key === "Escape") setEditingId(null);
                      }}
                    />
                  ) : (
                    <button
                      type="button"
                      className="identity-name"
                      title="点击重命名"
                      onClick={() => beginRename(identity)}
                    >
                      {identity.name}
                      {isSelf && (
                        <span className="identity-self" title="这是你自己">
                          我
                        </span>
                      )}
                    </button>
                  )}
                  <span className="muted identity-meta">
                    {identity.samples.length} 个样本
                  </span>
                  <span className="identity-actions">
                    <button
                      type="button"
                      className="btn small"
                      onClick={() => void toggleSelf(identity.id)}
                    >
                      {isSelf ? "取消我" : "这是我"}
                    </button>
                    <button
                      type="button"
                      className="btn small"
                      onClick={() => toggleExpanded(identity.id)}
                      aria-expanded={open}
                    >
                      样本 {open ? "▾" : "▸"}
                    </button>
                    <button
                      type="button"
                      className="btn small danger"
                      onClick={() => void remove(identity)}
                    >
                      删除
                    </button>
                  </span>
                </div>

                {meetings.length > 0 && (
                  <div className="muted identity-sources">
                    来源：{meetings.map((m) => meetingLabel(m)).join("、")}
                  </div>
                )}

                {others(identity.id).length > 0 && (
                  <div className="identity-merge">
                    <span className="muted">同一个人？合并到</span>
                    <select
                      value={mergeInto[identity.id] ?? ""}
                      onChange={(e) =>
                        setMergeInto((prev) => ({
                          ...prev,
                          [identity.id]: e.target.value,
                        }))
                      }
                    >
                      <option value="">选择声纹…</option>
                      {others(identity.id).map((o) => (
                        <option key={o.id} value={o.id}>
                          {o.name}
                        </option>
                      ))}
                    </select>
                    <button
                      type="button"
                      className="btn small"
                      disabled={!mergeInto[identity.id]}
                      onClick={() => void merge(identity)}
                    >
                      合并
                    </button>
                  </div>
                )}

                {open && (
                  <ul className="identity-samples">
                    {identity.samples.map((sample, index) => (
                      <li
                        key={`${sample.enrolledAt}-${index}`}
                        className="identity-sample"
                      >
                        <span className="muted">
                          {formatDate(sample.enrolledAt)} ·{" "}
                          {meetingLabel(sample.sourceMeetingId)}
                          {sample.voicedMs > 0 &&
                            ` · ${(sample.voicedMs / 1000).toFixed(0)}s`}
                        </span>
                        <button
                          type="button"
                          className="btn small danger"
                          title="删除这次样本"
                          onClick={() => void removeSample(identity, index)}
                        >
                          删除样本
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
