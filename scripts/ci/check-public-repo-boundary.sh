#!/usr/bin/env bash

set -euo pipefail
shopt -s nocasematch

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

treeish="${1:-}"
if [[ -n "$treeish" ]]; then
  treeish="$(git rev-parse "$treeish^{commit}")"
fi

violations=()

while IFS= read -r -d '' path; do
  case "$path" in
    .research/*|research/*|\
    notes/*|\
    .codex/*|.mcp.json|.playwright-mcp/*|.superpowers/*|\
    blind-review-*|*.tsbuildinfo|apps/desktop/dist/*|\
    docs/research/*|docs/*/research/*|\
    docs/internal/*|docs/private/*|docs/*/internal/*|docs/*/private/*)
      violations+=("$path")
      ;;
  esac

  case "$path" in
    apps/desktop/src-tauri/src/context_capture.rs|\
    apps/desktop/src-tauri/src/context_inference.rs|\
    apps/desktop/src-tauri/src/bin/lumen-asr-context-*|\
    */context_capture.*|*/context_inference.*)
      violations+=("$path")
      ;;
  esac

  case "$path" in
    crates/*bench*/*|*/benches/*|benchmarks/*|benchmark-results/*|*benchmark-results*)
      violations+=("$path")
      ;;
  esac

  case "$path" in
    *research*|*competitive*|*competitor*|*vendor*evaluation*|\
    *capability*selection*|*provider*comparison*|*provider*evaluation*|\
    *context*pipeline*|\
    *benchmark*|*竞品*|*调研*|*研究*|*评测*)
      violations+=("$path")
      ;;
  esac

  case "$path" in
    docs/*|notes/*|planning/*|plans/*|\
    *.md|*.mdx|*.txt|*.rst|*.adoc|*.html|*.htm|*.pdf|*.docx|*.org|*.tex)
      case "$path" in
        */planning/*|*/plans/*|planning/*|plans/*|\
        *implementation*plan*|*roadmap*|*milestone*|*evolution*|*strategy*)
          violations+=("$path")
          ;;
      esac
      ;;
  esac

  case "$path" in
    docs/*)
      case "$path" in
        docs/README.md|\
        docs/SHARED_MODELS_CONTRACT.md|\
        docs/product/*|\
        docs/ui/*|\
        docs/release/*|\
        docs/architecture/*|\
        docs/governance/*|\
        docs/images/*)
          ;;
        *)
          violations+=("$path")
          ;;
      esac

      case "$path" in
        *context*capture*|*context*inference*)
          violations+=("$path")
          ;;
      esac
      ;;
  esac

  case "$path" in
    .env|.env.*|*/.env|*/.env.*|\
    *.pem|*.key|*.p12|*.pfx|\
    id_rsa|id_rsa.*|*/id_rsa|*/id_rsa.*|\
    id_ed25519|id_ed25519.*|*/id_ed25519|*/id_ed25519.*|\
    *credentials*.json|*secrets*.json)
      violations+=("$path")
      ;;
  esac
done < <(
  if [[ -n "$treeish" ]]; then
    git ls-tree -r -z --name-only "$treeish"
  else
    git ls-files -z
  fi
)

