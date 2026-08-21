import { invoke } from "@tauri-apps/api/core";
import type {
  AsrStatus,
  AudioDevice,
  BuildInfo,
  CorrectorStatus,
  CorrectTextOutcome,
  ContextSnapshotRecord,
  DictationAttemptRecord,
  DictionaryEntry,
  EditEvent,
  EditLearningFeedback,
  EditLearningObservability,
  EditObservation,
  ExportOutput,
  ExportPreset,
  Health,
  LearnCandidate,
  LearningProposal,
  LiveAnnotation,
  Meeting,
  MeetingDetail,
  MeetingRecordingResult,
  MeetingStatus,
  SessionRecord,
  TranscribeOutcome,
} from "./types";

export type PermissionStatus = {
  microphone: string;
  accessibility: string;
  accessibilityTrusted: boolean;
  canRecord: boolean;
  canInject: boolean;
  copyOnlyOk: boolean;
  processHint: string;
  processPath: string;
  /** Name likely shown in System Settings → Accessibility (e.g. Lumen ASR). */
  settingsListName: string;
  bundleId: string;
  codesignKind: string;
  codesignIdentifier: string;
  codesignAdhoc: boolean;
};

export type OnboardingState = {
  completed: boolean;
  skipped: boolean;
  version: number;
  step: number;
  showWizard: boolean;
  maxStepStageB: number;
};

export type AsrModelCandidate = {
  engine: string;
  path: string;
  label: string;
  ready: boolean;
  source: string;
};

export type AsrModelStatus = {
  sensevoiceReady: boolean;
  sensevoiceDir: string;
  whisperReady: boolean;
  whisperDir: string;
  qwenReady: boolean;
  qwenDir: string;
  paraformerOfflineReady: boolean;
  paraformerOfflineDir: string;
  paraformerStreamingReady: boolean;
  paraformerStreamingDir: string;
  qwenRuntimeSupported: boolean;
  qwenFallbackReason?: string | null;
  recommendedEngine: string;
  totalMemoryMb?: number | null;
  modelsRoot: string;
  activeEngine: string;
  activeModelDir: string;
  candidates: AsrModelCandidate[];
  downloadUrl: string;
  paraformerOfflineDownloadUrl: string;
  paraformerStreamingDownloadUrl: string;
};

export type CorrectorProbeResult = {
  ollamaRunning: boolean;
  ollamaUrl: string;
  ollamaModels: string[];
  hasQwen257b: boolean;
  envOpenaiBase?: string | null;
  envOpenaiKeySet: boolean;
  envLumenModel?: string | null;
  suggestedProvider: string;
  suggestedBaseUrl: string;
  suggestedModel: string;
  message: string;
};

export type HotkeyValidation = {
  ok: boolean;
  shortcut: string;
  warnings: string[];
  errors: string[];
};

export type HotkeyIntent = {
  id: string;
  chord: string;
  mode: string;
  intent: string;
  targetLanguage: string;
  /** For intent="translate": optional translation style/register — a preset
   * key ("faithful" | "formal" | "casual" | "social") or free-form custom text.
   * Omitted/empty means faithful translation. */
  translateStyle?: string;
  enabled: boolean;
};

export type MeetingDetectionStatus = {
  /** The user's opt-in preference (persisted). */
  enabled: boolean;
  /** Whether this OS exposes the audio-activity capability at all. */
  capabilityAvailable: boolean;
  /** Whether the detector poller is currently running. */
  active: boolean;
};

export type MeetingAppKind = "meeting" | "browser";

export type MeetingAppEntry = {
  name: string;
  kind: MeetingAppKind;
  bundle_ids: string[];
  detect: boolean;
  capture: boolean;
};

export type MeetingAppCatalog = {
  path: string;
  version: number;
  applications: MeetingAppEntry[];
  loadError?: string | null;
};

/** Local-only meeting-detection counters (stored on this machine, never
 * uploaded) — how often detection prompted/suggested and what the user chose. */
export type MeetingDetectionStats = {
  promptShown: number;
  promptAccepted: number;
  promptDismissed: number;
  stopSuggested: number;
  stopAccepted: number;
  stopDeclined: number;
};

/** Meeting watchdog settings: auto-stop after prolonged mic silence, and a
 * prompt to stop when a calendar-linked meeting's end time passes. */
