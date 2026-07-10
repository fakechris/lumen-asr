import { invoke } from "@tauri-apps/api/core";
import type {
  AsrStatus,
  AudioDevice,
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
