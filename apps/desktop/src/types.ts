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

export type BuildInfo = {
  version: string;
  git_sha: string;
  build_time: string;
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
  fallbackReason?: string | null;
  asrEngine: string;
  correctorEngine: string;
  sampleRate: number;
  numSamples: number;
  durationMs: number;
  session: SessionRecord;
  watchPostPaste?: boolean;
  postPasteSeconds?: number;
  insertNotice?: string | null;
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
  persistEditEvidenceText: boolean;
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
  | "absolute_silence"
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
  proposal_id?: string;
};

export type LearningProposal = {
  id: string;
  edit_session_id: string;
  revision_id: string;
  kind: "term" | "replacement" | string;
  payload_json: string;
  confidence: number;
  risk: string;
  status: string;
  policy_version: number;
  created_at: string;
};

export type EditLearningFeedback = {
  id: string;
  edit_session_id: string;
  kind: string;
  message: string;
  proposal_ids: string[];
  created_at: string;
  delivered_at?: string | null;
  acknowledged_at?: string | null;
};

export type EditLearningObservability = {
  active_sessions: number;
  reservations_started: number;
  reservations_succeeded: number;
  reservations_failed: number;
  sessions_started: number;
  sessions_failed_to_start: number;
  snapshots_observed: number;
  snapshots_unavailable: number;
  suspensions: number;
  recoveries: number;
  revisions_recorded: number;
  proposals_created: number;
  proposals_superseded: number;
  proposal_persistence_retries: number;
  feedback_enqueued: number;
  parent_persistence_retries: number;
  persistence_failures: number;
  sessions_evicted: number;
  same_surface_sessions_finalized: number;
  evidence_records_redacted: number;
  insertion_target_mismatches: number;
  surface_transition_timeouts: number;
  content_boundary_finalizations: number;
  snapshot_latency_ms_total: number;
  snapshot_latency_ms_max: number;
  poll_backoffs: number;
};

export type TabId =
  | "record"
  | "meeting"
  | "overview"
  | "history"
  | "dictionary"
  | "identity"
  | "learn"
  | "settings";

// ---- Meeting mode (M4) --------------------------------------------------
// TS mirrors of the lumen-core / lumen-meeting types the meeting_cmd Tauri
// commands return. Field names match the serde output (default snake_case for
// the domain structs; camelCase only where a DTO opts in).

/** Coarse lifecycle of a stored meeting (serde snake_case). */
export type MeetingStatus =
  | "recording"
  | "processing"
  | "transcribing"
  | "summarizing"
  | "ready"
  | "failed";

/** A meeting recording row (`list_meetings` returns `Meeting[]`). */
export type Meeting = {
  id: string;
  created_at: string;
  title?: string | null;
  audio_path?: string | null;
  duration_seconds?: number | null;
  status: MeetingStatus;
  language?: string | null;
  /** Why a `failed` meeting failed (missing diar models, unsupported platform,
   * …); absent on every non-failed meeting. */
  failure_reason?: string | null;
  /** Free-form notes the user took during the meeting (empty until written).
   * Fed to the minutes LLM pass as extra context. */
  notes: string;
};

/** Result of `stop_meeting_recording` (camelCase DTO). */
export type MeetingRecordingResult = {
  id: string;
  audioPath: string;
  durationSeconds: number;
  sampleRate: number;
  status: string;
};

/** A speaker cluster within one meeting. Unconfirmed while `display_name` is null. */
export type Speaker = {
  id: string;
  meeting_id: string;
  label: string;
  display_name?: string | null;
  embedding_ref?: string | null;
  /** Enrolled identity behind the name, when attribution came from the
   * voiceprint library (v13 provenance). */
  identity_id?: string | null;
  /** How the name was attributed: "manual" | "verification" |
   * "offline_diarization" (v13 provenance). */
  attribution_origin?: string | null;
  /** Verification match confidence, when attribution_origin is
   * "verification". */
  attribution_confidence?: number | null;
};

/** An identity enrolled in the local voiceprint library (camelCase DTO; the
 * embedding itself never crosses IPC). */
export type EnrolledSpeaker = {
  id: string;
  name: string;
  enrolledAt: string;
  sourceMeetingId?: string | null;
  /** Every voiceprint sample, oldest-first — the order `removeSpeakerSample`
   * indexes into. */
  samples: EnrolledSample[];
};

