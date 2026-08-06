#!/usr/bin/env python3
"""Download Snack's fixed local meeting bundle from ModelScope.

The desktop process consumes one JSON progress object per line.  Progress is
derived from the bytes already committed to the ModelScope cache, rather than
from a guessed timer, so the UI remains truthful across resumed downloads.
"""
import json
import os
import threading
import time
from pathlib import Path

from modelscope import snapshot_download

MODEL_IDS = (
    "iic/speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch",
    "iic/speech_fsmn_vad_zh-cn-16k-common-pytorch",
    "iic/punc_ct-transformer_cn-en-common-vocab471067-large",
    "iic/speech_campplus_sv_zh-cn_16k-common",
)
ESTIMATED_BYTES = 2_200_000_000


def cache_size(path: Path) -> int:
    total = 0
    for item in path.rglob("*"):
        try:
            if item.is_file():
                total += item.stat().st_size
        except FileNotFoundError:
            # The SDK may atomically replace a temporary file while the
            # monitor is walking the cache; retrying on the next tick is safe.
            continue
    return total


def emit(cache: Path, previous: list[float]) -> None:
    downloaded = cache_size(cache)
    now = time.monotonic()
    elapsed = max(now - previous[1], 0.1)
    speed = max(0, int((downloaded - previous[0]) / elapsed))
    previous[:] = [downloaded, now]
    percent = min(99, int(downloaded * 100 / max(ESTIMATED_BYTES, downloaded)))
    print(json.dumps({
        "type": "progress",
        "downloadedBytes": downloaded,
        "totalBytes": max(ESTIMATED_BYTES, downloaded),
        "speedBytesPerSec": speed,
        "percent": percent,
    }), flush=True)


def main() -> None:
    cache = Path(os.environ["MODELSCOPE_CACHE"]).expanduser()
    cache.mkdir(parents=True, exist_ok=True)
    previous = [cache_size(cache), time.monotonic()]
    failure: list[Exception] = []

    def download() -> None:
        try:
            for model_id in MODEL_IDS:
                snapshot_download(model_id=model_id, cache_dir=str(cache))
        except Exception as error:  # reported as JSON for the desktop UI
            failure.append(error)

    worker = threading.Thread(target=download, daemon=True)
    worker.start()
    while worker.is_alive():
        emit(cache, previous)
        worker.join(0.5)
    if failure:
        raise failure[0]
    emit(cache, previous)
    print(json.dumps({"type": "completed", "downloadedBytes": cache_size(cache)}), flush=True)


if __name__ == "__main__":
    main()
