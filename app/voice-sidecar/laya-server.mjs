// Runs Laya for the harness's voice commander.
//
// Spoken to over stdin/stdout, one JSON object per line. Requests carry an `id`; each gets
// exactly one reply with the same `id` and `ok`. Download progress arrives as
// `{"event":"progress",...}` lines in between. Nothing else is written to stdout; logs go
// to stderr.
//
//   {"id":1,"op":"status"}                          -> {"id":1,"ok":true,"downloaded":bool,"loaded":bool}
//   {"id":2,"op":"load"}                            -> {"id":2,"ok":true,"ms":1234}
//   {"id":3,"op":"decide","state":{},"questions":{}} -> {"id":3,"ok":true,"answers":{},"ms":140}
//
// The package is imported lazily, so a missing native runtime is reported as an error on
// `load` rather than a crash at start.

import { stat } from "node:fs/promises";
import path from "node:path";
import readline from "node:readline";

let laya = null;
let loading = null;

const send = (message) => process.stdout.write(JSON.stringify(message) + "\n");
const log = (...args) => console.error("[laya]", ...args);

async function bundleOnDisk() {
  const { defaultCacheDir, BUNDLE_FILES, DEFAULT_REPO } = await import("@receptron/laya");
  const dir = path.join(defaultCacheDir(), DEFAULT_REPO.replace("/", "--"), "main");
  for (const file of BUNDLE_FILES) {
    try {
      await stat(path.join(dir, file));
    } catch {
      return false;
    }
  }
  return true;
}

async function load() {
  if (laya) return;
  if (!loading) {
    loading = (async () => {
      const { Laya } = await import("@receptron/laya");
      const started = Date.now();
      let last = 0;
      laya = await Laya.load({
        onProgress: ({ file, received, total }) => {
          // A few updates a second is plenty for a progress bar.
          const now = Date.now();
          if (now - last > 250 || received === total) {
            last = now;
            send({ event: "progress", file, received, total });
          }
        },
      });
      log(`loaded from ${laya.modelDir} in ${Date.now() - started} ms`);
    })().finally(() => {
      loading = null;
    });
  }
  await loading;
}

async function handle(request) {
  switch (request.op) {
    case "status":
      return { downloaded: await bundleOnDisk().catch(() => false), loaded: laya !== null };
    case "load": {
      const started = Date.now();
      await load();
      return { ms: Date.now() - started };
    }
    case "decide": {
      if (!laya) throw new Error("Laya is not loaded");
      const started = performance.now();
      const result = await laya.systemOne(request.state, request.questions);
      return { answers: result.answers, ms: Math.round(performance.now() - started), tokens: result.usage.input_tokens };
    }
    default:
      throw new Error(`unknown op ${JSON.stringify(request.op)}`);
  }
}

// One request at a time, in order: the harness never sends a second before the first is
// answered, and ONNX Runtime gains nothing from overlapping calls here.
let queue = Promise.resolve();
readline.createInterface({ input: process.stdin }).on("line", (line) => {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    return log("ignoring a line that is not JSON");
  }
  queue = queue.then(async () => {
    try {
      send({ id: request.id, ok: true, ...(await handle(request)) });
    } catch (error) {
      send({ id: request.id, ok: false, error: String(error?.message ?? error) });
    }
  });
});

process.stdin.on("end", async () => {
  await queue;
  if (laya) await laya.close().catch(() => undefined);
  process.exit(0);
});
