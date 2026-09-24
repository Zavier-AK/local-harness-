import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import VoiceHud from "./VoiceHud";
import "./styles.css";

/** The push-to-talk bar is a second window on the same bundle, told apart by its label. */
function isVoiceBar(): boolean {
  const internals = (window as unknown as { __TAURI_INTERNALS__?: { metadata?: { currentWindow?: { label?: string } } } })
    .__TAURI_INTERNALS__;
  return internals?.metadata?.currentWindow?.label === "voice-hud" || new URLSearchParams(location.search).has("hud");
}

const voiceBar = isVoiceBar();
if (voiceBar) document.body.classList.add("hud");

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>{voiceBar ? <VoiceHud /> : <App />}</React.StrictMode>,
);
