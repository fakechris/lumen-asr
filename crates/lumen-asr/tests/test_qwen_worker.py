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


class RequestFailureTests(unittest.TestCase):
    def test_greedy_failure_does_not_transcribe_audio_twice(self) -> None:
        class Greedy:
            recovered = False

            def transcribe(self, audio_path, language):
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
