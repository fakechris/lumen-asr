"""Persistent local Qwen3-ASR worker used by the Rust engine adapter."""

import argparse
import contextlib
import importlib.metadata
import json
import math
import re
import sys
import time
import traceback

try:
    import resource
except ImportError:
    resource = None


MAX_TOKEN_EVIDENCE = 2048
SUPPORTED_RUNTIME_VERSIONS = frozenset({"0.3.5"})
SHADOW_POLICY_VERSION = "qwen_shadow_v1"
SHADOW_ACCEPT_MARGIN = 0.02


def _empty_shadow_diagnostics(
    status: str,
    *,
    chunk_count: int,
    fallback_reason: str | None = None,
) -> dict:
    return {
        "schema_version": 1,
        "status": status,
        "policy_version": SHADOW_POLICY_VERSION,
        "chunk_count": chunk_count,
        "triggered_span_count": 0,
        "candidate_count": 0,
        "proposal_count": 0,
        "cache_clone_count": 0,
        "decoder_step_count": 0,
        "shadow_total_ms": None,
        "detector_ms": None,
        "beam_ms": None,
        "verifier_ms": None,
        "user_output_changed": False,
        "fallback_reason": fallback_reason,
        "spans": [],
    }


def _run_shadow_fail_soft(callback, *, chunk_count: int) -> dict:
    started = time.perf_counter()
    try:
        diagnostics = callback()
        diagnostics["user_output_changed"] = False
        return diagnostics
    except Exception:
        traceback.print_exc(file=sys.stderr)
        diagnostics = _empty_shadow_diagnostics(
            "failed",
            chunk_count=chunk_count,
            fallback_reason="shadow_runtime_error",
        )
        diagnostics["shadow_total_ms"] = _finite_or_none(
            (time.perf_counter() - started) * 1000
        )
        return diagnostics


def _select_shadow_spans(
    evidence: list[dict],
    *,
    max_spans: int,
    token_offset: int = 0,
) -> list[dict]:
    ranked = []
    for row in evidence:
        reasons = []
        priority = 0.0
        if float(row["selected_logprob"]) <= -1.5:
            reasons.append("low_logprob")
            priority += 4.0 + min(-float(row["selected_logprob"]), 8.0)
        if float(row["entropy"]) >= 2.5:
            reasons.append("high_entropy")
            priority += 3.0 + min(float(row["entropy"]), 8.0) / 8.0
        if float(row["top1_top2_margin"]) <= 0.2:
            reasons.append("low_margin")
            priority += 1.0

        text = str(row.get("text", ""))
        letters = "".join(character for character in text if character.isalpha())
        if (
            len(letters) >= 2
            and letters.isascii()
            and letters.upper() == letters
        ):
            reasons.append("uppercase_run")
            priority += 2.0
        if re.search(r"[\u3400-\u9fff]", text) and re.search(
            r"[A-Za-z]",
            text,
        ):
            reasons.append("mixed_script")
            priority += 2.5

        if not reasons:
            continue
        local_index = int(row["token_index"]) - token_offset
        if local_index < 0:
            continue
        ranked.append(
            {
                "token_start": local_index,
                "token_end": local_index + 1,
                "current_surface": text,
                "detector_reasons": reasons,
                "_priority": priority,
            }
        )

    selected = sorted(
        ranked,
        key=lambda span: (-span["_priority"], span["token_start"]),
    )[: max(0, max_spans)]
    # Process right-to-left: local beam scoring rewinds and mutates the shared
    # KV cache in place, so later spans must only depend on untouched history.
    selected.sort(key=lambda span: span["token_start"], reverse=True)
    for span in selected:
        span.pop("_priority", None)
    return selected


def _dictionary_candidates(
    terms: list[dict],
    hypotheses: list[dict],
) -> list[dict]:
    candidates = []
    seen = set()
    for term in terms:
        surface = str(term.get("surface", "")).strip()
        surface_key = _surface_key(surface)
        if not surface_key or surface_key in seen:
            continue
        beam_rank = next(
            (
                int(hypothesis["rank"])
                for hypothesis in hypotheses
                if (
                    _surface_key(hypothesis.get("text", ""))
                    == surface_key
                    or _surface_key(
                        hypothesis.get("text", "")
                    ).startswith(
                        surface_key + " "
                    )
                )
            ),
            None,
        )
        if beam_rank is None:
            continue
        seen.add(surface_key)
        candidates.append(
            {
                "surface": surface,
                "source": str(term.get("source", "")),
                "beam_rank": beam_rank,
            }
        )
    return candidates


def _surface_key(value) -> str:
    return " ".join(str(value).strip().casefold().split())


def _runtime_version() -> str | None:
    try:
        return importlib.metadata.version("mlx-qwen3-asr")
    except importlib.metadata.PackageNotFoundError:
        return None


def _finite_or_none(value: float) -> float | None:
    value = float(value)
    return value if math.isfinite(value) else None


