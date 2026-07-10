import { invoke } from "@tauri-apps/api/core";
import type {
  AsrStatus,
  AudioDevice,
  CorrectorStatus,
  CorrectTextOutcome,
  DictionaryEntry,
  EditEvent,
  Health,
  LearnCandidate,
  SessionRecord,
  TranscribeOutcome,
} from "./types";

export const api = {
  health: () => invoke<Health>("app_health"),

  listAudioDevices: () => invoke<AudioDevice[]>("list_audio_devices"),
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
    provider?: string;
    baseUrl?: string;
    model?: string;
    apiKey?: string;
    timeoutSecs?: number;
  }) => invoke<CorrectorStatus>("save_corrector_config", { input }),
  correctText: (text: string) =>
    invoke<CorrectTextOutcome>("correct_text", { input: { text } }),

  getPermissionStatus: () =>
    invoke<{
      microphone: string;
      accessibility: string;
      canRecord: boolean;
      canInject: boolean;
      copyOnlyOk: boolean;
    }>("get_permission_status"),
  openMicrophoneSettings: () => invoke<void>("open_microphone_settings"),
  openAccessibilitySettings: () => invoke<void>("open_accessibility_settings"),
  requestMicrophoneAccess: () =>
    invoke<{
      microphone: string;
      accessibility: string;
      canRecord: boolean;
      canInject: boolean;
      copyOnlyOk: boolean;
    }>("request_microphone_access"),

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
    }>("get_hotkey_config"),
  saveHotkeyConfig: (input: {
    enabled?: boolean;
    toggle?: string;
    showCapsule?: boolean;
    mode?: string;
  }) =>
    invoke<{
      enabled: boolean;
      toggle: string;
      showCapsule: boolean;
      mode: string;
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

  deleteSession: (id: string) => invoke<boolean>("delete_session", { id }),

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