if ((${#violations[@]} > 0)); then
  printf 'public repository boundary violation:\n' >&2
  printf '  - %s\n' "${violations[@]}" | LC_ALL=C sort -u >&2
  printf 'Move research to .research/docs/<topic>/ and remove prohibited code or artifacts.\n' >&2
  exit 1
fi

secret_pattern='-----BEGIN ([A-Z0-9 ]+ )?PRIVATE KEY-----|gh[pousr]_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{20,}|/Users/[A-Za-z0-9._-]+/|[A-Za-z]:\\Users\\[^\\ ]+\\|Apple Development:[^[:cntrl:]]+@|Apple Development:[^[:cntrl:]]*\([A-Z0-9]{10}\)|Personal Team[^[:cntrl:]]*\([A-Z0-9]{10}\)|TeamIdentifier[[:space:]]*[=:][[:space:]]*[A-Z0-9]{10}|Team ID[[:space:]]*[=:][[:space:]]*[A-Z0-9]{10}'
if [[ -n "$treeish" ]]; then
  secret_matches="$(git grep -I -n -E -e "$secret_pattern" "$treeish" -- . \
    ':(exclude)scripts/ci/check-public-repo-boundary.sh' \
    ':(exclude)scripts/ci/test-public-repo-boundary.sh' \
    ':(exclude)scripts/macos/create_local_codesign_identity.swift' || true)"
else
  secret_matches="$(git grep -I -n -E -e "$secret_pattern" -- . \
    ':(exclude)scripts/ci/check-public-repo-boundary.sh' \
    ':(exclude)scripts/ci/test-public-repo-boundary.sh' \
    ':(exclude)scripts/macos/create_local_codesign_identity.swift' || true)"
fi
if [[ -n "$secret_matches" ]]; then
  printf '%s\n' "$secret_matches"
  printf 'possible credential, personal identifier, or personal absolute path found in tracked content\n' >&2
  exit 1
fi

document_pathspecs=(
  '*.md' '*.mdx' '*.txt' '*.rst' '*.adoc' '*.html' '*.htm'
  '*.svg' '*.yaml' '*.yml' '*.json' '*.csv' '*.tsv' '*.org' '*.tex'
  '*.pdf' '*.docx'
  ':(exclude)AGENTS.md'
  ':(exclude).github/pull_request_template.md'
  ':(exclude)docs/README.md'
  ':(exclude)docs/governance/PUBLIC_REPOSITORY_BOUNDARY.md'
  ':(exclude)docs/product/README.md'
  ':(exclude)docs/ui/README.md'
)

prohibited_document_pattern='(ASR|speech)[^[:cntrl:]]{0,40}(provider|vendor|engine)[^[:cntrl:]]{0,40}(comparison|evaluation|matrix|rationale|recommendation)|(provider|vendor|engine)[^[:cntrl:]]{0,40}(comparison|evaluation|matrix)|provider scores?|capability matrix|competitive analysis|research findings|experiment protocol|benchmark|private reference|content CER|strict CER|word error rate|(^|[^[:alnum:]_])WER([^[:alnum:]_]|$)|wins[ /-]*ties[ /-]*losses|context (capture|inference)[^[:cntrl:]]{0,20}(pipeline|architecture)|capture_all_displays|send_to_corrector|implementation plan|internal roadmap|product roadmap|engineering roadmap|milestone sketch|future work|evolution plan|internal strategy|(供应商|引擎|模型)[^[:cntrl:]]{0,30}(对比|比较|选型|评估|矩阵)|供应商得分|能力矩阵|竞品分析|研究结论|实验方案|评测方法|私有参考集|字错率|词错率|上下文[^[:cntrl:]]{0,20}(采集|推理)[^[:cntrl:]]{0,20}(管线|架构|流程)|全显示器|实施计划|内部路线图|产品路线图|工程路线图|里程碑|后续工作|演进计划|内部策略'
if [[ -n "$treeish" ]]; then
  document_matches="$(git grep -I -i -n -E -e "$prohibited_document_pattern" "$treeish" -- \
    "${document_pathspecs[@]}" || true)"
else
  document_matches="$(git grep -I -i -n -E -e "$prohibited_document_pattern" -- \
    "${document_pathspecs[@]}" || true)"
fi
if [[ -n "$document_matches" ]]; then
  printf '%s\n' "$document_matches"
  printf 'prohibited research, Context pipeline, or benchmark material found in public documentation\n' >&2
  exit 1
fi

selection_subject_pattern='(ASR|speech)[^[:cntrl:]]{0,40}(provider|vendor|engine)[^[:cntrl:]]{0,20}selection|(provider|vendor|engine)[^[:cntrl:]]{0,20}selection|(供应商|引擎|模型)[^[:cntrl:]]{0,20}(选择|选型)'
selection_evidence_pattern='we (chose|selected)[^[:cntrl:]]{0,30}(provider|vendor|engine|model)|(provider|vendor|engine|model)[^[:cntrl:]]{0,40}(was chosen|was selected|outperformed|won)|(provider|vendor|engine|model)[^[:cntrl:]]{0,40}(test|evaluation|comparison) results|selection rationale|decision rationale|我们(选择|选定)[^[:cntrl:]]{0,20}(供应商|引擎|模型)|(供应商|引擎|模型)[^[:cntrl:]]{0,30}(测试结果|评估结果|对比结果)|选型理由|决策依据'
if [[ -n "$treeish" ]]; then
  selection_subject_files="$(git grep -I -i -l -E -e "$selection_subject_pattern" \
    "$treeish" -- "${document_pathspecs[@]}" || true)"
  selection_evidence_files="$(git grep -I -i -l -E -e "$selection_evidence_pattern" \
    "$treeish" -- "${document_pathspecs[@]}" || true)"
else
  selection_subject_files="$(git grep -I -i -l -E -e "$selection_subject_pattern" -- \
    "${document_pathspecs[@]}" || true)"
  selection_evidence_files="$(git grep -I -i -l -E -e "$selection_evidence_pattern" -- \
    "${document_pathspecs[@]}" || true)"
fi
selection_research_files="$(LC_ALL=C comm -12 \
  <(printf '%s\n' "$selection_subject_files" | sed '/^$/d' | LC_ALL=C sort -u) \
  <(printf '%s\n' "$selection_evidence_files" | sed '/^$/d' | LC_ALL=C sort -u))"
if [[ -n "$selection_research_files" ]]; then
  printf '%s\n' "$selection_research_files"
  printf 'provider selection research evidence found in public documentation\n' >&2
  exit 1
fi

sidecar_violations=()
while IFS= read -r -d '' sidecar; do
  path="${sidecar%.public.md}"
  case "$path" in
    docs/images/*.png|docs/images/*.PNG|\
    docs/images/*.jpg|docs/images/*.JPG|\
    docs/images/*.jpeg|docs/images/*.JPEG|\
    docs/images/*.webp|docs/images/*.WEBP|\
    docs/images/*.gif|docs/images/*.GIF)
      ;;
    *)
      sidecar_violations+=("$sidecar")
      continue
      ;;
  esac

  if [[ -n "$treeish" ]]; then
    image_type="$(git cat-file -t "${treeish}:${path}" 2>/dev/null || true)"
    sidecar_type="$(git cat-file -t "${treeish}:${sidecar}" 2>/dev/null || true)"
    if [[ "$image_type" != "blob" || "$sidecar_type" != "blob" ]]; then
      sidecar_violations+=("$sidecar")
      continue
    fi
    asset_blob="$(git rev-parse "${treeish}:${path}")"
    actual="$(git show "${treeish}:${sidecar}")"
  else
    if [[ ! -f "$path" || ! -f "$sidecar" ]]; then
      sidecar_violations+=("$sidecar")
      continue
    fi
    asset_blob="$(git hash-object "$path")"
    actual="$(<"$sidecar")"
  fi
  expected="$(printf '%s\n' \
    'asset-class: public-product-ui' \
    'data-class: synthetic' \
    'human-reviewed: true' \
    "asset-blob: ${asset_blob}")"
  if [[ "$actual" != "$expected" ]]; then
    expected="${expected/data-class: synthetic/data-class: public}"
    if [[ "$actual" != "$expected" ]]; then
      sidecar_violations+=("$sidecar")
    fi
  fi
done < <(
  if [[ -n "$treeish" ]]; then
    git ls-tree -r -z --name-only "$treeish" -- docs/images | \
      while IFS= read -r -d '' path; do
        [[ "$path" == *.public.md ]] && printf '%s\0' "$path"
      done
  else
    git ls-files -z -- docs/images | while IFS= read -r -d '' path; do
      [[ "$path" == *.public.md ]] && printf '%s\0' "$path"
    done
  fi
)
if ((${#sidecar_violations[@]} > 0)); then
  printf 'invalid public-image attestation:\n' >&2
  printf '  - %s\n' "${sidecar_violations[@]}" | LC_ALL=C sort -u >&2
  exit 1
fi

prohibited_implementation_pattern='Context(Capture|Inference)(State|Config)?|context[-_](capture|inference)|lumen[-_](context|bench)|private reference dataset'
if [[ -n "$treeish" ]]; then
  implementation_matches="$(git grep -I -i -n -E -e "$prohibited_implementation_pattern" "$treeish" -- \
    '*.rs' '*.toml' '*.tsx' '*.ts' '*.js' '*.mjs' '*.json' 'Cargo.lock' || true)"
else
  implementation_matches="$(git grep -I -i -n -E -e "$prohibited_implementation_pattern" -- \
    '*.rs' '*.toml' '*.tsx' '*.ts' '*.js' '*.mjs' '*.json' 'Cargo.lock' || true)"
fi
if [[ -n "$implementation_matches" ]]; then
  printf '%s\n' "$implementation_matches"
  printf 'prohibited Context pipeline or benchmark implementation found in tracked source\n' >&2
  exit 1
fi

printf 'public repository boundary: clean\n'