def _sum_known(values) -> float | None:
    values = list(values)
    if any(value is None for value in values):
        return None
    return _finite_or_none(sum(values))


def _max_known(values) -> int | None:
    values = list(values)
    if not values or any(value is None for value in values):
        return None
    return max(values)


def _resource_usage() -> dict | None:
    if resource is None:
        return None

    usage = resource.getrusage(resource.RUSAGE_SELF)
    max_rss = int(usage.ru_maxrss)
    if sys.platform != "darwin":
        max_rss *= 1024
    return {
        "max_rss_bytes": max(max_rss, 0),
        "user_cpu_seconds": max(float(usage.ru_utime), 0.0),
        "system_cpu_seconds": max(float(usage.ru_stime), 0.0),
    }


def _resource_metrics(started: dict | None) -> dict:
    finished = _resource_usage()
    if started is None or finished is None:
        return {
            "process_max_rss_bytes": None,
            "process_user_cpu_ms": None,
            "process_system_cpu_ms": None,
        }

    return {
        "process_max_rss_bytes": finished["max_rss_bytes"],
        "process_user_cpu_ms": _finite_or_none(
            max(
                finished["user_cpu_seconds"] - started["user_cpu_seconds"],
                0.0,
            )
            * 1000
        ),
        "process_system_cpu_ms": _finite_or_none(
            max(
                finished["system_cpu_seconds"]
                - started["system_cpu_seconds"],
                0.0,
            )
            * 1000
        ),
    }


def _select_output_language(
    forced_language: str | None,
    chunk_languages: list[str],
) -> str:
    detected_language = forced_language or "unknown"
    for language in chunk_languages:
        if detected_language == "unknown":
            detected_language = language
    return detected_language


def _official_fallback(
    session,
    audio_path: str,
    language: str | None,
    runtime_version: str | None,
    fallback_reason: str,
) -> dict:
    started = time.perf_counter()
    resource_started = _resource_usage()
    result = session.transcribe(
        audio_path,
        language=language,
        verbose=False,
    )
    return {
        "text": getattr(result, "text", ""),
        "language": getattr(result, "language", None),
        "token_evidence": [],
        "qwen_metrics": {
            "schema_version": 1,
            "runtime_version": runtime_version,
            "decode_mode": "official_fallback",
            "diagnostics_complete": False,
            "fallback_reason": fallback_reason,
            "chunk_count": None,
            "audio_encode_count": None,
            "prompt_prefill_count": None,
            "generated_token_count": None,
            "max_new_tokens": None,
            "finish_reason": None,
            "token_evidence_truncated": False,
            "audio_feature_ms": None,
            "prompt_prefill_ms": None,
            "greedy_decode_ms": None,
            "worker_total_ms": _finite_or_none(
                (time.perf_counter() - started) * 1000
            ),
            "mlx_peak_memory_bytes": None,
            "mlx_active_memory_bytes_before_cleanup": None,
            "mlx_active_memory_bytes_after_cleanup": None,
            "mlx_cache_memory_bytes_after_cleanup": None,
            **_resource_metrics(resource_started),
        },
    }