/** One voiceprint sample of an enrolled identity (embedding stays server-side). */
export type EnrolledSample = {
  enrolledAt: string;
  voicedMs: number;
  sourceMeetingId?: string | null;
  /** Short human label (e.g. what was said), for a recognizable list. */
  sourceLabel?: string | null;
  /** Whether the sample maps to a playable recording (via
   * `readVoiceprintSampleAudio`). */
  hasAudio: boolean;
};

/** Outcome of retroactively re-identifying a meeting against the voiceprint
 * library (回溯重认). */
export type ReidentifyResult = {
  updated: { label: string; name: string; score: number }[];
  examined: number;
};

/** Outcome of registering the user's own voice from dictation recordings. */
export type SelfEnrollResult = {
  identityId: string | null;
  name: string;
  enrolled: number;
  scanned: number;
  skipped: number;
};

/** A queued auto-enroll conflict: a meeting labelled `speakerId` as `labelName`,
 * but that voice matched the already-enrolled `existingName` (cosine `score`),
 * so the enrollment was withheld for the user to resolve. */
export type EnrollConflict = {
  id: string;
  meetingId: string;
  speakerId: string;
  labelName: string;
  existingName: string;
  score: number;
  createdAt: string;
};

/** A recording-time speaker **boundary** on a live caption line (serde
 * snake_case domain struct). Anchored to a precise time on the meeting's
 * unified timeline + capture track; opens a range that runs until the next
 * boundary on the same track. Reconciled into speaker attribution after stop
 * (segments are split at the boundary times). */
export type LiveAnnotation = {
  id: string;
  meeting_id: string;
  /** Boundary time on the unified timeline; the range runs until the next
   * boundary on this track. */
  start_seconds: number;
  /** Retained for provenance only; not a range end in the timeline model. */
  end_seconds?: number | null;
  channel: "mic" | "system";
  /** Enrolled identity id when picked from the library; null for typed names
   * and "无" boundaries. */
  identity_id?: string | null;
  /** Name snapshot at annotate time; empty for a "无" boundary. */
  display_name: string;
  /** A "无" boundary: from here on no manual speaker until the next boundary. */
  unassigned?: boolean;
  created_at: string;
};

/** Whether a meeting speaker has a stored voiceprint embedding (enrollable). */
export type SpeakerVoiceprint = {
  speakerId: string;
  hasEmbedding: boolean;
};

/** One transcript segment (aligned with the lumen-transcript.v1 shape). */
export type TranscriptSegment = {
  id: string;
  meeting_id: string;
  seq: number;
  start_seconds: number;
  end_seconds: number;
  text: string;
  speaker_id?: string | null;
  confidence?: number | null;
  /** Optional word-level timing; not rendered in M4b. */
  words?: unknown[] | null;
  /** Capture track: "mic" (我) / "system" (对方). Absent on legacy meetings. */
  channel?: "mic" | "system" | null;
};

export type SummaryKind = "summary" | "action_items" | "decisions";

/** A stored generated summary. For `kind === "summary"`, `content` is Minutes JSON. */
export type MeetingSummary = {
  id: string;
  meeting_id: string;
  kind: SummaryKind;
  content: string;
  created_at: string;
  model?: string | null;
};

/** Aggregate read-model returned by `get_meeting_detail`. */
export type MeetingDetail = {
  meeting: Meeting;
  speakers: Speaker[];
  segments: TranscriptSegment[];
  summaries: MeetingSummary[];
};

// ---- Structured minutes JSON (lumen-meeting::Minutes) --------------------

/** A time range (seconds from media start) grounding a minutes item. */
export type SourceRef = { start: number; end: number };

export type Decision = { text: string; source?: SourceRef | null };
export type ActionItem = {
  text: string;
  owner?: string | null;
  due?: string | null;
  source?: SourceRef | null;
};
export type DiscussionPoint = { topic: string; source?: SourceRef | null };
export type OpenQuestion = { text: string; source?: SourceRef | null };

/** The structured minutes document (parsed from the Summary row `content`). */
export type Minutes = {
  one_liner: string;
  decisions: Decision[];
  action_items: ActionItem[];
  discussion: DiscussionPoint[];
  open_questions: OpenQuestion[];
};

/** Rendered export payload from `export_meeting`. */
export type ExportOutput = { filename: string; content: string };

/** The four fixed export presets. */
export type ExportPreset =
  | "minutes_md"
  | "transcript_md"
  | "subtitles_srt"
  | "data_json";
