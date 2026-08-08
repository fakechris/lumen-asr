import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import { api } from "./api";
import type { EnrolledSpeaker, EnrollConflict, Meeting } from "./types";

type SelfProgress = { scanned: number; enrolled: number; target: number };

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
  const [selfProgress, setSelfProgress] = useState<SelfProgress | null>(null);
  // Which mutating action is in flight ("merge:<id>", "sample:<id>:<i>", …),
  // so its button can show a busy state and double-clicks are ignored.
  const [busy, setBusy] = useState<string | null>(null);
  // Single audio player: only one sample plays at a time.
  const [playing, setPlaying] = useState<string | null>(null);
  const [loadingAudio, setLoadingAudio] = useState<string | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const audioUrlRef = useRef<string | null>(null);
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

  /** Stop and fully tear down the current player (idempotent). */
  const stopAudio = useCallback(() => {
    const audio = audioRef.current;
    if (audio) {
      audio.pause();
      audio.src = "";
      audioRef.current = null;
    }
    if (audioUrlRef.current) {
      URL.revokeObjectURL(audioUrlRef.current);
      audioUrlRef.current = null;
    }
    setPlaying(null);
  }, []);

  /** Play one sample. Only one plays at a time: clicking the playing sample
   * stops it; clicking another stops the first and plays the new one. */
  const playSample = useCallback(
    async (identityId: string, index: number) => {
      const key = `${identityId}:${index}`;
      if (playing === key) {
        stopAudio();
        return;
      }
      stopAudio(); // stop whatever else is playing first
      onError(null);
      setLoadingAudio(key);
      try {
        const buf = await api.readVoiceprintSampleAudio(identityId, index);
        const url = URL.createObjectURL(new Blob([buf], { type: "audio/wav" }));
        const audio = new Audio(url);
        audioRef.current = audio;
        audioUrlRef.current = url;
        const clear = () => {
          if (audioUrlRef.current === url) {
            URL.revokeObjectURL(url);
            audioUrlRef.current = null;
            audioRef.current = null;
            setPlaying((p) => (p === key ? null : p));
          }
        };
        audio.onended = clear;
        audio.onerror = clear;
        await audio.play();
        setPlaying(key);
      } catch (e) {
        stopAudio();
        onError(String(e));
      } finally {
        setLoadingAudio((k) => (k === key ? null : k));
      }
    },
    [playing, stopAudio, onError],
  );

  // Stop playback when leaving the page / listen for self-enroll progress.
  useEffect(() => stopAudio, [stopAudio]);
  useEffect(() => {
    const unlisten = listen<SelfProgress>("self-enroll-progress", (e) =>
      setSelfProgress(e.payload),
    );
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  const commitRename = useCallback(async () => {
    const id = editingId;
    if (!id) return;
    const name = draftName.trim();
    setEditingId(null);
    const current = enrolled.find((i) => i.id === id);
    if (!name || !current || name === current.name || busy) return;
    setBusy(`rename:${id}`);
    try {
      await api.renameEnrolledSpeaker(id, name);
      await refresh();
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(null);
    }
  }, [editingId, draftName, enrolled, busy, refresh, onError]);

  const toggleSelf = useCallback(
    async (identityId: string) => {
      if (busy) return;
      onError(null);
      setBusy(`self:${identityId}`);
      try {
        const next = selfIdentityId === identityId ? null : identityId;
        setSelfIdentityId(await api.setSelfIdentity(next));
      } catch (e) {
        onError(String(e));
      } finally {
        setBusy(null);
      }
    },
    [selfIdentityId, busy, onError],
  );

  const remove = useCallback(
    async (identity: EnrolledSpeaker) => {
      if (
        busy ||
        !window.confirm(
          `删除声纹「${identity.name}」？已识别的会议不受影响，之后的会议将不再自动识别此人。`,
        )
      )
        return;
      onError(null);
      setBusy(`remove:${identity.id}`);
      try {
        await api.removeEnrolledSpeaker(identity.id);
        await refresh();
      } catch (e) {
        onError(String(e));
      } finally {
        setBusy(null);
      }
    },
    [busy, refresh, onError],
  );

  const merge = useCallback(
    async (from: EnrolledSpeaker) => {
      const intoId = mergeInto[from.id];
      const into = enrolled.find((i) => i.id === intoId);
      if (!into || busy) return;
      if (
        !window.confirm(
          `把「${from.name}」的所有声纹样本合并到「${into.name}」，然后删除「${from.name}」？用于两个名字其实是同一个人。`,
        )
      )
        return;
      onError(null);
      setBusy(`merge:${from.id}`);
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
      } finally {
        setBusy(null);
      }
    },
    [mergeInto, enrolled, busy, refresh, onError],
  );

  const enrollSelf = useCallback(async () => {
    if (selfBusy) return;
    // When a self identity already exists, always top it up under its own name
    // (never fork a second identity from an edited field).
    const targetName =
      enrolled.find((i) => i.id === selfIdentityId)?.name ?? selfName;
    setSelfBusy(true);
    setSelfMsg(null);
    setSelfProgress(null);
    onError(null);
    try {
      const r = await api.enrollSelfFromRecordings(targetName);
      setSelfMsg(
        `已标记「${r.name}」为「我」：新增 ${r.enrolled} 个声纹样本（扫描 ${r.scanned} 段录音）。`,
      );
      await refresh();
    } catch (e) {
      onError(String(e));
    } finally {
      setSelfBusy(false);
      setSelfProgress(null);
    }
  }, [selfName, selfBusy, enrolled, selfIdentityId, refresh, onError]);

  const resolveConflict = useCallback(
    async (conflict: EnrollConflict, enrollAs: string | null) => {
      if (busy) return;
      onError(null);
      setBusy(`conflict:${conflict.id}`);
      try {
        await api.resolveEnrollConflict(conflict.id, enrollAs);
        await refresh();
      } catch (e) {
        onError(String(e));
      } finally {
        setBusy(null);
      }
    },
    [busy, refresh, onError],
  );

  const removeSample = useCallback(
    async (identity: EnrolledSpeaker, index: number) => {
      if (busy) return;
      const last = identity.samples.length === 1;
      if (
        last &&
        !window.confirm(
          `这是「${identity.name}」最后一个声纹样本，删除后整条声纹将被移除。继续？`,
        )
      )
        return;
      // Stop playback if the sample being removed is the one playing.
      if (playing === `${identity.id}:${index}`) stopAudio();
      onError(null);
      setBusy(`sample:${identity.id}:${index}`);
      try {
        await api.removeSpeakerSample(identity.id, index);
        await refresh();
      } catch (e) {
        onError(String(e));
      } finally {
        setBusy(null);
      }
    },
    [busy, playing, stopAudio, refresh, onError],
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

  const selfIdentity = enrolled.find((i) => i.id === selfIdentityId) ?? null;

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
          {selfIdentityId
            ? "已注册过——再次点击会扫描更新的录音、补充新样本（不会重复已有的）。"
            : "需要几段有清晰说话的听写录音。"}
        </p>
        <div className="identity-self-row">
          <input
            className="identity-name-input"
            value={selfIdentity ? selfIdentity.name : selfName}
            onChange={(e) => setSelfName(e.target.value)}
            disabled={selfBusy || !!selfIdentity}
            title={selfIdentity ? "已注册，补充样本会加到这条身份" : undefined}
            aria-label="我的名字"
          />
          <button
            type="button"
            className="btn"
            disabled={selfBusy}
            onClick={() => void enrollSelf()}
          >
            {selfBusy ? (
              <span className="identity-inline-busy">
                <span className="spinner" aria-hidden="true" />
                注册中…
              </span>
            ) : selfIdentityId ? (
              "补充样本（扫描新录音）"
            ) : (
              "从我的听写录音注册"
            )}
          </button>
        </div>
        {selfBusy && (
          <p className="identity-self-msg" role="status" aria-live="polite">
            {selfProgress
              ? `正在从录音中提取声纹…已扫描 ${selfProgress.scanned} 段，采集 ${selfProgress.enrolled}/${selfProgress.target}`
              : "正在读取录音…"}
          </p>
        )}
        {!selfBusy && selfMsg && (
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
                    disabled={busy === `conflict:${c.id}`}
                    title={`同一个人：把这段声纹并入「${c.existingName}」`}
                    onClick={() => void resolveConflict(c, c.existingName)}
                  >
                    就是{c.existingName}
                  </button>
                  <button
                    type="button"
                    className="btn small"
                    disabled={busy === `conflict:${c.id}`}
                    title={`不同的人：单独注册为「${c.labelName}」`}
                    onClick={() => void resolveConflict(c, c.labelName)}
                  >
                    确实是{c.labelName}
                  </button>
                  <button
                    type="button"
                    className="btn small"
                    disabled={busy === `conflict:${c.id}`}
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
                      disabled={!!busy}
                      title={
                        isSelf
                          ? "取消「这是我」标注"
                          : "把这条声纹标记为你自己"
                      }
                      onClick={() => void toggleSelf(identity.id)}
                    >
                      {isSelf ? "取消标注" : "这是我"}
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
                      disabled={!!busy}
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
                      disabled={!mergeInto[identity.id] || !!busy}
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
                        <div className="identity-sample-info">
                          <span className="muted">
                            {formatDate(sample.enrolledAt)} ·{" "}
                            {meetingLabel(sample.sourceMeetingId)}
                            {sample.voicedMs > 0 &&
                              ` · ${(sample.voicedMs / 1000).toFixed(0)}s`}
                          </span>
                          {sample.sourceLabel && (
                            <span
                              className="identity-sample-text"
                              title={sample.sourceLabel}
                            >
                              「{sample.sourceLabel}」
                            </span>
                          )}
                        </div>
                        <span className="identity-sample-actions">
                          {sample.hasAudio ? (
                            (() => {
                              const key = `${identity.id}:${index}`;
                              const isPlaying = playing === key;
                              const isLoading = loadingAudio === key;
                              return (
                                <button
                                  type="button"
                                  className="btn small"
                                  disabled={isLoading}
                                  title={
                                    isPlaying
                                      ? "停止播放"
                                      : "播放这段录音，确认是不是你"
                                  }
                                  onClick={() =>
                                    void playSample(identity.id, index)
                                  }
                                >
                                  {isLoading
                                    ? "⏳ 加载"
                                    : isPlaying
                                      ? "⏸ 停止"
                                      : "▶ 播放"}
                                </button>
                              );
                            })()
                          ) : (
                            <span className="muted identity-sample-noaudio">
                              旧样本·无法回放
                            </span>
                          )}
                          <button
                            type="button"
                            className="btn small danger"
                            disabled={busy === `sample:${identity.id}:${index}`}
                            title="删除这次样本"
                            onClick={() => void removeSample(identity, index)}
                          >
                            删除样本
                          </button>
                        </span>
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
