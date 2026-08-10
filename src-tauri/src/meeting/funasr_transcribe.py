#!/usr/bin/env python3
"""Run the fixed ModelScope bundle and stream JSON-line progress/results."""

import json
import math
import os
import sys
import threading
import time
import wave
from contextlib import redirect_stdout
from pathlib import Path

MODEL_NAMES = (
    "iic--speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch",
    "iic--speech_fsmn_vad_zh-cn-16k-common-pytorch",
    "iic--punc_ct-transformer_cn-en-common-vocab471067-large",
    "iic--speech_campplus_sv_zh-cn_16k-common",
)
PROGRESS_INTERVAL_SECONDS = 1.0
MIN_REMAINING_SECONDS = 15.0
MODEL_WARMUP_SECONDS = 20.0
ESTIMATED_REALTIME_FACTOR = 0.65
PCM_BYTES_PER_SECOND = 32_000
PROTOCOL_OUTPUT = sys.stdout
PROTOCOL_LOCK = threading.Lock()


def model_dir(cache: Path, name: str) -> Path:
    path = cache / "models" / name / "snapshots" / "master"
    if not path.is_dir():
        raise RuntimeError(f"模型文件缺失：{name}")
    return path


def emit(payload: dict) -> None:
    with PROTOCOL_LOCK:
        PROTOCOL_OUTPUT.write(json.dumps(payload, ensure_ascii=False) + "\n")
        PROTOCOL_OUTPUT.flush()


def wav_duration(path: Path) -> float | None:
    if path.suffix.lower() != ".wav":
        return None
    try:
        with wave.open(str(path), "rb") as source:
            return source.getnframes() / max(source.getframerate(), 1)
    except (OSError, wave.Error):
        return None


def decoded_duration(path: Path) -> float | None:
    try:
        import soundfile

        return float(soundfile.info(str(path)).duration)
    except Exception:
        try:
            import librosa

            return float(librosa.get_duration(path=str(path)))
        except Exception:
            return None


def audio_duration(path: Path) -> float:
    decoded = wav_duration(path) or decoded_duration(path)
    if decoded and math.isfinite(decoded) and decoded > 0:
        return decoded
    return max(1.0, path.stat().st_size / PCM_BYTES_PER_SECOND)


def estimated_total_seconds(duration: float) -> float:
    return max(30.0, MODEL_WARMUP_SECONDS + duration * ESTIMATED_REALTIME_FACTOR)


class ProgressReporter:
    def __init__(self, duration: float) -> None:
        self.estimated_total = estimated_total_seconds(duration)
        self.started = time.monotonic()
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self._run, name="snack-progress", daemon=True)

    def start(self) -> None:
        self._emit(0.0)
        self.thread.start()

    def stop(self) -> None:
        self.stopped.set()
        self.thread.join(timeout=2.0)

    def _run(self) -> None:
        while not self.stopped.wait(PROGRESS_INTERVAL_SECONDS):
            self._emit(time.monotonic() - self.started)

    def _emit(self, elapsed: float) -> None:
        effective_total = max(self.estimated_total, elapsed + MIN_REMAINING_SECONDS)
        percent = max(2, min(95, int(elapsed / effective_total * 100)))
        remaining = max(MIN_REMAINING_SECONDS, effective_total - elapsed)
        emit({
            "type": "progress",
            "percent": percent,
            "remainingSeconds": math.ceil(remaining),
        })


def transcript_payload(item: dict) -> dict:
    text = (item.get("text") or "").strip()
    segments = []
    for sentence in item.get("sentence_info") or []:
        value = (sentence.get("text") or sentence.get("sentence") or "").strip()
        if value:
            segments.append({
                "startMs": int(sentence.get("start", 0)),
                "endMs": int(sentence.get("end", 0)),
                "text": value,
                "speaker": f"说话人 {int(sentence.get('spk', 0)) + 1}",
            })
    return {"type": "result", "text": text, "segments": segments}


def run_model(audio_path: Path) -> list[dict]:
    cache = Path(os.environ["MODELSCOPE_CACHE"])
    asr, vad, punc, speaker = (model_dir(cache, name) for name in MODEL_NAMES)
    with redirect_stdout(sys.stderr):
        from funasr import AutoModel

        model = AutoModel(
            model=str(asr), vad_model=str(vad), punc_model=str(punc), spk_model=str(speaker),
            disable_update=True, disable_pbar=True,
        )
        return model.generate(
            input=str(audio_path), batch_size_s=300, sentence_timestamp=True
        )


def main() -> None:
    audio_path = Path(sys.argv[1]).resolve()
    reporter = ProgressReporter(audio_duration(audio_path))
    reporter.start()
    try:
        result = run_model(audio_path)
    finally:
        reporter.stop()
    emit(transcript_payload(result[0] if result else {}))


if __name__ == "__main__":
    main()
