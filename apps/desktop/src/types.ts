export type Health = {
  app: string;
  version: string;
  data_dir: string;
  db_path: string;
  db_ok: boolean;
  session_count: number;
  dictionary_count: number;
};

export type FocusInfo = {
  app_name?: string | null;
  bundle_id?: string | null;
  window_title?: string | null;
};

export type SessionRecord = {
  id: string;
  created_at: string;
  focus: FocusInfo;
  asr_raw?: string | null;
  corrected?: string | null;
  pasted?: string | null;
  asr_engine?: string | null;
  corrector_engine?: string | null;
  insert_strategy: string;
  audio_path?: string | null;
  status: string;
};

export type EditEvent = {
  id: string;
  session_id: string;
  source: string;
  before_text: string;
  after_text: string;
  created_at: string;
};

export type DictionaryEntry = {
  id: string;
  kind: "term" | "replacement";
  term?: string | null;
  from_text?: string | null;
  to_text?: string | null;
  source: string;
  hit_count: number;
  confirmed: boolean;
  updated_at: string;
};

export type LearnCandidate = {
  kind: "term" | "replacement";
  term?: string | null;
  from_text?: string | null;
  to_text?: string | null;
  reason: string;
};

export type TabId = "overview" | "history" | "dictionary" | "learn";
