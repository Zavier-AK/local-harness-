// The voice's voice: Kokoro (82M, Apache 2.0), a natural-sounding text-to-speech model
// that runs on the Mac. Spoken to over stdin/stdout, one JSON object per line:
//   → {"id": 1, "op": "say", "text": "…", "voice": "bm_george", "speed": 1.0}
//   ← {"id": 1, "ok": true, "wav": "<base64>", "ms": 420}
//   ← {"event": "progress", "file": "…", "received": 123, "total": 456}   (first download)
//
// The model (~90 MB) is downloaded from Hugging Face on first use and cached in
// HARNESS_TTS_CACHE. The voices ship inside the kokoro-js package.

import { createInterface } from "node:readline";

const MODEL = "onnx-community/Kokoro-82M-v1.0-ONNX";
let tts = null;
let loading = null;

async function load() {
  if (tts) return tts;
  if (!loading) {
    loading = (async () => {
      const { env } = await import("@huggingface/transformers");
      if (process.env.HARNESS_TTS_CACHE) env.cacheDir = process.env.HARNESS_TTS_CACHE;
      const { KokoroTTS } = await import("kokoro-js");
      tts = await KokoroTTS.from_pretrained(MODEL, {
        dtype: "q8",
        device: "cpu",
        progress_callback: (p) => {
          if (p.status === "progress") {
            reply({ event: "progress", file: p.file, received: p.loaded ?? 0, total: p.total ?? null });
          }
        },
      });
      return tts;
    })().catch((error) => {
      loading = null;
      throw error;
    });
  }
  return loading;
}

const ops = {
  async status() {
    return { loaded: !!tts };
  },
  async load() {
    await load();
    return {};
  },
  async say({ text, voice, speed }) {
    const started = Date.now();
    const model = await load();
    const clean = String(text || "").trim().slice(0, 1000);
    if (!clean) throw new Error("nothing to say");
    const audio = await model.generate(clean, {
      voice: voice || "bm_george",
      speed: Math.min(1.5, Math.max(0.6, Number(speed) || 1)),
    });
    const wav = Buffer.from(audio.toWav()).toString("base64");
    return { wav, ms: Date.now() - started };
  },
};

function reply(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}

let queue = Promise.resolve();
const lines = createInterface({ input: process.stdin });
lines.on("line", (line) => {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    return;
  }
  queue = queue.then(async () => {
    const op = ops[request.op];
    if (!op) return reply({ id: request.id, ok: false, error: `unknown op ${request.op}` });
    try {
      reply({ id: request.id, ok: true, ...(await op(request)) });
    } catch (error) {
      reply({ id: request.id, ok: false, error: String(error?.message || error).split("\n")[0] });
    }
  });
});
lines.on("close", () => process.exit(0));
