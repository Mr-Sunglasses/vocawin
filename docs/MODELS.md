# VocaWin model adapters

Every engine executes locally. A model is downloaded only once; VocaWin never uploads microphone audio.

## Layout

In-app Download unpacks each model under `%APPDATA%\com.vocahq.vocawin\models` using the catalog ID:

```text
models/
├── whisper-tiny.bin
├── distil-whisper-large-v3.bin
├── parakeet-tdt-0.6b-v3/
├── moonshine-tiny/
├── moonshine-base/
├── sensevoice-small/
├── gigaam-v3/
└── canary-180m/
```

Whisper-family models are a single GGML `.bin`. ONNX models are directories whose filenames match what `transcribe-rs` expects.

## Supported adapters

| VocaWin ID | Adapter | Source package |
| --- | --- | --- |
| `whisper-*` / `distil-whisper-large-v3` | whisper.cpp | Official GGML `.bin` from Hugging Face |
| `parakeet-tdt-0.6b-v3` | ONNX Runtime / Parakeet | [int8 archive](https://blob.handy.computer/parakeet-v3-int8.tar.gz) |
| `moonshine-tiny` | ONNX Runtime / Moonshine | [ONNX files](https://huggingface.co/onnx-community/moonshine-tiny-ONNX) |
| `moonshine-base` | ONNX Runtime / Moonshine | [Moonshine base archive](https://blob.handy.computer/moonshine-base.tar.gz) |
| `sensevoice-small` | ONNX Runtime / SenseVoice | [int8 archive](https://blob.handy.computer/sense-voice-int8.tar.gz) |
| `gigaam-v3` | ONNX Runtime / GigaAM | [int8 archive](https://blob.handy.computer/giga-am-v3-int8.tar.gz) |
| `canary-180m` | ONNX Runtime / Canary | [Canary 180M archive](https://blob.handy.computer/canary-180m-flash.tar.gz) |

On Windows, Parakeet, SenseVoice and Canary run on ONNX Runtime's DirectML execution provider when DXGI finds a hardware GPU (software/WARP adapters do not count). DirectML uses the system's default adapter, which can differ from the one Settings names for Whisper. Operators DirectML cannot run fall back to CPU inside ONNX Runtime. If a DirectML load or decode fails, VocaWin decodes that take on CPU and keeps later takes on CPU until it restarts. Moonshine and GigaAM always run on CPU.

## Long takes

Canary, Moonshine and GigaAM get takes longer than 20 seconds in windows of up to about 25 seconds, cut in pauses, and the window texts are joined (`src-tauri/src/chunking.rs`). Decoded whole, Canary skips sentences once real speech runs past roughly 40 seconds, Moonshine repeats a phrase and rejects anything over 64 seconds, and GigaAM's encoder rejects anything over 200 seconds. Whisper already decodes in 30-second windows. Parakeet and SenseVoice decode the whole take.

Parakeet's memory grows with the take: about 1.2 GB for 10 seconds, 1.8 GB for 60 seconds (the default maximum recording) and 4 GB for 300 seconds. Splitting it would bring that down, but Parakeet drops words at the start of each window, so VocaWin keeps the take whole. On a low-RAM PC, keep Parakeet takes short or pick a smaller model.

## Not in the catalog

`parakeet-ctc-1.1b` and `vosk-small-en` stay out of the Models list until an adapter can transcribe them. The UI never offers a Download that cannot run.
