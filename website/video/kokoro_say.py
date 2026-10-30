"""Speaks the narration with Kokoro (82M, Apache 2.0) through kokoro-onnx.

make_video.py runs this with the Python of a virtualenv that has kokoro-onnx,
for example:

    python3 -m venv ~/.cache/meersh-kokoro
    ~/.cache/meersh-kokoro/bin/python -I -m pip install kokoro-onnx
    ~/.cache/meersh-kokoro/bin/python -I website/video/kokoro_say.py JOBS.json

JOBS.json holds the folder of the model files, the voice, the speed, the
pronunciation lexicon (word to phonemes in Kokoro's espeak-style IPA) and a
list of jobs, each with the text and the WAV file to write. Lexicon words are
spliced in as phonemes, the rest of a sentence goes through espeak-ng, so
"meersh" comes out as written in the lexicon and not as espeak guesses it.
Prints one JSON line per job with the phonemes it spoke.

The model files are kokoro-v1.0.onnx and voices-v1.0.bin from the kokoro-onnx
GitHub releases (model-files-v1.0). The espeak-ng of the system is used,
because the copy bundled with kokoro-onnx cannot read its data from a long
path. The tour videos of meertex (tools/video) and meerora (docs/video) use the
same voice.
"""

import json
import re
import sys
import wave
from pathlib import Path

import numpy as np
from kokoro_onnx import EspeakConfig, Kokoro

PUNCTUATION = ".,!?;:"


def espeak_config() -> EspeakConfig:
    for lib in ("/usr/lib64/libespeak-ng.so.1", "/usr/lib/x86_64-linux-gnu/libespeak-ng.so.1", "/usr/lib/libespeak-ng.so.1"):
        for data in ("/usr/share/espeak-ng-data", "/usr/lib/x86_64-linux-gnu/espeak-ng-data"):
            if Path(lib).exists() and Path(data, "phontab").exists():
                return EspeakConfig(lib_path=lib, data_path=data)
    return EspeakConfig()


def phonemes(kokoro: Kokoro, text: str, lexicon: dict[str, str], lang: str) -> str:
    if not lexicon:
        return kokoro.tokenizer.phonemize(text, lang)
    words = "|".join(re.escape(w) for w in sorted(lexicon, key=len, reverse=True))
    pieces: list[str] = []
    for part in re.split(rf"\b({words})\b", text, flags=re.IGNORECASE):
        said = lexicon.get(part.lower())
        if said:
            pieces.append(said)
            continue
        part = part.strip()
        if not part:
            continue
        if all(c in PUNCTUATION for c in part.replace(" ", "")):
            pieces.append(part.replace(" ", ""))
        else:
            pieces.append(kokoro.tokenizer.phonemize(part, lang))
    out = ""
    for p in pieces:
        out += p if (not out or p[0] in PUNCTUATION) else " " + p
    return out


def write_wav(path: Path, samples: np.ndarray, rate: int) -> None:
    pcm = (np.clip(samples, -1.0, 1.0) * 32767).astype("<i2")
    with wave.open(str(path), "wb") as f:
        f.setnchannels(1)
        f.setsampwidth(2)
        f.setframerate(rate)
        f.writeframes(pcm.tobytes())


def main() -> None:
    job = json.loads(Path(sys.argv[1]).read_text())
    models = Path(job["models"]).expanduser()
    kokoro = Kokoro(str(models / "kokoro-v1.0.onnx"), str(models / "voices-v1.0.bin"), espeak_config=espeak_config())
    lexicon = {k.lower(): v for k, v in job.get("lexicon", {}).items()}
    lang = job.get("lang", "en-us")
    for item in job["jobs"]:
        said = phonemes(kokoro, item["text"], lexicon, lang)
        samples, rate = kokoro.create(said, voice=item.get("voice", job["voice"]), speed=job.get("speed", 1.0), lang=lang, is_phonemes=True)
        write_wav(Path(item["out"]), samples, rate)
        print(json.dumps({"out": item["out"], "phonemes": said, "seconds": round(len(samples) / rate, 3)}), flush=True)


if __name__ == "__main__":
    main()
