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
  useCapturedContext: boolean;
  provider: string;
  baseUrl: string;
  model: string;
  hasApiKey: boolean;
  timeoutSecs: number;
  label: string;
  /** none | light | medium | strong */
  cleanup?: string;
  /** qwen | default */
  cleanupProfile?: string;
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
  dictionary_context_hash?: string | null;
  dictionary_context_hash_algorithm?: string | null;
  dictionary_term_count: number;
  dictionary_replacement_count: number;
  enhancement_mode: EnhancementMode;
};

export type EnhancementMode = "none" | "qwen_shadow" | "unknown";
export type InsertionOutcome =
  | "not_requested"
  | "copied"
  | "inserted"
  | "failed"
  | "unknown";
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
  | "input_unavailable"
  | "clipboard_failure"
  | "injection_failure"
  | "unknown";

export type QwenDecodeMode =
  | "greedy_only"
  | "official_fallback"
  | "unknown";

export type AsrTokenEvidence = {
  chunk_index: number;
  token_index: number;
  token_id: number;
  text: string;
  selected_logprob: number;
  entropy: number;
  top1_top2_margin: number;
};

export type QwenRuntimeMetrics = {
  schema_version: number;
  runtime_version?: string | null;
  decode_mode: QwenDecodeMode;
  diagnostics_complete: boolean;
  fallback_reason?: string | null;
  chunk_count?: number | null;
  audio_encode_count?: number | null;
  prompt_prefill_count?: number | null;
  generated_token_count?: number | null;
  max_new_tokens?: number | null;
  finish_reason?: string | null;
  token_evidence_truncated: boolean;
  audio_feature_ms?: number | null;
  prompt_prefill_ms?: number | null;
  greedy_decode_ms?: number | null;
  worker_total_ms?: number | null;
  mlx_peak_memory_bytes?: number | null;
  mlx_active_memory_bytes_before_cleanup?: number | null;
  mlx_active_memory_bytes_after_cleanup?: number | null;
  mlx_cache_memory_bytes_after_cleanup?: number | null;
  process_max_rss_bytes?: number | null;
  process_user_cpu_ms?: number | null;
  process_system_cpu_ms?: number | null;
};

export type QwenShadowStatus =
  | "disabled"
  | "completed"
  | "no_trigger"
  | "unavailable"
  | "failed"
  | "unknown";

export type QwenShadowScore = {
  sum_logprob?: number | null;
  mean_logprob?: number | null;
  min_token_logprob?: number | null;
};

export type QwenShadowCandidate = {
  surface: string;
  source: string;
  beam_rank?: number | null;
  score: QwenShadowScore;
  candidate_minus_current?: number | null;
  disposition: string;
};

export type QwenShadowSpan = {
  chunk_index: number;
  token_start: number;
  token_end: number;
  current_surface: string;
  detector_reasons: string[];
  current_score: QwenShadowScore;
  candidates: QwenShadowCandidate[];
};

export type QwenShadowDiagnostics = {
  schema_version: number;
  status: QwenShadowStatus;
  policy_version: string;
  chunk_count: number;
  triggered_span_count: number;
  candidate_count: number;
  proposal_count: number;
  cache_clone_count: number;
  decoder_step_count: number;
  shadow_total_ms?: number | null;
  detector_ms?: number | null;
  beam_ms?: number | null;
  verifier_ms?: number | null;
  user_output_changed: boolean;
  fallback_reason?: string | null;
  spans: QwenShadowSpan[];
};

export type AsrRuntimeDiagnostics = {
  worker_reused?: boolean | null;
  model?: string | null;
  model_revision?: string | null;
  token_evidence: AsrTokenEvidence[];
  qwen?: QwenRuntimeMetrics | null;
  qwen_shadow?: QwenShadowDiagnostics | null;
};

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
  insertion_outcome: InsertionOutcome;
  insert_succeeded: boolean;
  stage_issues: PipelineStageIssue[];
  asr_runtime?: AsrRuntimeDiagnostics | null;
};

export type PipelineStageIssue = {
  stage: PipelineStage;
  kind: PipelineIssueKind;
  message: string;
};

export type ContextInputRef = {
  capture_id: string;
  revision: number;
  snapshot_hash: string;
  context_schema_version: number;
  capture_profile: string;
  source_presence_bitmap: number;
  source_status_summary: string;
};

export type ContextStageUsage = {
  stage: PipelineStage;
  sources: string[];
  projection_schema_version: number;
  projection_path?: string | null;
  projection_hash?: string | null;
  projection_chars: number;
  captured: boolean;
  selected: boolean;
  consumed: boolean;
  sent: boolean;
  not_used_reason?: string | null;
};

export type PipelineInputs = {
  schema_version: number;
  context?: ContextInputRef | null;
  stage_usages: ContextStageUsage[];
};

export type ContextSnapshotRecord = {
  capture_id: string;
  session_id: string;
  revision: number;
  schema_version: number;
  profile: string;
  target_generation: number;
  started_at: string;
  frozen_at: string;
  completed_at?: string | null;
  manifest_path: string;
  source_presence_bitmap: number;
  source_status_json: string;
  sanitized_hash: string;
  encryption: string;
  status: string;
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
  pipeline_inputs: PipelineInputs;
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
  attribution: {
    schema_version: number;
    attempt_id?: string | null;
    target_app_name?: string | null;
    target_bundle_id?: string | null;
    observer?: string | null;
    target_fingerprint_hash?: string | null;
    field_before_hash?: string | null;
    field_after_hash?: string | null;
    status: string;
  };
};

export type EditObservation = {
  id: string;
  session_id: string;
  attempt_id: string;
  source: string;
  status: string;
  end_reason: string;
  target_app_name?: string | null;
  target_bundle_id?: string | null;
  target_fingerprint_hash?: string | null;
  inserted_text_hash: string;
  field_initial_hash?: string | null;
  field_final_hash?: string | null;
  normalized_edit_distance?: number | null;
  started_at: string;
  completed_at: string;
  edit_event_id?: string | null;
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
