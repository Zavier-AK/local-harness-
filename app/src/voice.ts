import { invoke } from "@tauri-apps/api/core";
import type { Heard, QuotaReport, Spoken, VoiceAction } from "./types";

/**
 * What voice does in the main window. Every harness action runs through the same
 * handler as its button, so voice cannot do anything a click could not.
 */
export type VoiceHandlers = {
  navigate: (pane: "chat" | "plan" | "night" | "preview" | "tools" | "settings") => void;
  switchProject: (root: string) => void;
  openWorker: (id: string) => void;
  askHead: (text: string) => void;
  stopTurn: () => void;
  stopWorker: (id: string) => void;
  resolveMerge: (id: string, approve: boolean) => void;
  undoMerge: (id: string) => void;
  decide: (id: string, approve: boolean, reason?: string) => void;
  setAutonomy: (level: "ask" | "review" | "land_safe" | "land_most") => void;
  runPlan: () => void;
  discardPlan: () => void;
  planFeedback: (note: string) => void;
  stopNight: () => void;
  proposeNight: () => void;
  nightSetup: (goal: string | null) => void;
};

export function dispatch(action: VoiceAction, on: VoiceHandlers): void {
  switch (action.action) {
    case "navigate":
      return on.navigate(action.pane);
    case "switch_project":
      return on.switchProject(action.project);
    case "open_worker":
      return on.openWorker(action.worker);
    case "ask_head":
      return on.askHead(action.text);
    case "stop_turn":
      return on.stopTurn();
    case "stop_worker":
      return on.stopWorker(action.worker);
    case "approve_merge":
      return on.resolveMerge(action.worker, true);
    case "reject_merge":
      return on.resolveMerge(action.worker, false);
    case "undo_merge":
      return on.undoMerge(action.worker);
    case "approve_delegation":
      return on.decide(action.worker, true);
    case "decline_delegation":
      return on.decide(action.worker, false, action.reason ?? undefined);
    case "set_autonomy":
      return on.setAutonomy(action.level);
    case "run_plan":
      return on.runPlan();
    case "discard_plan":
      return on.discardPlan();
    case "plan_feedback":
      return on.planFeedback(action.note);
    case "stop_night":
      return on.stopNight();
    case "propose_night":
      return on.proposeNight();
    case "night_setup":
      return on.nightSetup(action.goal);
    // Status is answered in words, computer actions run in the shell, and yes/no
    // answers are settled there too: none of them reach this window.
    default:
      return;
  }
}

/** The one line to show, and to say aloud, for what was heard. */
export function replyLine(heard: Heard): string | null {
  const { interpretation, pending, done, error } = heard;
  if (error) return error;
  const outcome = interpretation.outcome;
  // The agent's own answer says what it found and that a yes is waiting; the buttons
  // name the step.
  if (pending && interpretation.source === "agent" && outcome.outcome === "reply") return outcome.text;
  if (pending) return `${pending.describe}? Say yes to go ahead.`;
  switch (outcome.outcome) {
    case "clarify":
      return outcome.question;
    case "reply":
      return outcome.text;
    case "to_head":
      return "In the chat box — edit it and send when ready.";
    case "nothing":
      return null;
    case "act":
      if (outcome.action.action === "status") return interpretation.reply;
      return done ?? outcome.describe;
  }
}

function hours(seconds: number): string {
  const h = Math.round(seconds / 3600);
  return h <= 1 ? "within the hour" : `in ${h} hours`;
}

/** Limits, in a sentence — the one status question the shell leaves to the app. */
export function limitsLine(report: QuotaReport | null): string {
  if (!report) return "I can't read your limits right now.";
  const parts: string[] = [];
  for (const provider of report.providers) {
    const windows = provider.windows.filter((w) => Number.isFinite(w.used_percent));
    if (windows.length === 0) continue;
    const now = Date.now() / 1000;
    const said = windows.map((w) => {
      const resets = w.resets_at ? `, resets ${hours(Math.max(0, w.resets_at - now))}` : "";
      return `${w.label} ${Math.round(w.used_percent)}% used${resets}`;
    });
    parts.push(`${provider.provider}: ${said.join("; ")}`);
  }
  if (report.rate_limited.length > 0) parts.push(`${report.rate_limited.join(", ")} is at its limit`);
  return parts.length > 0 ? `${parts.join(". ")}.` : "No provider has reported its limits yet.";
}

/** Say it with the Mac's own voices. Anything still being said is cut off. */
/** macOS voices in order of preference when none is chosen: British men, best first. */
const BRITISH = ["Daniel (Premium)", "Jamie (Premium)", "Daniel (Enhanced)", "Arthur (Enhanced)", "Oliver (Enhanced)", "Arthur", "Daniel"];

/** The system voice to use: the one named, else the best British one installed. */
export function systemVoice(name: string): SpeechSynthesisVoice | null {
  if (!("speechSynthesis" in window)) return null;
  const voices = window.speechSynthesis.getVoices();
  if (name) {
    const chosen = voices.find((v) => v.name === name);
    if (chosen) return chosen;
  }
  for (const wanted of BRITISH) {
    const found = voices.find((v) => v.name === wanted);
    if (found) return found;
  }
  return voices.find((v) => v.lang === "en-GB") ?? null;
}

let playing: HTMLAudioElement | null = null;

function stopSpeaking() {
  playing?.pause();
  playing = null;
  if ("speechSynthesis" in window) window.speechSynthesis.cancel();
}

function sayWithSystem(text: string, name: string, rate: number) {
  if (!("speechSynthesis" in window)) return;
  const utterance = new SpeechSynthesisUtterance(text);
  const voice = systemVoice(name);
  if (voice) {
    utterance.voice = voice;
    utterance.lang = voice.lang;
  }
  utterance.rate = rate;
  window.speechSynthesis.speak(utterance);
}

/**
 * Say a reply aloud: the natural voice (Kokoro, on the Mac) when it's chosen and ready,
 * otherwise the best system voice. Never silent because of the natural voice.
 */
export async function speak(text: string): Promise<void> {
  if (!text) return;
  stopSpeaking();
  let spoken: Spoken | null = null;
  try {
    spoken = await invoke<Spoken>("voice_say", { text });
  } catch {
    spoken = null;
  }
  if (spoken?.wav) {
    const audio = new Audio(`data:audio/wav;base64,${spoken.wav}`);
    playing = audio;
    try {
      await audio.play();
      return;
    } catch {
      playing = null;
    }
  }
  sayWithSystem(text, spoken?.system_voice ?? "", spoken?.rate ?? 1);
}
