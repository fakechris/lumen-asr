import contextlib
import importlib.util
import io
from pathlib import Path
import sys
import unittest


sys.dont_write_bytecode = True
WORKER_PATH = Path(__file__).parents[1] / "src" / "qwen_worker.py"
SPEC = importlib.util.spec_from_file_location("lumen_qwen_worker", WORKER_PATH)
WORKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WORKER)


class OutputLanguageTests(unittest.TestCase):
    def test_uses_first_known_chunk_language(self) -> None:
        self.assertEqual(
            WORKER._select_output_language(
                None,
                ["unknown", "Chinese", "English"],
            ),
            "Chinese",
        )

    def test_forced_language_wins(self) -> None:
        self.assertEqual(
            WORKER._select_output_language(
                "English",
                ["unknown", "Chinese"],
            ),
            "English",
        )

    def test_all_unknown_stays_unknown(self) -> None:
        self.assertEqual(
            WORKER._select_output_language(
                None,
                ["unknown", "unknown"],
            ),
            "unknown",
        )


class ResourceMetricsTests(unittest.TestCase):
    def test_resource_metrics_are_unknown_without_platform_support(self) -> None:
        original_resource = WORKER.resource
        WORKER.resource = None
        try:
            self.assertIsNone(WORKER._resource_usage())
            self.assertEqual(
                WORKER._resource_metrics(None),
                {
                    "process_max_rss_bytes": None,
                    "process_user_cpu_ms": None,
                    "process_system_cpu_ms": None,
                },
            )
        finally:
            WORKER.resource = original_resource

    def test_unavailable_or_invalid_metrics_remain_unknown(self) -> None:
        self.assertIsNone(WORKER._finite_or_none(float("nan")))
        self.assertIsNone(WORKER._finite_or_none(float("inf")))
        self.assertIsNone(WORKER._sum_known([1.0, None]))
        self.assertIsNone(WORKER._max_known([1, None]))

        diagnostics = object.__new__(WORKER.GreedyDiagnostics)
        diagnostics.mx = object()
        self.assertIsNone(diagnostics._memory("get_peak_memory"))


class ShadowPolicyTests(unittest.TestCase):
    def test_detector_is_bounded_and_returns_spans_right_to_left(self) -> None:
        evidence = [
            {
                "chunk_index": 0,
                "token_index": 0,
                "text": "正常",
                "selected_logprob": -0.01,
                "entropy": 0.1,
                "top1_top2_margin": 5.0,
            },
            {
                "chunk_index": 0,
                "token_index": 1,
                "text": "词",
                "selected_logprob": -0.4,
                "entropy": 1.0,
                "top1_top2_margin": 0.05,
            },
            {
                "chunk_index": 0,
                "token_index": 2,
                "text": "GPD",
                "selected_logprob": -0.02,
                "entropy": 0.1,
                "top1_top2_margin": 4.0,
            },
            {
                "chunk_index": 0,
                "token_index": 3,
                "text": "错",
                "selected_logprob": -3.0,
                "entropy": 3.5,
                "top1_top2_margin": 0.01,
            },
        ]

        spans = WORKER._select_shadow_spans(evidence, max_spans=2)

        self.assertEqual([span["token_start"] for span in spans], [3, 2])
        self.assertIn("low_logprob", spans[0]["detector_reasons"])
        self.assertIn("uppercase_run", spans[1]["detector_reasons"])

    def test_dictionary_candidates_require_local_beam_support(self) -> None:
        terms = [
            {"surface": "Codex", "source": "personal_dictionary"},
            {"surface": "codex", "source": "personal_dictionary"},
            {"surface": "Qdrant", "source": "personal_dictionary"},
            {"surface": "he", "source": "personal_dictionary"},
        ]
        hypotheses = [
            {"rank": 1, "text": "Codex is"},
            {"rank": 2, "text": "Cortex"},
            {"rank": 3, "text": "hello"},
        ]

        candidates = WORKER._dictionary_candidates(terms, hypotheses)

        self.assertEqual(
            candidates,
            [
                {
                    "surface": "Codex",
                    "source": "personal_dictionary",
                    "beam_rank": 1,
                }
            ],
        )

    def test_shadow_failure_is_fail_soft_and_never_changes_output(self) -> None:
        def fail():
            raise RuntimeError("branch failed")

        with contextlib.redirect_stderr(io.StringIO()):
            diagnostics = WORKER._run_shadow_fail_soft(
                fail,
                chunk_count=1,
            )

        self.assertEqual(diagnostics["status"], "failed")
        self.assertEqual(diagnostics["fallback_reason"], "shadow_runtime_error")
        self.assertFalse(diagnostics["user_output_changed"])


class RequestFailureTests(unittest.TestCase):
    def test_greedy_failure_does_not_transcribe_audio_twice(self) -> None:
        class Greedy:
            recovered = False

            def transcribe(self, audio_path, language, shadow=None):
                raise RuntimeError("diagnostic failure")

            def recover_after_failure(self):
                self.recovered = True

        class Session:
            call_count = 0

            def transcribe(self, audio_path, language=None, verbose=False):
                self.call_count += 1
                raise AssertionError("official fallback must not run")

        greedy = Greedy()
        session = Session()
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "diagnostic failure"):
                WORKER._run_transcription(
                    session,
                    greedy,
                    "/audio.wav",
                    "Chinese",
                    "0.3.5",
                    None,
                )

        self.assertTrue(greedy.recovered)
        self.assertEqual(session.call_count, 0)


if __name__ == "__main__":
    unittest.main()
