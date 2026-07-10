#!/usr/bin/env python3
"""Orpheus TTS 3B inference server.

Started by the plugin-tts-orpheus-3b Rust plugin. Prints "PORT:<n>" to stdout
once the HTTP server is bound so the plugin knows which port to connect to.

The model is downloaded from HuggingFace on first run and cached in --model-dir.

Endpoints
---------
POST /synthesize
    Body: {"text": "...", "voice": "tara", "instructions": "..."}
    Returns: audio/wav bytes

GET /health
    Returns: {"status": "ok"}
"""

import argparse
import io
import json
import os
import socket
import sys
import threading

import numpy as np
import scipy.io.wavfile as wavfile
import torch
from fastapi import FastAPI, HTTPException
from fastapi.responses import Response
from huggingface_hub import snapshot_download
from pydantic import BaseModel
from snac import SNAC
from transformers import AutoModelForCausalLM, AutoTokenizer
import uvicorn

# ── Model IDs ────────────────────────────────────────────────────────────────

ORPHEUS_MODEL_ID = "canopylabs/orpheus-3b-0.1-ft"
SNAC_MODEL_ID    = "hubertsiuzdak/snac_24khz"
SAMPLE_RATE      = 24000

VALID_VOICES = {"tara", "dan", "leah", "zac", "zoe", "mia", "julia", "leo"}

# ── Globals set at startup ────────────────────────────────────────────────────

model      = None
tokenizer  = None
snac_model = None
default_voice = "tara"
if torch.cuda.is_available():
    device = "cuda"
elif hasattr(torch.backends, "mps") and torch.backends.mps.is_available():
    device = "mps"
    # Some transformer ops are not yet implemented on MPS; fall back to CPU.
    os.environ.setdefault("PYTORCH_ENABLE_MPS_FALLBACK", "1")
else:
    device = "cpu"

# ── Model loading ─────────────────────────────────────────────────────────────

def load_model(model_dir: str, quantization: str) -> None:
    global model, tokenizer, snac_model

    print(f"[orpheus] loading model (quantization={quantization}, device={device})", flush=True)

    hf_cache = os.path.join(model_dir, "hf_cache")
    os.makedirs(hf_cache, exist_ok=True)

    tokenizer = AutoTokenizer.from_pretrained(
        ORPHEUS_MODEL_ID,
        cache_dir=hf_cache,
    )

    load_kwargs: dict = {
        "cache_dir":         hf_cache,
        "torch_dtype":       torch.float16 if device in ("cuda", "mps") else torch.float32,
        "device_map":        "auto" if device == "cuda" else None,
        "low_cpu_mem_usage": True,
    }

    # bitsandbytes quantization is CUDA-only — skip on MPS and CPU.
    if device == "cuda":
        if quantization == "int8":
            load_kwargs["load_in_8bit"] = True
        elif quantization == "int4":
            load_kwargs["load_in_4bit"] = True
    elif quantization != "none":
        print(f"[orpheus] quantization '{quantization}' not supported on {device}, running fp16", flush=True)

    model = AutoModelForCausalLM.from_pretrained(ORPHEUS_MODEL_ID, **load_kwargs)
    if device != "cuda":  # for cuda, device_map="auto" already handles placement
        model = model.to(device)
    model.eval()

    snac_model = SNAC.from_pretrained(SNAC_MODEL_ID, cache_dir=hf_cache).to(device)
    snac_model.eval()

    print("[orpheus] model loaded", flush=True)


# ── Inference ─────────────────────────────────────────────────────────────────

def _tokens_to_audio(token_ids: list[int]) -> np.ndarray:
    """Decode Orpheus audio token stream via SNAC to a float32 waveform."""
    # Orpheus uses a 7-level SNAC codec; tokens are interleaved in groups of 7.
    # Filter to valid audio token range (typically 128266–129290 for 24 kHz SNAC).
    audio_token_start = 128266
    audio_tokens = [t - audio_token_start for t in token_ids if t >= audio_token_start]

    if len(audio_tokens) < 7:
        return np.zeros(0, dtype=np.float32)

    # Trim to multiple of 7.
    n = (len(audio_tokens) // 7) * 7
    audio_tokens = audio_tokens[:n]

    layers = [[] for _ in range(7)]
    for i, tok in enumerate(audio_tokens):
        layers[i % 7].append(tok)

    with torch.no_grad():
        codes = [
            torch.tensor(layer, dtype=torch.long, device=device).unsqueeze(0)
            for layer in layers
        ]
        audio = snac_model.decode(codes)

    return audio.squeeze().cpu().float().numpy()


def synthesize_text(text: str, voice: str, instructions: str | None) -> bytes:
    voice = voice if voice in VALID_VOICES else default_voice

    # Build prompt in Orpheus format.
    prompt = f"<|audio|>{voice}: {text}<|eot_id|>"
    if instructions:
        prompt = f"<|audio|>{voice}: {text} [style: {instructions}]<|eot_id|>"

    inputs = tokenizer(prompt, return_tensors="pt").to(device)

    with torch.no_grad():
        output_ids = model.generate(
            **inputs,
            max_new_tokens=4096,
            do_sample=True,
            temperature=0.7,
            repetition_penalty=1.1,
            eos_token_id=tokenizer.eos_token_id,
        )

    # Strip the prompt tokens; keep only newly generated tokens.
    new_tokens = output_ids[0][inputs["input_ids"].shape[1]:].tolist()
    waveform = _tokens_to_audio(new_tokens)

    if waveform.size == 0:
        raise RuntimeError("orpheus: decoding produced no audio samples")

    # Encode to 16-bit WAV in memory.
    pcm = (waveform * 32767).astype(np.int16)
    buf = io.BytesIO()
    wavfile.write(buf, SAMPLE_RATE, pcm)
    return buf.getvalue()


# ── FastAPI app ───────────────────────────────────────────────────────────────

app = FastAPI()


class SynthesizeRequest(BaseModel):
    text:         str
    voice:        str | None = None
    instructions: str | None = None


@app.post("/synthesize")
def synthesize(req: SynthesizeRequest):
    if not req.text.strip():
        raise HTTPException(status_code=400, detail="text is empty")
    try:
        audio = synthesize_text(
            req.text,
            req.voice or default_voice,
            req.instructions,
        )
        return Response(content=audio, media_type="audio/wav")
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.get("/health")
def health():
    return {"status": "ok"}


# ── Entry point ───────────────────────────────────────────────────────────────

def main() -> None:
    global default_voice

    parser = argparse.ArgumentParser()
    parser.add_argument("--model-dir",     default="models/orpheus-3b")
    parser.add_argument("--quantization",  default="int8", choices=["none", "int8", "int4"])
    parser.add_argument("--default-voice", default="tara")
    args = parser.parse_args()

    default_voice = args.default_voice

    load_model(args.model_dir, args.quantization)

    # Bind on port 0 — OS assigns a free port.
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()

    # Print port for the Rust plugin to read.
    print(f"PORT:{port}", flush=True)

    config = uvicorn.Config(app, host="127.0.0.1", port=port, log_level="warning")
    server = uvicorn.Server(config)
    server.run()


if __name__ == "__main__":
    main()
