#!/usr/bin/env python3
"""Experimental offline file transcription helper (not the production CLI).

Prefer:

  lumen-asr-desktop meeting process <audio> --engine mlx-whisper --lang es \\
    --format bilingual --translate zh

Example:

  python scripts/offline_file_transcript.py \\
    --audio ./talk.m4a \\
    --engine qwen \\
    --lang Spanish \\
    --desktop-bin ./target/release/lumen-asr-desktop \\
    --out /tmp/out
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import wave
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Optional


@dataclass
class DiarTurn:
    start: float
    end: float
    speaker: str  # S1, S2, …


def ensure_wav(src: Path, work: Path) -> Path:
    if src.suffix.lower() in {".wav", ".wave"}:
        return src
    out = work / "input.16k.wav"
    cmd = [
        "ffmpeg",
        "-y",
        "-i",
        str(src),
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_s16le",
        str(out),
    ]
    subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return out


def load_wav_mono(path: Path) -> tuple[list[float], int]:
    with wave.open(str(path), "rb") as f:
        assert f.getnchannels() == 1
        sr = f.getframerate()
        raw = f.readframes(f.getnframes())
    import array

    samples = array.array("h")
    samples.frombytes(raw)
    return [s / 32768.0 for s in samples], sr


def write_wav_slice(samples: list[float], sr: int, start: float, end: float, path: Path) -> None:
    import array
    import struct

    i0 = max(0, int(start * sr))
    i1 = min(len(samples), int(end * sr))
    chunk = samples[i0:i1]
    pcm = array.array("h", (max(-32768, min(32767, int(x * 32767))) for x in chunk))
    with wave.open(str(path), "wb") as f:
        f.setnchannels(1)
        f.setsampwidth(2)
        f.setframerate(sr)
        f.writeframes(pcm.tobytes())


def diar_via_desktop(desktop: Path, wav: Path, work: Path) -> list[DiarTurn]:
    """Run product diar+SenseVoice only to harvest speaker timeline (ignore text)."""
    out_json = work / "diar_seed.json"
    cmd = [
        str(desktop),
        "meeting",
        "process",
        str(wav),
        "--engine",
        "sensevoice",
        "--format",
        "json",
    ]
    # Prefer new CLI; fall back to --json
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        cmd = [str(desktop), "meeting", "process", str(wav), "--json"]
        proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise RuntimeError(f"diar seed failed: {proc.returncode}")
    segs = json.loads(proc.stdout)
    out_json.write_text(json.dumps(segs, ensure_ascii=False, indent=2))
    # Map uuid speaker_id → S1.. in first-seen order
    label: dict[str, str] = {}
    turns: list[DiarTurn] = []
    for s in segs:
        sid = s.get("speaker_id") or "?"
        if sid not in label:
            label[sid] = f"S{len(label) + 1}"
        turns.append(
            DiarTurn(
                start=float(s["start_seconds"]),
                end=float(s["end_seconds"]),
                speaker=label[sid],
            )
        )
    return turns


def load_turns_json(path: Path) -> list[DiarTurn]:
    data = json.loads(path.read_text())
    # Accept either desktop segment array or {turns:[{start,end,speaker}]}
    if isinstance(data, dict) and "turns" in data:
        return [
            DiarTurn(float(t["start"]), float(t["end"]), str(t["speaker"]))
            for t in data["turns"]
        ]
    if isinstance(data, dict) and "segments" in data:
        data = data["segments"]
    label: dict[str, str] = {}
    turns: list[DiarTurn] = []
    for s in data:
        sid = str(s.get("speaker_id") or s.get("speaker") or "?")
        if sid not in label and not sid.startswith("S"):
            label[sid] = f"S{len(label) + 1}"
        spk = label.get(sid, sid if sid.startswith("S") else label[sid])
        start = float(s.get("start_seconds", s.get("start", 0)))
        end = float(s.get("end_seconds", s.get("end", 0)))
        turns.append(DiarTurn(start, end, spk))
    return turns


def qwen_transcribe_file(
    audio: Path,
    model_dir: Path,
    language: Optional[str],
) -> dict[str, Any]:
    from mlx_qwen3_asr import transcribe

    # Word timestamps need a separate forced-aligner download; for offline
    # production we use diar turn bounds as segment times (cut-style).
    r = transcribe(
        str(audio),
        model=str(model_dir),
        language=language,
        diarize=False,
        return_timestamps=False,
        verbose=False,
    )
    words = []
    segs = getattr(r, "segments", None) or getattr(r, "chunks", None) or []
    for s in segs:
        if hasattr(s, "text"):
            words.append(
                {
                    "word": getattr(s, "text", ""),
                    "start": float(getattr(s, "start", 0) or 0),
                    "end": float(getattr(s, "end", 0) or 0),
                }
            )
        elif isinstance(s, dict):
            words.append(
                {
                    "word": s.get("text") or s.get("word") or "",
                    "start": float(s.get("start") or 0),
                    "end": float(s.get("end") or 0),
                }
            )
    return {
        "text": getattr(r, "text", "") or "",
        "language": getattr(r, "language", language),
        "words": words,
    }


def qwen_per_turn(
    samples: list[float],
    sr: int,
    turns: list[DiarTurn],
    model_dir: Path,
    language: Optional[str],
    work: Path,
) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for i, t in enumerate(turns):
        if t.end - t.start < 0.2:
            out.append(
                {
                    "start": t.start,
                    "end": t.end,
                    "speaker": t.speaker,
                    "text": "",
                    "words": [],
                }
            )
            continue
        slice_path = work / f"turn_{i:04d}.wav"
        write_wav_slice(samples, sr, t.start, t.end, slice_path)
        r = qwen_transcribe_file(slice_path, model_dir, language)
        # lift word times into absolute media time
        words = []
        for w in r["words"]:
            words.append(
                {
                    "word": w["word"],
                    "start": w["start"] + t.start,
                    "end": w["end"] + t.start,
                }
            )
        text = (r["text"] or "").strip()
        print(f"[{t.start:7.1f}-{t.end:<7.1f}] {t.speaker}: {text[:120]}", file=sys.stderr)
        out.append(
            {
                "start": t.start,
                "end": t.end,
                "speaker": t.speaker,
                "text": text,
                "words": words,
            }
        )
    return out


def mlx_whisper_full(
    audio: Path,
    model: str,
    language: Optional[str],
) -> dict[str, Any]:
    import mlx_whisper

    # mlx_whisper.transcribe returns dict with text + segments
    kwargs: dict[str, Any] = {"path_or_hf_repo": model}
    if language:
        # short codes
        lang = language
        if language.lower() in {"spanish", "español", "espanol"}:
            lang = "es"
        kwargs["language"] = lang
    result = mlx_whisper.transcribe(str(audio), **kwargs)
    words: list[dict[str, Any]] = []
    segs_out: list[dict[str, Any]] = []
    for seg in result.get("segments") or []:
        segs_out.append(
            {
                "start": float(seg.get("start", 0)),
                "end": float(seg.get("end", 0)),
                "text": (seg.get("text") or "").strip(),
            }
        )
        for w in seg.get("words") or []:
            words.append(
                {
                    "word": w.get("word") or w.get("text") or "",
                    "start": float(w.get("start", 0)),
                    "end": float(w.get("end", 0)),
                }
            )
    return {
        "text": (result.get("text") or "").strip(),
        "language": result.get("language") or language,
        "segments": segs_out,
        "words": words,
    }


def assign_words_to_turns(
    words: list[dict[str, Any]],
    turns: list[DiarTurn],
    min_coverage: float = 0.5,
) -> list[dict[str, Any]]:
    """Cut-style: for each diar turn, take words whose midpoints fall inside the turn.

    Simpler than cut's paragraph coverage but same idea: diar owns speaker identity;
    ASR owns text/timing.
    """
    out: list[dict[str, Any]] = []
    for t in turns:
        owned = []
        for w in words:
            mid = 0.5 * (float(w["start"]) + float(w["end"]))
            if t.start <= mid < t.end and (w.get("word") or "").strip():
                owned.append(w)
        text = " ".join((w.get("word") or "").strip() for w in owned).strip()
        # fallback: if no words (whisper without word ts), leave empty
        out.append(
            {
                "start": t.start,
                "end": t.end,
                "speaker": t.speaker,
                "text": text,
                "words": owned,
            }
        )
    return out


def to_lumen_transcript_v1(
    segments: list[dict[str, Any]],
    media_path: str,
    engine: str,
    language: Optional[str],
    duration: Optional[float],
) -> dict[str, Any]:
    speakers = []
    seen = set()
    for s in segments:
        sp = s["speaker"]
        if sp not in seen:
            seen.add(sp)
            speakers.append({"id": sp, "display_name": None})
    t_segs = []
    for i, s in enumerate(segments):
        item: dict[str, Any] = {
            "id": str(i),
            "start": s["start"],
            "end": s["end"],
            "text": s["text"],
            "speaker": s["speaker"],
        }
        if s.get("words"):
            item["words"] = [
                {"word": w["word"], "start": w["start"], "end": w["end"]} for w in s["words"]
            ]
        t_segs.append(item)
    return {
        "schema": "lumen-transcript.v1",
        "provenance": {
            "app": "lumen-asr-offline",
            "engine": engine,
            "language": language,
        },
        "media": {
            "path": media_path,
            "duration_seconds": duration,
            "sample_rate": 16000,
            "channels": 1,
        },
        "speakers": speakers,
        "segments": t_segs,
    }


def write_text(path: Path, segments: list[dict[str, Any]]) -> None:
    lines = []
    for s in segments:
        lines.append(f"[{s['start']:8.1f}-{s['end']:<8.1f}] {s['speaker']}: {s['text']}")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def default_qwen_model() -> Path:
    return (
        Path.home()
        / "Library/Application Support/Lumen/models/qwen3-asr-0.6b-8bit"
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--audio", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path, help="output directory")
    ap.add_argument("--engine", choices=["qwen", "mlx-whisper", "both"], default="qwen")
    ap.add_argument("--lang", default="Spanish")
    ap.add_argument("--qwen-model", type=Path, default=None)
    ap.add_argument(
        "--whisper-model",
        default="mlx-community/whisper-large-v3-turbo",
        help="HF repo or local path for mlx-whisper",
    )
    ap.add_argument(
        "--desktop-bin",
        type=Path,
        default=None,
        help="lumen-asr-desktop for diar seed (SenseVoice path harvests timeline only)",
    )
    ap.add_argument(
        "--turns",
        type=Path,
        default=None,
        help="reuse existing diar/segment JSON instead of re-running diar",
    )
    args = ap.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="lumen-offline-"))
    try:
        wav = ensure_wav(args.audio, work)
        samples, sr = load_wav_mono(wav)
        duration = len(samples) / sr

        if args.turns:
            turns = load_turns_json(args.turns)
            print(f"loaded {len(turns)} turns from {args.turns}", file=sys.stderr)
        else:
            desktop = args.desktop_bin
            if desktop is None:
                cand = Path(__file__).resolve().parents[1] / "target/release/lumen-asr-desktop"
                desktop = cand if cand.is_file() else None
            if desktop is None or not desktop.is_file():
                raise SystemExit(
                    "need --desktop-bin path/to/lumen-asr-desktop or --turns diar.json"
                )
            print(f"diar seed via {desktop}", file=sys.stderr)
            turns = diar_via_desktop(desktop, wav, work)
            (args.out / "diar_turns.json").write_text(
                json.dumps(
                    [{"start": t.start, "end": t.end, "speaker": t.speaker} for t in turns],
                    ensure_ascii=False,
                    indent=2,
                ),
                encoding="utf-8",
            )
            print(f"diar: {len(turns)} turns, speakers={sorted({t.speaker for t in turns})}", file=sys.stderr)

        qwen_model = args.qwen_model or default_qwen_model()
        if args.engine in {"qwen", "both"}:
            if not qwen_model.is_dir():
                raise SystemExit(f"Qwen model missing: {qwen_model}")
            print(f"Qwen per-turn ASR model={qwen_model}", file=sys.stderr)
            segs = qwen_per_turn(samples, sr, turns, qwen_model, args.lang, work)
            doc = to_lumen_transcript_v1(
                segs,
                str(args.audio),
                "diar-rs+qwen3-asr-mlx",
                args.lang,
                duration,
            )
            (args.out / "qwen.lumen-transcript.v1.json").write_text(
                json.dumps(doc, ensure_ascii=False, indent=2), encoding="utf-8"
            )
            write_text(args.out / "qwen.speakers.txt", segs)
            print(f"wrote {args.out / 'qwen.lumen-transcript.v1.json'}", file=sys.stderr)

        if args.engine in {"mlx-whisper", "both"}:
            try:
                import mlx_whisper  # noqa: F401
            except ImportError:
                raise SystemExit(
                    'mlx-whisper not installed. Run: uv pip install --python "$LUMEN_QWEN_PYTHON" mlx-whisper'
                )
            print(f"mlx-whisper full pass model={args.whisper_model}", file=sys.stderr)
            full = mlx_whisper_full(wav, args.whisper_model, args.lang)
            (args.out / "whisper.full.json").write_text(
                json.dumps(full, ensure_ascii=False, indent=2), encoding="utf-8"
            )
            # Prefer word-level assign; if no words, map segment midpoints
            if full["words"]:
                segs = assign_words_to_turns(full["words"], turns)
            else:
                # assign whole whisper segments by midpoint into diar turns, then merge
                pseudo_words = []
                for s in full["segments"]:
                    pseudo_words.append(
                        {
                            "word": s["text"],
                            "start": s["start"],
                            "end": s["end"],
                        }
                    )
                segs = assign_words_to_turns(pseudo_words, turns)
            # if empty texts, fall back to per-turn re-slice whisper (slow) — skip for now
            doc = to_lumen_transcript_v1(
                segs,
                str(args.audio),
                f"diar-rs+{args.whisper_model}",
                full.get("language") or args.lang,
                duration,
            )
            (args.out / "whisper.lumen-transcript.v1.json").write_text(
                json.dumps(doc, ensure_ascii=False, indent=2), encoding="utf-8"
            )
            write_text(args.out / "whisper.speakers.txt", segs)
            print(f"wrote {args.out / 'whisper.lumen-transcript.v1.json'}", file=sys.stderr)

        return 0
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
