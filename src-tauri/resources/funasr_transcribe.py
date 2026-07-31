#!/usr/bin/env python3
import argparse
import os
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

from funasr import AutoModel

MODEL_NAMES = (
    "iic--speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch",
    "iic--speech_fsmn_vad_zh-cn-16k-common-pytorch",
    "iic--punc_ct-transformer_cn-en-common-vocab471067-large",
    "iic--speech_campplus_sv_zh-cn_16k-common",
)


def model_paths():
    root = Path(os.environ["MODELSCOPE_CACHE"]) / "models"
    paths = tuple(root / name / "snapshots" / "master" for name in MODEL_NAMES)
    missing = [str(path) for path in paths if not path.is_dir()]
    if missing:
        raise RuntimeError("本地模型文件不完整：" + ", ".join(missing))
    return paths


def normalize_audio(input_path):
    if input_path.suffix.lower() == ".wav":
        return input_path, None
    ffmpeg = shutil.which("ffmpeg") or next((path for path in ("/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg") if Path(path).is_file()), None)
    if not ffmpeg:
        raise RuntimeError("转写该音频格式需要安装 FFmpeg")
    handle = tempfile.NamedTemporaryFile(prefix="snack-record-", suffix=".wav", delete=False)
    handle.close()
    temporary = Path(handle.name)
    result = subprocess.run([ffmpeg, "-nostdin", "-loglevel", "error", "-y", "-i", str(input_path), "-vn", "-ac", "1", "-ar", "16000", str(temporary)], capture_output=True, text=True)
    if result.returncode:
        temporary.unlink(missing_ok=True)
        raise RuntimeError(result.stderr.strip().splitlines()[-1])
    return temporary, temporary


def timestamp(milliseconds):
    seconds = max(0, int(milliseconds)) // 1000
    return f"{seconds // 3600:02d}:{seconds % 3600 // 60:02d}:{seconds % 60:02d}"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("input", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("start_time")
    parser.add_argument("--mode", choices=("fast", "standard"), default="fast")
    args = parser.parse_args()
    asr, vad, punctuation, speaker = model_paths()
    options = {"model": str(asr), "vad_model": str(vad), "punc_model": str(punctuation), "disable_update": True, "disable_pbar": True}
    if args.mode == "standard":
        options["spk_model"] = str(speaker)
    prepared, temporary = normalize_audio(args.input)
    try:
        result = AutoModel(**options).generate(input=str(prepared), batch_size_s=300, sentence_timestamp=True)[0]
    finally:
        if temporary:
            temporary.unlink(missing_ok=True)
    lines = [f"录音开始时间：{args.start_time}", ""]
    for sentence in result.get("sentence_info") or []:
        text = re.sub(r"(?<=[\u3400-\u9fff])\s+(?=[\u3400-\u9fff])", "", (sentence.get("text") or "").strip())
        if text:
            speaker_name = f"说话人 {int(sentence.get('spk', 0)) + 1}：" if args.mode == "standard" else ""
            lines.append(f"[{timestamp(sentence.get('start', 0))}] {speaker_name}{text}")
    if len(lines) == 2:
        lines.append("[00:00:00] " + (result.get("text") or "未检测到可识别的对话声音"))
    args.output.write_text("\n".join(lines) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
