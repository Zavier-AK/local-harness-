import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";

let allowed: boolean | null = null;

/**
 * Tell the person something finished — but only if they are not already looking.
 *
 * The point of delegating is being able to look away; a notification while the window
 * is focused would just be noise on top of what the rail already shows.
 */
export async function notifyIfAway(title: string, body: string): Promise<void> {
  if (document.hasFocus()) return;
  try {
    if (allowed === null) {
      allowed = await isPermissionGranted();
      if (!allowed) allowed = (await requestPermission()) === "granted";
    }
    if (allowed) sendNotification({ title, body });
  } catch {
    // A notification is a courtesy; failing to send one must never break the app.
  }
}
