#!/usr/bin/env python3
"""Persistent mlx-whisper worker (stdin/stdout JSON lines).

Protocol (mirrors qwen_worker shape, simplified):
  request:  {"id": N, "audio_path": "/path.wav"}
  response: {"id": N, "text": "...", "language": "es", "error": null}

Loads mlx-whisper once; keeps the model warm across requests.
"""

from __future__ import annotations

import argparse
import json
import sys
import traceback
from typing import Any, Optional


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--model",
        default="mlx-community/whisper-large-v3-turbo",
        help="HF repo id or local path for mlx-whisper",
    )
    parser.add_argument("--language", default=None, help="forced language code e.g. es")
    args = parser.parse_args()

    try:
        import mlx_whisper
    except ImportError as e:
        print(
            json.dumps(
                {
                    "id": -1,
                    "error": f"mlx_whisper not installed: {e}. "
                    "pip/uv install mlx-whisper into the configured Python env.",
                }
            ),
            flush=True,
        )
        sys.exit(1)

    model = args.model
    language = args.language or None
    # Warm import only; first transcribe loads weights.
    print(
        json.dumps({"id": 0, "ready": True, "model": model, "language": language}),
        flush=True,
        file=sys.stderr,
    )

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        req_id: Any = None
        try:
            req = json.loads(line)
            req_id = req.get("id")
            path = req.get("audio_path")
            if not path:
                raise ValueError("audio_path required")
            kwargs: dict[str, Any] = {
                "path_or_hf_repo": model,
                "verbose": False,
                "word_timestamps": False,
            }
            # Per-request language override if provided.
            lang = req.get("language") or language
            if lang:
                # normalize common names
                low = str(lang).lower()
                if low in {"spanish", "español", "espanol", "spa"}:
                    lang = "es"
                elif low in {"chinese", "中文", "zh-cn", "zh-hans"}:
                    lang = "zh"
                elif low in {"english", "eng"}:
                    lang = "en"
                kwargs["language"] = lang
            result = mlx_whisper.transcribe(path, **kwargs)
            text = (result.get("text") or "").strip()
            out_lang = result.get("language") or lang
            print(
                json.dumps(
                    {
                        "id": req_id,
                        "text": text,
                        "language": out_lang,
                        "error": None,
                    },
                    ensure_ascii=False,
                ),
                flush=True,
            )
        except Exception as e:
            print(
                json.dumps(
                    {
                        "id": req_id,
                        "text": None,
                        "language": None,
                        "error": f"{e}\n{traceback.format_exc()[-800:]}",
                    },
                    ensure_ascii=False,
                ),
                flush=True,
            )


if __name__ == "__main__":
    main()