class GreedyDiagnostics:
    """Official-compatible greedy transcription with token-level evidence."""

    def __init__(self, session, runtime_version: str | None) -> None:
        import mlx.core as mx
        from mlx_qwen3_asr.audio import SAMPLE_RATE, compute_features
        from mlx_qwen3_asr.chunking import split_audio_into_chunks
        from mlx_qwen3_asr.generate import (
            FINISH_REASON_EOS,
            FINISH_REASON_LENGTH,
            FINISH_REASON_REPETITION,
            GenerationConfig,
            detect_repetition,
            resolve_max_new_tokens,
        )
        from mlx_qwen3_asr.runtime_utils import supports_kwarg
        from mlx_qwen3_asr.tokenizer import (
            canonicalize_language,
            parse_asr_output,
        )
        from mlx_qwen3_asr.transcribe import (
            _aggregate_finish_reason,
            _clear_mlx_cache,
            _join_chunk_texts,
            _to_audio_np,
        )

        self.session = session
        self.runtime_version = runtime_version
        self.mx = mx
        self.sample_rate = SAMPLE_RATE
        self.compute_features = compute_features
        self.split_audio_into_chunks = split_audio_into_chunks
        self.finish_eos = FINISH_REASON_EOS
        self.finish_length = FINISH_REASON_LENGTH
        self.finish_repetition = FINISH_REASON_REPETITION
        self.GenerationConfig = GenerationConfig
        self.detect_repetition = detect_repetition
        self.resolve_max_new_tokens = resolve_max_new_tokens
        self.supports_kwarg = supports_kwarg
        self.canonicalize_language = canonicalize_language
        self.parse_asr_output = parse_asr_output
        self.aggregate_finish_reason = _aggregate_finish_reason
        self.clear_mlx_cache = _clear_mlx_cache
        self.join_chunk_texts = _join_chunk_texts
        self.to_audio_np = _to_audio_np

    def _eval_cache(self, cache) -> None:
        tensors = [value for value in cache.keys if value is not None]
        tensors += [value for value in cache.values if value is not None]
        if tensors:
            self.mx.eval(tensors)

    def _memory(self, name: str) -> int | None:
        getter = getattr(self.mx, name, None)
        return int(getter()) if callable(getter) else None

    def _sync(self) -> None:
        synchronize = getattr(self.mx, "synchronize", None)
        if callable(synchronize):
            synchronize()

    def recover_after_failure(self) -> None:
        self._sync()
        self.clear_mlx_cache()

    def _token_and_evidence(
        self,
        logits,
        *,
        chunk_index: int,
        token_index: int,
    ) -> tuple[int, dict | None]:
        step_logits = logits.reshape(-1).astype(self.mx.float32)
        selected_tensor = self.mx.argmax(step_logits)
        logprobs = step_logits - self.mx.logsumexp(step_logits)
        selected_logprob_tensor = logprobs[selected_tensor]
        probabilities = self.mx.exp(logprobs)
        entropy_terms = self.mx.where(
            probabilities > 0,
            probabilities * logprobs,
            self.mx.zeros_like(logprobs),
        )
        entropy_tensor = -self.mx.sum(entropy_terms)

        size = int(step_logits.size)
        margin = 0.0
        if size >= 2:
            partition = self.mx.argpartition(step_logits, size - 2)
            top_values = self.mx.take(step_logits, partition[-2:])
            self.mx.eval(
                selected_tensor,
                selected_logprob_tensor,
                entropy_tensor,
                top_values,
            )
            ordered = sorted(
                (float(value) for value in top_values.tolist()),
                reverse=True,
            )
            margin = _finite_or_none(ordered[0] - ordered[1])
        else:
            self.mx.eval(
                selected_tensor,
                selected_logprob_tensor,
                entropy_tensor,
            )

        selected = int(selected_tensor.item())
        selected_logprob = _finite_or_none(selected_logprob_tensor.item())
        entropy = _finite_or_none(entropy_tensor.item())

        if selected_logprob is None or entropy is None or margin is None:
            return selected, None

        return selected, {
            "chunk_index": chunk_index,
            "token_index": token_index,
            "token_id": selected,
            "text": self.session.tokenizer.decode([selected]),
            "selected_logprob": selected_logprob,
            "entropy": entropy,
            "top1_top2_margin": margin,
        }

    def _position_ids_for(self, index: int, dtype):
        position = self.mx.array([[index]], dtype=dtype)
        return self.mx.stack([position, position, position], axis=1)

    def _token_logprob(self, logits, token_id: int) -> float:
        step_logits = logits.reshape(-1).astype(self.mx.float32)
        value = step_logits[token_id] - self.mx.logsumexp(step_logits)
        self.mx.eval(value)
        result = _finite_or_none(value.item())
        if result is None:
            raise ValueError("non-finite shadow token log probability")
        return result

    def _top_tokens(self, logits, *, count: int) -> list[dict]:
        step_logits = logits.reshape(-1).astype(self.mx.float32)
        logprobs = step_logits - self.mx.logsumexp(step_logits)
        size = int(step_logits.size)
        count = min(max(count, 1), size)
        indices = self.mx.argpartition(step_logits, size - count)[-count:]
        values = self.mx.take(logprobs, indices)
        self.mx.eval(indices, values)
        rows = [
            {
                "token_id": int(token_id),
                "text": self.session.tokenizer.decode([int(token_id)]),
                "logprob": _finite_or_none(logprob),
            }
            for token_id, logprob in zip(
                indices.tolist(),
                values.tolist(),
                strict=True,
            )
        ]
        if any(row["logprob"] is None for row in rows):
            raise ValueError("non-finite shadow beam log probability")
        return sorted(
            rows,
            key=lambda row: (-row["logprob"], row["token_id"]),
        )

    def _reconstruct_span_start_logits(
        self,
        *,
        cache,
        prompt_len: int,
        token_start: int,
        generated_token_ids: list[int],
        prefill_logits,
        position_dtype,
        unchecked_step: dict,
    ):
        checkpoint_offset = prompt_len + token_start
        if token_start == 0:
            cache.offset = prompt_len
            return prefill_logits

        cache.offset = checkpoint_offset - 1
        previous_token = generated_token_ids[token_start - 1]
        logits = self.session.model.step(
            input_ids=self.mx.array(
                [[previous_token]],
                dtype=self.mx.int32,
            ),
            position_ids=self._position_ids_for(
                checkpoint_offset - 1,
                position_dtype,
            ),
            cache=cache,
            **unchecked_step,
        )
        self.mx.eval(logits)
        self._eval_cache(cache)
        return logits

    def _score_surface(
        self,
        *,
        cache,
        checkpoint_offset: int,
        span_start_logits,
        token_ids: list[int],
        position_dtype,
        unchecked_step: dict,
    ) -> tuple[dict, int]:
        if not token_ids:
            raise ValueError("shadow surface token list must not be empty")

        first_logprob = self._token_logprob(
            span_start_logits,
            token_ids[0],
        )
        cache.offset = checkpoint_offset
        scores = [first_logprob]
        step_count = 0
        for relative_index, token_id in enumerate(token_ids[:-1]):
            logits = self.session.model.step(
                input_ids=self.mx.array(
                    [[token_id]],
                    dtype=self.mx.int32,
                ),
                position_ids=self._position_ids_for(
                    checkpoint_offset + relative_index,
                    position_dtype,
                ),
                cache=cache,
                **unchecked_step,
            )
            step_count += 1
            self.mx.eval(logits)
            self._eval_cache(cache)
            scores.append(
                self._token_logprob(
                    logits,
                    token_ids[relative_index + 1],
                )
            )

        total = math.fsum(scores)
        return (
            {
                "sum_logprob": _finite_or_none(total),
                "mean_logprob": _finite_or_none(total / len(scores)),
                "min_token_logprob": _finite_or_none(min(scores)),
            },
            step_count,
        )

    def _sequential_local_beam(
        self,
        *,
        cache,
        checkpoint_offset: int,
        span_start_logits,
        position_dtype,
        unchecked_step: dict,
        beam_width: int,
        beam_depth: int,
    ) -> tuple[list[dict], int]:
        active = [
            {
                "token_ids": [],
                "token_logprobs": [],
                "sum_logprob": 0.0,
            }
        ]
        step_count = 0

        for _ in range(beam_depth):
            expansions = []
            for hypothesis in active:
                cache.offset = checkpoint_offset
                logits = span_start_logits
                for relative_index, token_id in enumerate(
                    hypothesis["token_ids"]
                ):
                    logits = self.session.model.step(
                        input_ids=self.mx.array(
                            [[token_id]],
                            dtype=self.mx.int32,
                        ),
                        position_ids=self._position_ids_for(
                            checkpoint_offset + relative_index,
                            position_dtype,
                        ),
                        cache=cache,
                        **unchecked_step,
                    )
                    step_count += 1
                    self.mx.eval(logits)
                    self._eval_cache(cache)

                for item in self._top_tokens(
                    logits,
                    count=max(beam_width * 2, beam_width),
                ):
                    token_logprob = float(item["logprob"])
                    expansions.append(
                        {
                            "token_ids": [
                                *hypothesis["token_ids"],
                                int(item["token_id"]),
                            ],
                            "token_logprobs": [
                                *hypothesis["token_logprobs"],
                                token_logprob,
                            ],
                            "sum_logprob": (
                                hypothesis["sum_logprob"] + token_logprob
                            ),
                        }
                    )

            unique = {}
            for item in expansions:
                key = tuple(item["token_ids"])
                previous = unique.get(key)
                if (
                    previous is None
                    or item["sum_logprob"] > previous["sum_logprob"]
                ):
                    unique[key] = item
            active = sorted(
                unique.values(),
                key=lambda item: (
                    -item["sum_logprob"],
                    item["token_ids"],
                ),
            )[:beam_width]

        hypotheses = []
        for rank, item in enumerate(active, start=1):
            hypotheses.append(
                {
                    "rank": rank,
                    "token_ids": item["token_ids"],
                    "text": self.session.tokenizer.decode(
                        item["token_ids"]
                    ),
                    "sum_logprob": _finite_or_none(item["sum_logprob"]),
                    "mean_logprob": _finite_or_none(
                        item["sum_logprob"] / len(item["token_ids"])
                    ),
                    "min_token_logprob": _finite_or_none(
                        min(item["token_logprobs"])
                    ),
                }
            )
        return hypotheses, step_count

    def _shadow_chunk(
        self,
        *,
        chunk_index: int,
        token_offset: int,
        visible_tokens: list[int],
        evidence: list[dict],
        cache,
        prompt_len: int,
        prefill_logits,
        position_dtype,
        unchecked_step: dict,
        request: dict,
    ) -> dict:
        shadow_started = time.perf_counter()
        detector_started = time.perf_counter()
        max_spans = min(max(int(request.get("max_spans_per_chunk", 2)), 1), 2)
        beam_width = min(max(int(request.get("beam_width", 4)), 1), 4)
        beam_depth = min(max(int(request.get("beam_depth", 4)), 1), 4)
        spans = _select_shadow_spans(
            evidence,
            max_spans=max_spans,
            token_offset=token_offset,
        )
        detector_ms = (time.perf_counter() - detector_started) * 1000
        if not spans:
            diagnostics = _empty_shadow_diagnostics(
                "no_trigger",
                chunk_count=1,
            )
            diagnostics["detector_ms"] = _finite_or_none(detector_ms)
            diagnostics["shadow_total_ms"] = _finite_or_none(
                (time.perf_counter() - shadow_started) * 1000
            )
            return diagnostics

        terms = request.get("terms", [])
        result_spans = []
        candidate_count = 0
        proposal_count = 0
        decoder_step_count = 0
        beam_ms = 0.0
        verifier_ms = 0.0

        for span in spans:
            token_start = span["token_start"]
            token_end = min(token_start + 1, len(visible_tokens))
            if token_start >= token_end:
                continue
            current_token_ids = visible_tokens[token_start:token_end]
            current_surface = self.session.tokenizer.decode(
                current_token_ids
            )
            checkpoint_offset = prompt_len + token_start
            span_start_logits = self._reconstruct_span_start_logits(
                cache=cache,
                prompt_len=prompt_len,
                token_start=token_start,
                generated_token_ids=visible_tokens,
                prefill_logits=prefill_logits,
                position_dtype=position_dtype,
                unchecked_step=unchecked_step,
            )

            beam_started = time.perf_counter()
            hypotheses, beam_steps = self._sequential_local_beam(
                cache=cache,
                checkpoint_offset=checkpoint_offset,
                span_start_logits=span_start_logits,
                position_dtype=position_dtype,
                unchecked_step=unchecked_step,
                beam_width=beam_width,
                beam_depth=beam_depth,
            )
            beam_ms += (time.perf_counter() - beam_started) * 1000
            decoder_step_count += beam_steps

            verifier_started = time.perf_counter()
            current_score, current_steps = self._score_surface(
                cache=cache,
                checkpoint_offset=checkpoint_offset,
                span_start_logits=span_start_logits,
                token_ids=current_token_ids,
                position_dtype=position_dtype,
                unchecked_step=unchecked_step,
            )
            decoder_step_count += current_steps
            candidates = []
            current_surface_key = _surface_key(current_surface)
            beam_surfaces = {current_surface_key}

            for hypothesis in hypotheses:
                surface = hypothesis["text"]
                surface_key = _surface_key(surface)
                if not surface_key or surface_key in beam_surfaces:
                    continue
                beam_surfaces.add(surface_key)
                candidates.append(
                    {
                        "surface": surface,
                        "source": "qwen_local_beam",
                        "beam_rank": hypothesis["rank"],
                        "score": {
                            "sum_logprob": hypothesis["sum_logprob"],
                            "mean_logprob": hypothesis["mean_logprob"],
                            "min_token_logprob": hypothesis[
                                "min_token_logprob"
                            ],
                        },
                        "candidate_minus_current": None,
                        "disposition": "beam_only",
                    }
                )

            dictionary_candidates = _dictionary_candidates(
                terms,
                hypotheses,
            )[:beam_width]
            dictionary_surfaces = {current_surface_key}
            for candidate in dictionary_candidates:
                surface = candidate["surface"]
                surface_key = _surface_key(surface)
                if surface_key in dictionary_surfaces:
                    continue
                token_ids = [
                    int(token)
                    for token in self.session.tokenizer.encode(surface)
                ]
                if (
                    not token_ids
                    or self.session.tokenizer.decode(token_ids) != surface
                ):
                    continue
                score, score_steps = self._score_surface(
                    cache=cache,
                    checkpoint_offset=checkpoint_offset,
                    span_start_logits=span_start_logits,
                    token_ids=token_ids,
                    position_dtype=position_dtype,
                    unchecked_step=unchecked_step,
                )
                decoder_step_count += score_steps
                difference = (
                    float(score["mean_logprob"])
                    - float(current_score["mean_logprob"])
                )
                disposition = (
                    "proposal"
                    if difference >= SHADOW_ACCEPT_MARGIN
                    else "rejected"
                )
                dictionary_surfaces.add(surface_key)
                candidates.append(
                    {
                        **candidate,
                        "score": score,
                        "candidate_minus_current": _finite_or_none(
                            difference
                        ),
                        "disposition": disposition,
                    }
                )

            verifier_ms += (time.perf_counter() - verifier_started) * 1000
            candidate_count += len(candidates)
            proposal_count += sum(
                candidate["disposition"] == "proposal"
                for candidate in candidates
            )
            result_spans.append(
                {
                    "chunk_index": chunk_index,
                    "token_start": token_offset + token_start,
                    "token_end": token_offset + token_end,
                    "current_surface": current_surface,
                    "detector_reasons": span["detector_reasons"],
                    "current_score": current_score,
                    "candidates": candidates,
                }
            )

        return {
            "schema_version": 1,
            "status": "completed" if result_spans else "no_trigger",
            "policy_version": SHADOW_POLICY_VERSION,
            "chunk_count": 1,
            "triggered_span_count": len(result_spans),
            "candidate_count": candidate_count,
            "proposal_count": proposal_count,
            "cache_clone_count": 0,
            "decoder_step_count": decoder_step_count,
            "shadow_total_ms": _finite_or_none(
                (time.perf_counter() - shadow_started) * 1000
            ),
            "detector_ms": _finite_or_none(detector_ms),
            "beam_ms": _finite_or_none(beam_ms),
            "verifier_ms": _finite_or_none(verifier_ms),
            "user_output_changed": False,
            "fallback_reason": None,
            "spans": result_spans,
        }

    def _finish_reason(self, generated: list[int], config) -> str:
        if generated and generated[-1] in config.eos_token_ids:
            return self.finish_eos
        if len(generated) >= config.max_new_tokens:
            return self.finish_length
        if self.detect_repetition(generated):
            return self.finish_repetition
        raise AssertionError("greedy decode stopped without a finish reason")

    def _decode_chunk(
        self,
        chunk_audio,
        *,
        chunk_index: int,
        token_offset: int,
        language: str | None,
        shadow: dict | None,
    ) -> dict:
        chunk_started = time.perf_counter()
        duration_seconds = len(chunk_audio) / self.sample_rate
        max_new_tokens = self.resolve_max_new_tokens(
            None,
            audio_duration_sec=duration_seconds,
        )
        config = self.GenerationConfig(
            max_new_tokens=max_new_tokens,
            temperature=0.0,
        )

        feature_started = time.perf_counter()
        mel, feature_lens = self.compute_features(chunk_audio)
        audio_features, _ = self.session.model.audio_tower(
            mel.astype(self.session.dtype),
            feature_lens,
        )
        self.mx.eval(audio_features)
        audio_feature_ms = (time.perf_counter() - feature_started) * 1000

        prompt = self.session.tokenizer.build_prompt_tokens(
            n_audio_tokens=int(audio_features.shape[1]),
            language=language,
            context="",
        )
        input_ids = self.mx.array([prompt])
        seq_len = int(input_ids.shape[1])
        positions = self.mx.arange(seq_len)[None, :]
        position_ids = self.mx.stack([positions, positions, positions], axis=1)
        cache = self.session.model.create_cache(
            max_seq_len=seq_len + max_new_tokens
        )

        prefill_started = time.perf_counter()
        logits = self.session.model.prefill(
            input_ids=input_ids,
            audio_features=audio_features,
            position_ids=position_ids,
            cache=cache,
        )
        self.mx.eval(logits)
        self._eval_cache(cache)
        want_shadow = shadow is not None and bool(
            shadow.get("enabled", False)
        )
        prefill_logits = logits if want_shadow else None
        prompt_prefill_ms = (time.perf_counter() - prefill_started) * 1000

        next_pos_base = self.mx.arange(
            seq_len,
            seq_len + max(max_new_tokens - 1, 0),
            dtype=position_ids.dtype,
        )
        next_positions = self.mx.stack(
            [next_pos_base, next_pos_base, next_pos_base],
            axis=0,
        )[None, :, :]
        unchecked_step = (
            {"validate_input_ids": False}
            if self.supports_kwarg(
                getattr(self.session.model, "step", None),
                "validate_input_ids",
            )
            else {}
        )

        generated: list[int] = []
        evidence: list[dict] = []
        evidence_complete = True
        greedy_started = time.perf_counter()
        for step in range(max_new_tokens):
            token, row = self._token_and_evidence(
                logits,
                chunk_index=chunk_index,
                token_index=token_offset + len(generated),
            )
            generated.append(token)
            if token not in config.eos_token_ids:
                if row is None:
                    evidence_complete = False
                else:
                    evidence.append(row)

            if token in config.eos_token_ids:
                break
            if self.detect_repetition(generated):
                break
            if len(generated) >= max_new_tokens:
                break

            logits = self.session.model.step(
                input_ids=self.mx.array([[token]]),
                position_ids=next_positions[:, :, step : step + 1],
                cache=cache,
                **unchecked_step,
            )
            self.mx.eval(logits)
            self._eval_cache(cache)

        greedy_decode_ms = (time.perf_counter() - greedy_started) * 1000
        finish_reason = self._finish_reason(generated, config)
        visible_tokens = (
            generated[:-1]
            if generated and generated[-1] in config.eos_token_ids
            else generated
        )
        raw_text = self.session.tokenizer.decode(visible_tokens)
        detected_language, text = self.parse_asr_output(
            raw_text,
            user_language=language,
        )
        detected_language = (
            self.canonicalize_language(detected_language)
            or detected_language
        )
        shadow_diagnostics = None
        if shadow is not None:
            if not want_shadow:
                shadow_diagnostics = _empty_shadow_diagnostics(
                    "disabled",
                    chunk_count=1,
                )
            else:
                shadow_diagnostics = _run_shadow_fail_soft(
                    lambda: self._shadow_chunk(
                        chunk_index=chunk_index,
                        token_offset=token_offset,
                        visible_tokens=visible_tokens,
                        evidence=evidence,
                        cache=cache,
                        prompt_len=seq_len,
                        prefill_logits=prefill_logits,
                        position_dtype=position_ids.dtype,
                        unchecked_step=unchecked_step,
                        request=shadow,
                    ),
                    chunk_count=1,
                )
        active_before_cleanup = self._memory("get_active_memory")

        del mel, feature_lens, audio_features, input_ids
        del positions, position_ids, next_pos_base, next_positions
        del logits, prefill_logits, cache
        self._sync()
        self.clear_mlx_cache()

        return {
            "text": text,
            "language": detected_language,
            "tokens": visible_tokens,
            "evidence": evidence,
            "evidence_complete": evidence_complete,
            "finish_reason": finish_reason,
            "max_new_tokens": max_new_tokens,
            "audio_feature_ms": audio_feature_ms,
            "prompt_prefill_ms": prompt_prefill_ms,
            "greedy_decode_ms": greedy_decode_ms,
            "active_before_cleanup": active_before_cleanup,
            "active_after_cleanup": self._memory("get_active_memory"),
            "cache_after_cleanup": self._memory("get_cache_memory"),
            "total_ms": (time.perf_counter() - chunk_started) * 1000,
            "qwen_shadow": shadow_diagnostics,
        }

    def transcribe(
        self,
        audio_path: str,
        language: str | None,
        shadow: dict | None = None,
    ) -> dict:
        started = time.perf_counter()
        resource_started = _resource_usage()
        reset_peak = getattr(self.mx, "reset_peak_memory", None)
        if callable(reset_peak):
            reset_peak()

        audio = self.to_audio_np(audio_path)
        chunks = self.split_audio_into_chunks(audio, sr=self.sample_rate)
        forced_language = self.canonicalize_language(language)

        chunk_results: list[dict] = []
        token_offset = 0
        for chunk_index, (chunk_audio, _) in enumerate(chunks):
            result = self._decode_chunk(
                chunk_audio,
                chunk_index=chunk_index,
                token_offset=token_offset,
                language=forced_language,
                shadow=shadow,
            )
            chunk_results.append(result)
            token_offset += len(result["tokens"])

        if not chunk_results:
            raise AssertionError("audio chunker returned no chunks")

        final_language = _select_output_language(
            forced_language,
            [result["language"] for result in chunk_results],
        )
        final_language = (
            self.canonicalize_language(final_language)
            or final_language
        )
        texts = [result["text"] for result in chunk_results]
        text = self.join_chunk_texts(texts, final_language)
        finish_reason = self.aggregate_finish_reason(
            [result["finish_reason"] for result in chunk_results]
        )

        all_evidence = [
            row
            for result in chunk_results
            for row in result["evidence"]
        ]
        evidence_truncated = len(all_evidence) > MAX_TOKEN_EVIDENCE
        all_evidence = all_evidence[:MAX_TOKEN_EVIDENCE]
        diagnostics_complete = all(
            result["evidence_complete"] for result in chunk_results
        )
        shadow_diagnostics = None
        if shadow is not None:
            per_chunk_shadow = [
                result["qwen_shadow"] for result in chunk_results
            ]
            failed = next(
                (
                    result
                    for result in per_chunk_shadow
                    if result["status"] == "failed"
                ),
                None,
            )
            triggered_span_count = sum(
                result["triggered_span_count"]
                for result in per_chunk_shadow
            )
            if failed is not None:
                shadow_status = "failed"
            elif not bool(shadow.get("enabled", False)):
                shadow_status = "disabled"
            elif triggered_span_count:
                shadow_status = "completed"
            else:
                shadow_status = "no_trigger"
            shadow_diagnostics = {
                "schema_version": 1,
                "status": shadow_status,
                "policy_version": SHADOW_POLICY_VERSION,
                "chunk_count": len(chunk_results),
                "triggered_span_count": triggered_span_count,
                "candidate_count": sum(
                    result["candidate_count"]
                    for result in per_chunk_shadow
                ),
                "proposal_count": sum(
                    result["proposal_count"]
                    for result in per_chunk_shadow
                ),
                "cache_clone_count": sum(
                    result["cache_clone_count"]
                    for result in per_chunk_shadow
                ),
                "decoder_step_count": sum(
                    result["decoder_step_count"]
                    for result in per_chunk_shadow
                ),
                "shadow_total_ms": _sum_known(
                    result["shadow_total_ms"]
                    for result in per_chunk_shadow
                ),
                "detector_ms": _sum_known(
                    result["detector_ms"]
                    for result in per_chunk_shadow
                ),
                "beam_ms": _sum_known(
                    result["beam_ms"]
                    for result in per_chunk_shadow
                ),
                "verifier_ms": _sum_known(
                    result["verifier_ms"]
                    for result in per_chunk_shadow
                ),
                "user_output_changed": False,
                "fallback_reason": (
                    failed["fallback_reason"]
                    if failed is not None
                    else None
                ),
                "spans": [
                    span
                    for result in per_chunk_shadow
                    for span in result["spans"]
                ],
            }

        output = {
            "text": text,
            "language": final_language,
            "token_evidence": all_evidence,
            "qwen_metrics": {
                "schema_version": 1,
                "runtime_version": self.runtime_version,
                "decode_mode": "greedy_only",
                "diagnostics_complete": diagnostics_complete,
                "fallback_reason": (
                    None
                    if diagnostics_complete
                    else "non_finite_token_evidence"
                ),
                "chunk_count": len(chunk_results),
                "audio_encode_count": len(chunk_results),
                "prompt_prefill_count": len(chunk_results),
                "generated_token_count": sum(
                    len(result["tokens"]) for result in chunk_results
                ),
                "max_new_tokens": sum(
                    result["max_new_tokens"] for result in chunk_results
                ),
                "finish_reason": finish_reason,
                "token_evidence_truncated": evidence_truncated,
                "audio_feature_ms": _sum_known(
                    result["audio_feature_ms"] for result in chunk_results
                ),
                "prompt_prefill_ms": _sum_known(
                    result["prompt_prefill_ms"] for result in chunk_results
                ),
                "greedy_decode_ms": _sum_known(
                    result["greedy_decode_ms"] for result in chunk_results
                ),
                "worker_total_ms": _finite_or_none(
                    (time.perf_counter() - started) * 1000
                ),
                "mlx_peak_memory_bytes": (
                    self._memory("get_peak_memory")
                    if callable(reset_peak)
                    else None
                ),
                "mlx_active_memory_bytes_before_cleanup": _max_known(
                    result["active_before_cleanup"]
                    for result in chunk_results
                ),
                "mlx_active_memory_bytes_after_cleanup": chunk_results[-1][
                    "active_after_cleanup"
                ],
                "mlx_cache_memory_bytes_after_cleanup": chunk_results[-1][
                    "cache_after_cleanup"
                ],
                **_resource_metrics(resource_started),
            },
        }
        if shadow_diagnostics is not None:
            output["qwen_shadow"] = shadow_diagnostics
        return output


