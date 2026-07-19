"""Persistent local Qwen3-ASR worker used by the Rust engine adapter."""

import argparse
import contextlib
import importlib.metadata
import json
import math
import resource
import sys
import time
import traceback


MAX_TOKEN_EVIDENCE = 2048
SUPPORTED_RUNTIME_VERSIONS = frozenset({"0.3.5"})


def _runtime_version() -> str | None:
    try:
        return importlib.metadata.version("mlx-qwen3-asr")
    except importlib.metadata.PackageNotFoundError:
        return None


def _finite(value: float) -> float:
    value = float(value)
    return value if math.isfinite(value) else 0.0


def _resource_usage() -> dict:
    usage = resource.getrusage(resource.RUSAGE_SELF)
    max_rss = int(usage.ru_maxrss)
    if sys.platform != "darwin":
        max_rss *= 1024
    return {
        "max_rss_bytes": max(max_rss, 0),
        "user_cpu_seconds": max(float(usage.ru_utime), 0.0),
        "system_cpu_seconds": max(float(usage.ru_stime), 0.0),
    }


def _resource_metrics(started: dict) -> dict:
    finished = _resource_usage()
    return {
        "process_max_rss_bytes": finished["max_rss_bytes"],
        "process_user_cpu_ms": _finite(
            max(
                finished["user_cpu_seconds"] - started["user_cpu_seconds"],
                0.0,
            )
            * 1000
        ),
        "process_system_cpu_ms": _finite(
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
            "worker_total_ms": _finite(
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

    def _memory(self, name: str) -> int:
        getter = getattr(self.mx, name, None)
        return int(getter()) if callable(getter) else 0

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
    ) -> tuple[int, dict]:
        step_logits = logits.reshape(-1).astype(self.mx.float32)
        selected = int(self.mx.argmax(step_logits).item())
        logprobs = step_logits - self.mx.logsumexp(step_logits)
        selected_logprob = _finite(logprobs[selected].item())
        probabilities = self.mx.exp(logprobs)
        entropy_terms = self.mx.where(
            probabilities > 0,
            probabilities * logprobs,
            self.mx.zeros_like(logprobs),
        )
        entropy = _finite((-self.mx.sum(entropy_terms)).item())

        size = int(step_logits.size)
        margin = 0.0
        if size >= 2:
            partition = self.mx.argpartition(step_logits, size - 2)
            top_values = self.mx.take(step_logits, partition[-2:])
            self.mx.eval(top_values)
            ordered = sorted(
                (float(value) for value in top_values.tolist()),
                reverse=True,
            )
            margin = _finite(ordered[0] - ordered[1])

        return selected, {
            "chunk_index": chunk_index,
            "token_index": token_index,
            "token_id": selected,
            "text": self.session.tokenizer.decode([selected]),
            "selected_logprob": selected_logprob,
            "entropy": entropy,
            "top1_top2_margin": margin,
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
        greedy_started = time.perf_counter()
        for step in range(max_new_tokens):
            token, row = self._token_and_evidence(
                logits,
                chunk_index=chunk_index,
                token_index=token_offset + len(generated),
            )
            generated.append(token)
            if token not in config.eos_token_ids:
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
        active_before_cleanup = self._memory("get_active_memory")

        del mel, feature_lens, audio_features, input_ids
        del positions, position_ids, next_pos_base, next_positions
        del logits, cache
        self._sync()
        self.clear_mlx_cache()

        return {
            "text": text,
            "language": detected_language,
            "tokens": visible_tokens,
            "evidence": evidence,
            "finish_reason": finish_reason,
            "max_new_tokens": max_new_tokens,
            "audio_feature_ms": audio_feature_ms,
            "prompt_prefill_ms": prompt_prefill_ms,
            "greedy_decode_ms": greedy_decode_ms,
            "active_before_cleanup": active_before_cleanup,
            "active_after_cleanup": self._memory("get_active_memory"),
            "cache_after_cleanup": self._memory("get_cache_memory"),
            "total_ms": (time.perf_counter() - chunk_started) * 1000,
        }

    def transcribe(self, audio_path: str, language: str | None) -> dict:
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

        return {
            "text": text,
            "language": final_language,
            "token_evidence": all_evidence,
            "qwen_metrics": {
                "schema_version": 1,
                "runtime_version": self.runtime_version,
                "decode_mode": "greedy_only",
                "diagnostics_complete": True,
                "fallback_reason": None,
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
                "audio_feature_ms": _finite(
                    sum(
                        result["audio_feature_ms"]
                        for result in chunk_results
                    )
                ),
                "prompt_prefill_ms": _finite(
                    sum(
                        result["prompt_prefill_ms"]
                        for result in chunk_results
                    )
                ),
                "greedy_decode_ms": _finite(
                    sum(
                        result["greedy_decode_ms"]
                        for result in chunk_results
                    )
                ),
                "worker_total_ms": _finite(
                    (time.perf_counter() - started) * 1000
                ),
                "mlx_peak_memory_bytes": self._memory("get_peak_memory"),
                "mlx_active_memory_bytes_before_cleanup": max(
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


def _run_transcription(
    session,
    greedy,
    audio_path: str,
    language: str | None,
    runtime_version: str | None,
    fallback_reason: str | None,
) -> dict:
    if greedy is None:
        return _official_fallback(
            session,
            audio_path,
            language,
            runtime_version,
            fallback_reason or "greedy_unavailable",
        )
    try:
        return greedy.transcribe(audio_path, language)
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
