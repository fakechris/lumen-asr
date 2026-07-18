export type Health = {
  app: string;
  version: string;
  data_dir: string;
  db_path: string;
  db_ok: boolean;
  session_count: number;
  dictionary_count: number;
  sensevoice_ready: boolean;
  qwen_ready: boolean;
  whisper_ready: boolean;
  active_asr_ready: boolean;
  active_asr_label: string;
  recording: boolean;
  corrector_enabled: boolean;
  corrector_label: string;
};

export type CorrectorStatus = {
  enabled: boolean;
  provider: string;
  baseUrl: string;
  model: string;
  hasApiKey: boolean;
  timeoutSecs: number;
  label: string;
  /** none | light | medium | strong */
  cleanup?: string;
  style?: string;
  casing?: string;
  punctuation?: string;
  polish?: string[];
  customEnabled?: boolean;
  customInstruction?: string;
};

export type CorrectTextOutcome = {
  text: string;
  modelApplied: boolean;
  correctorEngine: string;
};

export type AudioDevice = {
  name: string;
  is_default: boolean;
};

export type AsrStatus = {
  recording: boolean;
  engine: "sensevoice" | "qwen" | "whisper";
  /** Settings ASR provider id — same source of truth as 设置 → 语音识别 */
  provider?: string;
  providerLabel?: string;
  sensevoice: { kind: string; ready: boolean; model_dir: string };
  qwen: { kind: string; ready: boolean; model_dir: string };
  qwenRuntimePath: string;
  qwenRuntimeReady: boolean;
  qwenRuntimeChecking: boolean;
  whisper: { kind: string; ready: boolean; model_dir: string };
  activeReady: boolean;
};

export type TranscribeOutcome = {
  text: string;
  asrText: string;
  correctedText: string;
  modelApplied: boolean;
  asrEngine: string;
  correctorEngine: string;
  sampleRate: number;
  numSamples: number;
  durationMs: number;
  session: SessionRecord;
  watchPostPaste?: boolean;
  postPasteSeconds?: number;
};

export type ProcessEditResult = {
  editEventId?: string | null;
  candidates: LearnCandidate[];
  autoPromoted: DictionaryEntry[];
  message: string;
};

export type LearningConfig = {
  autoPromote: boolean;
  autoPromoteThreshold: number;
  postPasteCapture: boolean;
  postPasteSeconds: number;
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

export type PipelineIdentity = {
  schema_version: number;
  asr_provider: string;
  asr_engine: string;
  asr_model?: string | null;
  asr_model_revision?: string | null;
  corrector_provider: string;
  corrector_engine: string;
  corrector_model?: string | null;
  prompt_hash?: string | null;
  prompt_hash_algorithm?: string | null;
  temperature?: number | null;
  enhancement_mode: EnhancementMode;
};

export type EnhancementMode = "none" | "unknown";
export type AttemptStatus = "in_progress" | "completed" | "failed" | "unknown";
export type PipelineStage =
  | "capture"
  | "preprocess"
  | "asr"
  | "enhancement"
  | "corrector"
  | "insert"
  | "unknown";
export type PipelineIssueKind =
  | "fallback"
  | "clipboard_failure"
  | "injection_failure"
  | "unknown";

export type PipelineMetrics = {
  schema_version: number;
  audio_duration_ms: number;
  preprocess_ms: number;
  asr_ms: number;
  enhancement_ms: number;
  corrector_ms: number;
  insert_ms: number;
  total_ms: number;
  asr_rtf?: number | null;
  asr_worker_reused?: boolean | null;
  corrector_fallback: boolean;
  insert_succeeded: boolean;
  stage_issues: PipelineStageIssue[];
};

export type PipelineStageIssue = {
  stage: PipelineStage;
  kind: PipelineIssueKind;
  message: string;
};

export type DictationAttemptRecord = {
  id: string;
  session_id: string;
  attempt_ordinal: number;
  created_at: string;
  asr_raw?: string | null;
  asr_enhanced?: string | null;
  corrected?: string | null;
  inserted?: string | null;
  pipeline_identity: PipelineIdentity;
  pipeline_metrics: PipelineMetrics;
  status: AttemptStatus;
  failed_stage?: PipelineStage | null;
  failure_message?: string | null;
  supersedes_attempt_id?: string | null;
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

export type TabId =
  | "record"
  | "overview"
  | "history"
  | "dictionary"
  | "learn"
  | "settings";
