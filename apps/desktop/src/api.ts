import { invoke } from "@tauri-apps/api/core";
import type {
  DictionaryEntry,
  EditEvent,
  Health,
  LearnCandidate,
  SessionRecord,
} from "./types";

export const api = {
  health: () => invoke<Health>("app_health"),

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
