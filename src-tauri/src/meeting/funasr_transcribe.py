#!/usr/bin/env python3
"""Run the fixed ModelScope bundle locally and return a compact transcript."""
import json
import os
import sys
from pathlib import Path

from funasr import AutoModel

MODEL_NAMES = (
    "iic--speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch",
    "iic--speech_fsmn_vad_zh-cn-16k-common-pytorch",
    "iic--punc_ct-transformer_cn-en-common-vocab471067-large",
    "iic--speech_campplus_sv_zh-cn_16k-common",
)


def model_dir(cache: Path, name: str) -> Path:
    path = cache / "models" / name / "snapshots" / "master"
    if not path.is_dir():
        raise RuntimeError(f"模型文件缺失：{name}")
    return path


def main() -> None:
    cache = Path(os.environ["MODELSCOPE_CACHE"])
    asr, vad, punc, speaker = (model_dir(cache, name) for name in MODEL_NAMES)
    model = AutoModel(
        model=str(asr), vad_model=str(vad), punc_model=str(punc), spk_model=str(speaker),
        disable_update=True, disable_pbar=True,
    )
    result = model.generate(input=str(Path(sys.argv[1])), batch_size_s=300, sentence_timestamp=True)
    item = result[0] if result else {}
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
    print(json.dumps({"text": text, "segments": segments}, ensure_ascii=False))


if __name__ == "__main__":
    main()
