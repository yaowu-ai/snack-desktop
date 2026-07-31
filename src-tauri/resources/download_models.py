#!/usr/bin/env python3
import os

from modelscope import snapshot_download

MODEL_IDS = (
    "iic/speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch",
    "iic/speech_fsmn_vad_zh-cn-16k-common-pytorch",
    "iic/punc_ct-transformer_cn-en-common-vocab471067-large",
    "iic/speech_campplus_sv_zh-cn_16k-common",
)


def main():
    cache_dir = os.environ["MODELSCOPE_CACHE"]
    for model_id in MODEL_IDS:
        snapshot_download(model_id, cache_dir=cache_dir)


if __name__ == "__main__":
    main()
