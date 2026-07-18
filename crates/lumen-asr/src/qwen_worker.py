"""Persistent local Qwen3-ASR worker used by the Rust engine adapter."""

import argparse
import contextlib
import json
import sys
import traceback


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--language")
    args = parser.parse_args()

    # stdout is reserved for the JSON-lines protocol.
    with contextlib.redirect_stdout(sys.stderr):
        from mlx_qwen3_asr import Session

        session = Session(args.model)

    for line in sys.stdin:
        request = {}
        try:
            request = json.loads(line)
            with contextlib.redirect_stdout(sys.stderr):
                result = session.transcribe(
                    request["audio_path"],
                    language=args.language or None,
                    verbose=False,
                )
            response = {
                "id": request["id"],
                "text": getattr(result, "text", ""),
                "language": getattr(result, "language", None),
            }
        except Exception as error:  # keep worker alive after one bad request
            traceback.print_exc(file=sys.stderr)
            response = {
                "id": request.get("id", 0) if isinstance(request, dict) else 0,
                "error": str(error),
            }
        print(json.dumps(response, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