def _run_transcription(
    session,
    greedy,
    audio_path: str,
    language: str | None,
    runtime_version: str | None,
    fallback_reason: str | None,
    shadow: dict | None = None,
) -> dict:
    if greedy is None:
        output = _official_fallback(
            session,
            audio_path,
            language,
            runtime_version,
            fallback_reason or "greedy_unavailable",
        )
        if shadow is not None:
            output["qwen_shadow"] = _empty_shadow_diagnostics(
                (
                    "unavailable"
                    if bool(shadow.get("enabled", False))
                    else "disabled"
                ),
                chunk_count=0,
                fallback_reason=(
                    (fallback_reason or "greedy_unavailable")
                    if bool(shadow.get("enabled", False))
                    else None
                ),
            )
        return output
    try:
        return greedy.transcribe(audio_path, language, shadow)
    except Exception:
        traceback.print_exc(file=sys.stderr)
        try:
            greedy.recover_after_failure()
        except Exception:
            traceback.print_exc(file=sys.stderr)
        raise


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--language")
    args = parser.parse_args()

    # stdout is reserved for the JSON-lines protocol.
    with contextlib.redirect_stdout(sys.stderr):
        from mlx_qwen3_asr import Session

        runtime_version = _runtime_version()
        session = Session(args.model)
        greedy = None
        fallback_reason = None
        if (
            runtime_version is not None
            and runtime_version in SUPPORTED_RUNTIME_VERSIONS
        ):
            try:
                greedy = GreedyDiagnostics(session, runtime_version)
            except Exception:
                traceback.print_exc(file=sys.stderr)
                fallback_reason = "greedy_initialization_failed"
        else:
            fallback_reason = (
                "runtime_version_unavailable"
                if runtime_version is None
                else "unsupported_runtime_version"
            )
            print(
                "Qwen runtime does not support product greedy diagnostics; "
                "using official Session.transcribe",
                file=sys.stderr,
            )

    for line in sys.stdin:
        request = {}
        try:
            request = json.loads(line)
            with contextlib.redirect_stdout(sys.stderr):
                output = _run_transcription(
                    session,
                    greedy,
                    request["audio_path"],
                    args.language or None,
                    runtime_version,
                    fallback_reason,
                    request.get("shadow"),
                )
            response = {"id": request["id"], **output}
        except Exception as error:  # keep worker alive after one bad request
            traceback.print_exc(file=sys.stderr)
            response = {
                "id": request.get("id", 0) if isinstance(request, dict) else 0,
                "error": str(error),
            }
        print(json.dumps(response, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
