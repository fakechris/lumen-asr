import { invoke } from "@tauri-apps/api/core";
import type {
  AsrStatus,
  AudioDevice,
  CorrectorStatus,
  CorrectTextOutcome,
  ContextSnapshotRecord,
  DictationAttemptRecord,
  DictionaryEntry,
  EditEvent,
  EditObservation,
  Health,
  LearnCandidate,
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
  modelsRoot: string;
  activeEngine: string;
  activeModelDir: string;
  candidates: AsrModelCandidate[];
  downloadUrl: string;
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
  enabled: boolean;
};

export const api = {
  health: () => invoke<Health>("app_health"),

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

  getLearningConfig: () =>
    invoke<import("./types").LearningConfig>("get_learning_config"),
  saveLearningConfig: (input: {
    autoPromote?: boolean;
    autoPromoteThreshold?: number;
    postPasteCapture?: boolean;
    postPasteSeconds?: number;
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
};
