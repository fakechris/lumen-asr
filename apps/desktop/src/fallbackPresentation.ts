const FALLBACK_REASON_LABELS: Record<string, string> = {
  timeout: "AI 服务响应超时",
  http: "AI 服务网络请求失败",
  authentication: "AI 服务鉴权失败",
  rate_limited: "AI 服务请求过于频繁",
  provider_client_error: "AI 服务拒绝了请求",
  provider_server_error: "AI 服务暂时不可用",
  provider_rejected: "AI 服务未返回可用结果",
  malformed_response: "AI 服务返回格式异常",
  empty_output: "AI 服务返回了空结果",
  empty_after_sanitization: "修订结果清理后为空",
  context_protected_token_mismatch: "数字或编号保护检查未通过",
  context_safety_marker_mismatch: "关键动作或否定含义检查未通过",
  context_unicode_separator: "修订结果包含不安全换行符",
  context_empty_mismatch: "修订结果意外变为空文本",
  context_content_too_long: "修订文本超出安全长度",
  context_excessive_growth: "修订新增内容过多",
  context_excessive_shrink: "修订删减内容过多",
  context_low_overlap: "修订与原转写重合度过低",
  context_excessive_edit_distance: "修订改动幅度过大",
  context_integrity_rejected: "修订内容安全检查未通过",
  build_failed: "AI 修订器启动失败",
  model_not_applied: "AI 修订未能应用",
  other: "AI 修订发生未知错误",
};

export function correctorFallbackReasonLabel(reason?: string | null): string {
  if (!reason) return FALLBACK_REASON_LABELS.model_not_applied;
  return FALLBACK_REASON_LABELS[reason] || `AI 修订回退（${reason}）`;
}

export function correctorFallbackNotice(reason?: string | null): string {
  return `识别完成，校对未应用：${correctorFallbackReasonLabel(reason)}`;
}

export function dictationDoneNotice(opts: {
  fallbackReason?: string | null;
  insertNotice?: string | null;
}): string | null {
  const parts: string[] = [];
  if (opts.fallbackReason) parts.push(correctorFallbackNotice(opts.fallbackReason));
  if (opts.insertNotice?.trim()) parts.push(opts.insertNotice.trim());
  return parts.length > 0 ? parts.join(" ") : null;
}
