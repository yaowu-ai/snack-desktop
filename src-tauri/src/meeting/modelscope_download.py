#!/usr/bin/env python3
"""Download Snack's fixed local meeting bundle from ModelScope.

The desktop process consumes one JSON progress object per line.  Progress is
derived from the bytes already committed to the ModelScope cache, rather than
from a guessed timer, so the UI remains truthful across resumed downloads.
"""
import json
import math
import os
import threading
import time
from collections import deque
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


def emit(cache: Path, samples: deque[tuple[float, int]]) -> None:
    downloaded = cache_size(cache)
    now = time.monotonic()
    samples.append((now, downloaded))
    while len(samples) > 2 and now - samples[0][0] > 10:
        samples.popleft()
    elapsed = max(now - samples[0][0], 0.1)
    speed = max(0, int((downloaded - samples[0][1]) / elapsed))
    total = max(ESTIMATED_BYTES, downloaded)
    remaining = max(0, total - downloaded)
    remaining_seconds = math.ceil(remaining / speed) if remaining and speed else None
    percent = min(99, int(downloaded * 100 / total))
    print(json.dumps({
        "type": "progress",
        "downloadedBytes": downloaded,
        "totalBytes": total,
        "speedBytesPerSec": speed,
        "percent": percent,
        "remainingSeconds": remaining_seconds,
    }), flush=True)


def main() -> None:
    cache = Path(os.environ["MODELSCOPE_CACHE"]).expanduser()
    cache.mkdir(parents=True, exist_ok=True)
    samples = deque([(time.monotonic(), cache_size(cache))])
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
        emit(cache, samples)
        worker.join(0.5)
    if failure:
        raise failure[0]
    emit(cache, samples)
    print(json.dumps({"type": "completed", "downloadedBytes": cache_size(cache)}), flush=True)


if __name__ == "__main__":
    main()
