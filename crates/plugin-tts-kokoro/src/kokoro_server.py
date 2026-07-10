#!/usr/bin/env python3
"""Kokoro-ONNX TTS inference server.

Started by the plugin-tts-kokoro Rust plugin. Prints "PORT:<n>" to stdout
once the HTTP server is bound so the plugin knows which port to connect to.

Model files (kokoro-v1.0.onnx + voices-v1.0.bin) are downloaded from GitHub
releases on first run and cached in --model-dir.

Endpoints
---------
POST /synthesize
    Body: {"text": "...", "voice": "if_sara", "lang": "it", "speed": 1.0}
    Returns: audio/wav bytes

GET /health
    Returns: {"status": "ok"}
"""

import argparse
import io
import os
import socket
import sys
import threading

import numpy as np
import soundfile as sf
import urllib.request
from fastapi import FastAPI, HTTPException
from fastapi.responses import Response
from pydantic import BaseModel
from kokoro_onnx import Kokoro
import uvicorn

# ── Model URLs ────────────────────────────────────────────────────────────────

MODEL_BASE_URL = "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0"
MODEL_ONNX_FILE   = "kokoro-v1.0.onnx"
MODEL_VOICES_FILE = "voices-v1.0.bin"

VALID_VOICES = {
    "af_heart", "af_bella", "af_nicole", "af_sarah", "af_sky",
    "am_adam", "am_michael",
    "bf_emma", "bf_isabella", "bm_george", "bm_lewis",
    "if_sara", "im_nicola",
    "jf_alpha", "jf_gongitsune", "jm_kumo",
    "zf_xiaobei", "zm_yunxi",
}

# ── Globals set at startup ────────────────────────────────────────────────────

kokoro_model: Kokoro | None = None
default_voice = "if_sara"
default_lang  = "it"
default_speed = 1.0

# ── Model loading ─────────────────────────────────────────────────────────────

def download_if_missing(model_dir: str, filename: str) -> str:
    path = os.path.join(model_dir, filename)
    if not os.path.exists(path):
        url = f"{MODEL_BASE_URL}/{filename}"
        print(f"[kokoro] downloading {filename} from {url}", flush=True)
        urllib.request.urlretrieve(url, path)
        print(f"[kokoro] downloaded {filename}", flush=True)
    return path


def load_model(model_dir: str) -> None:
    global kokoro_model
    os.makedirs(model_dir, exist_ok=True)
    onnx_path   = download_if_missing(model_dir, MODEL_ONNX_FILE)
    voices_path = download_if_missing(model_dir, MODEL_VOICES_FILE)
    print(f"[kokoro] loading model from {model_dir}", flush=True)
    kokoro_model = Kokoro(onnx_path, voices_path)
    print("[kokoro] model ready", flush=True)

# ── FastAPI server ────────────────────────────────────────────────────────────

app = FastAPI()


class SynthesizeRequest(BaseModel):
    text:  str
    voice: str | None = None
    lang:  str | None = None
    speed: float | None = None


@app.get("/health")
def health():
    return {"status": "ok"}


@app.post("/synthesize")
def synthesize(req: SynthesizeRequest):
    if kokoro_model is None:
        raise HTTPException(status_code=503, detail="model not loaded")

    voice = req.voice if req.voice in VALID_VOICES else default_voice
    lang  = req.lang  or default_lang
    speed = req.speed or default_speed

    try:
        samples, sample_rate = kokoro_model.create(req.text, voice=voice, speed=speed, lang=lang)
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))

    buf = io.BytesIO()
    sf.write(buf, samples, sample_rate, format="WAV")
    return Response(content=buf.getvalue(), media_type="audio/wav")

# ── Entry point ───────────────────────────────────────────────────────────────

def find_free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-dir",     default="models/kokoro")
    parser.add_argument("--default-voice", default="if_sara")
    parser.add_argument("--default-lang",  default="it")
    parser.add_argument("--default-speed", type=float, default=1.0)
    args = parser.parse_args()

    global default_voice, default_lang, default_speed
    default_voice = args.default_voice
    default_lang  = args.default_lang
    default_speed = args.default_speed

    load_model(args.model_dir)

    port = find_free_port()
    # Signal to the Rust parent that we are ready.
    print(f"PORT:{port}", flush=True)

    uvicorn.run(app, host="127.0.0.1", port=port, log_level="warning")


if __name__ == "__main__":
    main()