export type MeetingWatchdogConfig = {
  /** Minutes of continuous mic silence before an unattended recording
   * auto-stops. `0` disables the auto-stop. */
  silenceAutoStopMinutes: number;
  /** Prompt to stop when a calendar-linked meeting's end time passes. */
  calendarEndReminder: boolean;
};

export const api = {
  health: () => invoke<Health>("app_health"),
  buildInfo: () => invoke<BuildInfo>("build_info"),

  listAudioDevices: () => invoke<AudioDevice[]>("list_audio_devices"),
  getAudioDevice: () => invoke<string | null>("get_audio_device"),
  setAudioDevice: (name: string | null) =>
    invoke<void>("set_audio_device", { name }),
  setAsrEngine: (engine: string) =>
    invoke<string>("set_asr_engine", { engine }),
  getAsrStatus: () => invoke<AsrStatus>("get_asr_status"),
  startRecording: () => invoke<void>("start_recording"),
  stopAndTranscribe: (save = true) =>
    invoke<TranscribeOutcome>("stop_and_transcribe", { save }),
  cancelRecording: () => invoke<void>("cancel_recording"),

  getCorrectorConfig: () => invoke<CorrectorStatus>("get_corrector_config"),
  saveCorrectorConfig: (input: {
    enabled?: boolean;
    useCapturedContext?: boolean;
    provider?: string;
    baseUrl?: string;
    model?: string;
    apiKey?: string;
    timeoutSecs?: number;
    cleanup?: string;
    cleanupProfile?: string;
    style?: string;
    casing?: string;
    punctuation?: string;
    polish?: string[];
    customEnabled?: boolean;
    customInstruction?: string;
  }) => invoke<CorrectorStatus>("save_corrector_config", { input }),
  listLlmPresets: () =>
    invoke<
      {
        id: string;
        label: string;
        kind: string;
        baseUrl: string;
        defaultModel: string;
        models: string[];
        needsApiKey: boolean;
        notes: string;
      }[]
    >("list_llm_presets"),
  listAsrPresets: () =>
    invoke<
      {
        id: string;
        label: string;
        kind: string;
        baseUrl: string;
        defaultModel: string;
        models: string[];
        needsApiKey: boolean;
        status: string;
        notes: string;
      }[]
    >("list_asr_presets"),
  getAsrServiceConfig: () =>
    invoke<{
      provider: string;
      runtimePath: string;
      qwenShadowEnabled: boolean;
      baseUrl: string;
      model: string;
      hasApiKey: boolean;
      language: string;
      timeoutSecs: number;
    }>("get_asr_service_config"),
  saveAsrServiceConfig: (input: {
    provider?: string;
    runtimePath?: string;
    qwenShadowEnabled?: boolean;
    baseUrl?: string;
    model?: string;
    apiKey?: string;
    language?: string;
    timeoutSecs?: number;
  }) =>
    invoke<{
      provider: string;
      runtimePath: string;
      qwenShadowEnabled: boolean;
      baseUrl: string;
      model: string;
      hasApiKey: boolean;
      language: string;
      timeoutSecs: number;
    }>("save_asr_service_config", { input }),
  correctText: (text: string) =>
    invoke<CorrectTextOutcome>("correct_text", { input: { text } }),

  getPermissionStatus: () => invoke<PermissionStatus>("get_permission_status"),
  pollPermissions: () => invoke<PermissionStatus>("poll_permissions"),
  openMicrophoneSettings: () => invoke<void>("open_microphone_settings"),
  openAccessibilitySettings: () => invoke<void>("open_accessibility_settings"),
  requestAccessibilityAccess: () =>
    invoke<PermissionStatus>("request_accessibility_access"),
  dismissAccessibilityDragOverlay: () =>
    invoke<void>("dismiss_accessibility_drag_overlay_cmd"),
  requestMicrophoneAccess: () =>
    invoke<PermissionStatus>("request_microphone_access"),

  getOnboardingState: () => invoke<OnboardingState>("get_onboarding_state"),
  setOnboardingStep: (step: number) =>
    invoke<OnboardingState>("set_onboarding_step", { input: { step } }),
  skipOnboarding: () => invoke<OnboardingState>("skip_onboarding"),
  completeOnboarding: (completeAll = true) =>
    invoke<OnboardingState>("complete_onboarding", { completeAll }),
  reopenOnboarding: () => invoke<OnboardingState>("reopen_onboarding"),

  startVolumeMonitoring: (device?: string | null) =>
    invoke<void>("start_volume_monitoring_cmd", { device: device ?? null }),
  stopVolumeMonitoring: () => invoke<void>("stop_volume_monitoring_cmd"),

  checkAsrModelStatus: () => invoke<AsrModelStatus>("check_asr_model_status"),
  listLocalAsrModels: () => invoke<AsrModelCandidate[]>("list_local_asr_models"),
  useExistingAsrModel: (path: string, engine?: string) =>
    invoke<AsrModelStatus>("use_existing_asr_model", {
      input: { path, engine },
    }),
  startAsrModelDownload: () => invoke<AsrModelStatus>("start_asr_model_download"),
  downloadParaformerOffline: () =>
    invoke<AsrModelStatus>("start_paraformer_offline_download"),
  downloadParaformerStreaming: () =>
    invoke<AsrModelStatus>("start_paraformer_streaming_download"),
  cancelAsrModelDownload: () => invoke<void>("cancel_asr_model_download"),

  probeCorrector: () => invoke<CorrectorProbeResult>("probe_corrector"),
  ollamaListModels: () => invoke<string[]>("ollama_list_models"),
  ollamaPullModel: (model?: string) =>
    invoke<CorrectorProbeResult>("ollama_pull_model", {
      input: { model: model ?? null },
    }),
  applyCorrectorSuggestion: (input: {
    provider: string;
    baseUrl: string;
    model: string;
    enabled?: boolean;
    apiKey?: string;
  }) => invoke<CorrectorStatus>("apply_corrector_suggestion", { input }),

  validateHotkey: (shortcut: string) =>
    invoke<HotkeyValidation>("validate_hotkey", { shortcut }),

  getInjectConfig: () =>
    invoke<{
      mode: string;
      preserveClipboard: boolean;
      autoInsert: boolean;
    }>("get_inject_config"),
  saveInjectConfig: (input: {
    mode?: string;
    preserveClipboard?: boolean;
    autoInsert?: boolean;
  }) => invoke("save_inject_config", { input }),
  insertText: (text: string) =>
    invoke<{ strategy: string; restoredClipboard: boolean }>("insert_text", {
      text,
    }),

  toggleDictation: () => invoke<void>("toggle_dictation_cmd"),
  getHotkeyConfig: () =>
    invoke<{
      enabled: boolean;
      toggle: string;
      showCapsule: boolean;
      mode: string;
      intents: HotkeyIntent[];
      eventTapActive: boolean;
      registerNote: string;
    }>("get_hotkey_config"),
  saveHotkeyConfig: (input: {
    enabled?: boolean;
    toggle?: string;
    showCapsule?: boolean;
    mode?: string;
    intents?: HotkeyIntent[];
  }) =>
    invoke<{
      enabled: boolean;
      toggle: string;
      showCapsule: boolean;
      mode: string;
      intents: HotkeyIntent[];
      eventTapActive: boolean;
      registerNote: string;
    }>("save_hotkey_config", { input }),
  pauseHotkeys: () => invoke<void>("pause_hotkeys"),
  resumeHotkeys: () => invoke<void>("resume_hotkeys"),
  /** Start backend Fn/🌐 polling so the recorder can capture a Fn press
   * (webview never receives Fn events on macOS). No-op off macOS. */
  startFnCapture: () => invoke<void>("start_fn_capture"),
  stopFnCapture: () => invoke<void>("stop_fn_capture"),

  getLearningConfig: () =>
    invoke<import("./types").LearningConfig>("get_learning_config"),
  saveLearningConfig: (input: {
    autoPromote?: boolean;
    autoPromoteThreshold?: number;
    postPasteCapture?: boolean;
    postPasteSeconds?: number;
    persistEditEvidenceText?: boolean;
  }) => invoke<import("./types").LearningConfig>("save_learning_config", { input }),
  processEdit: (input: {
    beforeText: string;
    afterText: string;
    sessionId?: string;
    source?: string;
    recordEvent?: boolean;
  }) => invoke<import("./types").ProcessEditResult>("process_edit", { input }),

  listSessions: (limit = 50) =>
    invoke<SessionRecord[]>("list_sessions", { limit }),

  getSession: (id: string) =>
    invoke<SessionRecord | null>("get_session", { id }),

  listSessionAttempts: (
    sessionId: string,
    limit = 100,
    beforeOrdinal?: number,
  ) =>
    invoke<DictationAttemptRecord[]>("list_session_attempts", {
      sessionId,
      limit,
      beforeOrdinal,
    }),

  listContextSnapshots: (sessionId: string) =>
    invoke<ContextSnapshotRecord[]>("list_context_snapshots", { sessionId }),

  deleteSession: (id: string) => invoke<boolean>("delete_session", { id }),

  /** Raw WAV bytes for playback. */
  getSessionAudio: (id: string) => invoke<number[]>("get_session_audio", { id }),

  retrySessionTranscription: (id: string) =>
    invoke<{
      session: SessionRecord;
      asrText: string;
      correctedText: string;
      asrEngine: string;
      correctorEngine: string;
      modelApplied: boolean;
      fallbackReason?: string | null;
    }>("retry_session_transcription", { id }),

  seedDemoSession: () => invoke<SessionRecord>("seed_demo_session"),

  saveSession: (input: {
    asrRaw?: string;
    corrected?: string;
    pasted?: string;
    focusedApp?: string;
    recordEditIfChanged?: boolean;
  }) => invoke<SessionRecord>("save_session", { input }),

  listEditEvents: (sessionId: string) =>
    invoke<EditEvent[]>("list_edit_events", { sessionId }),
  listEditObservations: (sessionId: string) =>
    invoke<EditObservation[]>("list_edit_observations", { sessionId }),
  getEditLearningObservability: () =>
    invoke<EditLearningObservability>("get_edit_learning_observability"),
  listEditLearningFeedback: (limit = 100) =>
    invoke<EditLearningFeedback[]>("list_edit_learning_feedback", { limit }),
  acknowledgeEditLearningFeedback: (noticeId: string) =>
    invoke<void>("acknowledge_edit_learning_feedback", { noticeId }),
  listEditLearningProposals: (editSessionId: string) =>
    invoke<LearningProposal[]>("list_edit_learning_proposals", { editSessionId }),
  decideEditLearningProposal: (proposalId: string, decision: "rejected") =>
    invoke<void>("decide_edit_learning_proposal", { proposalId, decision }),

  recordEditEvent: (input: {
    sessionId: string;
    beforeText: string;
    afterText: string;
    source?: string;
  }) => invoke<string>("record_edit_event", { input }),

  suggestFromEdit: (before: string, after: string) =>
    invoke<LearnCandidate[]>("suggest_from_edit", { before, after }),

  confirmLearn: (input: {
    kind: string;
    term?: string;
    fromText?: string;
    toText?: string;
    sessionId?: string;
    beforeText?: string;
    afterText?: string;
    proposalId?: string;
  }) => invoke<DictionaryEntry>("confirm_learn", { input }),

  listDictionary: () => invoke<DictionaryEntry[]>("list_dictionary"),

  addTerm: (term: string) =>
    invoke<DictionaryEntry>("add_dictionary_term", { input: { term } }),

  addReplacement: (fromText: string, toText: string) =>
    invoke<DictionaryEntry>("add_dictionary_replacement", {
      input: { fromText, toText },
    }),

  deleteDictionaryEntry: (id: string) =>
    invoke<void>("delete_dictionary_entry", { id }),

  // ---- Meeting mode (M4) ------------------------------------------------

  /** Start a new meeting recording; resolves with the new meeting id. Suspends
   * the dictation hotkey for the duration (M3 mode arbiter). */
  startMeetingRecording: (title?: string) =>
    invoke<string>("start_meeting_recording", { title: title ?? null }),

  /** Stop the active meeting recording; the meeting then advances
   * `processing → … → ready`/`failed` in the background (poll the list/detail). */
  stopMeetingRecording: (meetingId: string) =>
    invoke<MeetingRecordingResult>("stop_meeting_recording", { meetingId }),

  /** Import wav/mp3/m4a/mp4 into the meeting library and start processing.
   * Omit `path` to open a native file picker; drag-and-drop passes the dropped path. */
  importMeetingFile: (path?: string) =>
    invoke<string>("import_meeting_file", { path: path ?? null }),

  // Pause/resume are backend-ready but intentionally NOT surfaced by the minimal
  // inline recording bar (which is reconstructed from backend state on remount
  // and so cannot track a pause state); they are reserved for the full recording
  // window that reports true paused-excluded elapsed.
  /** Pause the active meeting recording (paused audio is dropped, no gap). */
  pauseMeetingRecording: () => invoke<void>("pause_meeting_recording"),

  /** Resume a paused meeting recording. */
  resumeMeetingRecording: () => invoke<void>("resume_meeting_recording"),

  /** List meetings newest first, optionally filtered by status token / title query. */
  listMeetings: (opts?: {
    status?: MeetingStatus;
    query?: string;
    limit?: number;
  }) =>
    invoke<Meeting[]>("list_meetings", {
      status: opts?.status ?? null,
      query: opts?.query ?? null,
      limit: opts?.limit ?? null,
    }),

  // ---- Meeting detection (opt-in, capability-gated) ---------------------

  /** Read the meeting-detection opt-in preference plus runtime
   * capability/active state (for the settings toggle). */
  getMeetingDetection: () =>
    invoke<MeetingDetectionStatus>("get_meeting_detection"),

  /** Toggle the opt-in preference; starts/stops the detector to match. */
  setMeetingDetectionEnabled: (enabled: boolean) =>
    invoke<MeetingDetectionStatus>("set_meeting_detection_enabled", { enabled }),

  /** Runtime meeting/recording application catalog. The returned path is the
   * user-owned TOML file; the executable contains no compiled app list. */
  getMeetingAppCatalog: () =>
    invoke<MeetingAppCatalog>("get_meeting_app_catalog"),

  saveMeetingAppCatalog: (catalog: MeetingAppCatalog) =>
    invoke<MeetingAppCatalog>("save_meeting_app_catalog", {
      catalog: {
        version: catalog.version,
        application: catalog.applications,
      },
    }),

  reloadMeetingAppCatalog: () =>
    invoke<MeetingAppCatalog>("reload_meeting_app_catalog"),

  /** User accepted a detection prompt → start recording via the existing path.
   * Resolves the new meeting id, or "" if nothing was started. */
  acceptMeetingDetection: (captureSystemAudio: boolean) =>
    invoke<string>("accept_meeting_detection", { captureSystemAudio }),

  /** User dismissed a detection prompt (arms a per-app cooldown). */
  dismissMeetingDetection: () =>
    invoke<void>("dismiss_meeting_detection"),

  /** User accepted the end-of-meeting stop suggestion → stop the
   * detection-started recording via the existing stop path. A stale click
   * (recording already ended some other way) resolves as a no-op. */
  acceptMeetingDetectionStop: () =>
    invoke<void>("accept_meeting_detection_stop"),

  /** User declined the stop suggestion ("继续录制"): keep recording; no
   * further suggestion is made for this recording. */
  declineMeetingDetectionStop: () =>
    invoke<void>("decline_meeting_detection_stop"),

  /** Read the local detection counters (all counting stays on this machine). */
  getMeetingDetectionStats: () =>
    invoke<MeetingDetectionStats>("get_meeting_detection_stats"),

  /** Read the meeting watchdog settings (silence auto-stop minutes +
   * calendar-end reminder) for the settings UI. */
  getMeetingWatchdogConfig: () =>
    invoke<MeetingWatchdogConfig>("get_meeting_watchdog_config"),

  /** Persist the meeting watchdog settings; takes effect for the next
   * recording. Resolves the stored values. */
  setMeetingWatchdogConfig: (input: {
    silenceAutoStopMinutes: number;
    calendarEndReminder: boolean;
  }) =>
    invoke<MeetingWatchdogConfig>("set_meeting_watchdog_config", input),

  /** Keep recording after a prolonged-silence warning and re-arm the full
   * configured silence interval from the acknowledgement point. */
  continueMeetingAfterSilence: (meetingId: string) =>
    invoke<void>("continue_meeting_after_silence", { meetingId }),

  /** Read one meeting with its speakers, seq-ordered segments, and summaries. */
  getMeetingDetail: (meetingId: string) =>
    invoke<MeetingDetail>("get_meeting_detail", { meetingId }),

  /** Overwrite the user's free-form notes for a meeting (last-write-wins;
   * debounce on the caller side). These notes are fused into the minutes LLM
   * pass as extra context. Resolves `true` if the meeting row was updated. */
  saveMeetingNotes: (meetingId: string, notes: string) =>
    invoke<boolean>("save_meeting_notes", { meetingId, notes }),

  /** Rename a meeting (edit its title). A blank title clears back to untitled
   * ("未命名会议"). Resolves `true` if the meeting row was updated. */
  renameMeeting: (meetingId: string, title: string) =>
    invoke<boolean>("rename_meeting", { meetingId, title }),

  /** Keep one continuous portion of a finished meeting, permanently replace
   * its WAV files, and start transcript/minutes regeneration. Resolves the
   * exact duration of the newly written mic WAV. */
  trimMeetingAudio: (
    meetingId: string,
    startSeconds: number,
    endSeconds: number,
  ) =>
    invoke<number>("trim_meeting_audio", {
      meetingId,
      startSeconds,
      endSeconds,
    }),

  /** Delete a meeting and all attached data (segments/speakers/summaries cascade)
   * plus its recorded WAV on disk. Irreversible. Resolves `true` if a row was
   * deleted. */
  deleteMeeting: (meetingId: string) =>
    invoke<boolean>("delete_meeting", { meetingId }),

  /** Render a meeting into one of the four export presets. */
  exportMeeting: (meetingId: string, preset: ExportPreset) =>
    invoke<ExportOutput>("export_meeting", { meetingId, preset }),

  // Speaker-correction commands (backend ready in M4a; the correction UI is
  // wired in M4c, these bindings are provided for that stage).
  /** Edit the text of one transcript segment (manual correction on the review
   * page). Only the words change; the segment's timing and speaker are left
   * untouched. Resolves `true` if the segment row was updated. */
  editMeetingSegment: (segmentId: string, text: string) =>
    invoke<boolean>("edit_meeting_segment", { segmentId, text }),

  renameSpeaker: (speakerId: string, displayName: string) =>
    invoke<boolean>("rename_speaker", { speakerId, displayName }),

  // ---- Live speaker annotations (L2) ------------------------------------
  // Recording-time "who is speaking" marks on live caption lines. Persisted
  // immediately; the offline pipeline reconciles them into speaker
  // attribution after stop (manual always wins).

  /** Annotate one live caption line with a speaker **boundary** at its start
   * time. `segmentId` is the transient live segment id (tracing only); the
   * persisted anchor is the unified-timeline boundary + track. Pass
   * `unassigned: true` for a "无" boundary (no manual speaker from here on) —
   * `displayName`/`identityId` are then ignored. Resolves the stored row. */
  annotateLiveSegment: (input: {
    meetingId: string;
    segmentId: string;
    startSeconds: number;
    endSeconds?: number | null;
    channel: "mic" | "system";
    identityId?: string | null;
    displayName: string;
    unassigned?: boolean;
  }) =>
    invoke<LiveAnnotation>("annotate_live_segment", {
      meetingId: input.meetingId,
      segmentId: input.segmentId,
      startSeconds: input.startSeconds,
      endSeconds: input.endSeconds ?? null,
      channel: input.channel,
      identityId: input.identityId ?? null,
      displayName: input.displayName,
      unassigned: input.unassigned ?? false,
    }),

  /** List a meeting's live annotations, oldest first (restores chip labels
   * after a remount mid-recording). */
  listLiveAnnotations: (meetingId: string) =>
    invoke<LiveAnnotation[]>("list_live_annotations", { meetingId }),

  /** Delete one live annotation (the chip's 清除 action). Resolves `true` if
   * a row was deleted. */
  deleteLiveAnnotation: (annotationId: string) =>
    invoke<boolean>("delete_live_annotation", { annotationId }),

  /** Rename a mistyped speaker name across a whole meeting (the chip's 重命名
   * action): every line marked `oldName` becomes `newName`. Resolves the number
   * of annotations updated. */
  renameLiveAnnotations: (meetingId: string, oldName: string, newName: string) =>
    invoke<number>("rename_live_annotations", { meetingId, oldName, newName }),

  /** Drain interrupted-recording recovery outcomes buffered at startup (the
   * live `meeting-recovery` event can fire before the listener is ready). */
  takeRecoveryNotices: () =>
    invoke<
      {
        meetingId: string;
        title?: string;
        outcome: string;
        durationSeconds?: number;
        reason?: string;
      }[]
    >("take_recovery_notices"),

  // ---- Speaker voiceprint enrollment (M5) -------------------------------
  // The identity library is local-only (JSON under the Lumen identity dir);
  // embeddings never leave the machine and never cross IPC.

  /** Enroll a confirmed speaker's voiceprint under their real name (defaults
   * to the speaker's display name; `name` overrides it). Later meetings will
   * auto-identify this person. */
  enrollSpeaker: (meetingId: string, speakerId: string, name?: string) =>
    invoke<import("./types").EnrolledSpeaker>("enroll_speaker", {
      meetingId,
      speakerId,
      name: name ?? null,
    }),

  /** List every enrolled identity (name-ordered). */
  listEnrolledSpeakers: () =>
    invoke<import("./types").EnrolledSpeaker[]>("list_enrolled_speakers"),

  /** Remove one enrolled identity; resolves `true` if it existed. Existing
   * meetings keep their names — only future auto-identification stops. */
  removeEnrolledSpeaker: (identityId: string) =>
    invoke<boolean>("remove_enrolled_speaker", { identityId }),

  /** Rename an enrolled identity (samples kept). Rejects renaming onto another
   * identity's name — use `mergeEnrolledSpeakers` for the same person. */
  renameEnrolledSpeaker: (identityId: string, name: string) =>
    invoke<import("./types").EnrolledSpeaker>("rename_enrolled_speaker", {
      identityId,
      name,
    }),

  /** Merge `fromId` into `intoId`: move all of `from`'s samples onto `into`,
   * then delete `from`. Resolves the surviving (merged) identity. */
  mergeEnrolledSpeakers: (fromId: string, intoId: string) =>
    invoke<import("./types").EnrolledSpeaker>("merge_enrolled_speakers", {
      fromId,
      intoId,
    }),

  /** Delete one voiceprint sample by its (oldest-first) index; removing the
   * last sample deletes the identity (then resolves null). */
  removeSpeakerSample: (identityId: string, sampleIndex: number) =>
    invoke<import("./types").EnrolledSpeaker | null>("remove_speaker_sample", {
      identityId,
      sampleIndex,
    }),

  /** Raw WAV bytes of one voiceprint sample's source recording, for playback.
   * Only call when the sample's `hasAudio` is true. */
  readVoiceprintSampleAudio: (identityId: string, sampleIndex: number) =>
    invoke<ArrayBuffer>("read_voiceprint_sample_audio", {
      identityId,
      sampleIndex,
    }),

  /** Unresolved auto-enroll conflicts (same voice labelled under a different
   * name than an already-enrolled person), newest first. */
  listEnrollConflicts: () =>
    invoke<import("./types").EnrollConflict[]>("list_enroll_conflicts"),

  /** Resolve one conflict (meeting/speaker come from the stored record):
   * `enrollAs` a name enrolls that speaker's voiceprint under it (the existing
   * name = "same person", the meeting's label = "a different person who sounds
   * alike"); `null` just dismisses it. */
  resolveEnrollConflict: (conflictId: string, enrollAs: string | null) =>
    invoke<void>("resolve_enroll_conflict", { conflictId, enrollAs }),

  /** Read the enrolled identity marked as the user themself ("这是我"), or
   * null. Rendering hint: attribution matching it displays as "我". */
  getSelfIdentity: () => invoke<string | null>("get_self_identity"),

  /** Set (identityId) or clear (null) which enrolled identity is the user
   * themself. Resolves the stored value. */
  setSelfIdentity: (identityId: string | null) =>
    invoke<string | null>("set_self_identity", { identityId }),

  /** Register the user's own voice ("我") from their recent dictation
   * recordings and mark that identity as self. `name` defaults to "我". */
  enrollSelfFromRecordings: (name?: string | null) =>
    invoke<import("./types").SelfEnrollResult>("enroll_self_from_recordings", {
      name: name ?? null,
    }),

  /** Retroactively re-identify a stored meeting's unnamed speakers against the
   * current voiceprint library (fills 说话人N, never overrides manual names). */
  reidentifyMeeting: (meetingId: string) =>
    invoke<import("./types").ReidentifyResult>("reidentify_meeting", {
      meetingId,
    }),

  /** Which of a meeting's speakers have a stored voiceprint (enrollable). */
  getMeetingVoiceprints: (meetingId: string) =>
    invoke<import("./types").SpeakerVoiceprint[]>("get_meeting_voiceprints", {
      meetingId,
    }),

  reassignSegmentSpeaker: (segmentId: string, speakerId: string) =>
    invoke<boolean>("reassign_segment_speaker", { segmentId, speakerId }),
  mergeSpeakers: (
    meetingId: string,
    fromSpeakerId: string,
    intoSpeakerId: string,
  ) =>
    invoke<number>("merge_speakers", {
      meetingId,
      fromSpeakerId,
      intoSpeakerId,
    }),
};
