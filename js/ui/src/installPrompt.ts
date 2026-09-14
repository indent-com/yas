/**
 * The retained PWA install prompt, so the Cmd+K overlay can offer
 * "Install App". The browser fires `beforeinstallprompt` only when the
 * manifest is valid and the app isn't already installed. Do not cancel its
 * default action: Chromium can then show its native install affordance while
 * the retained event still backs the explicit menu action.
 *
 * Keep the shared prompt separate from the application entry point so menu
 * consumers do not import startup side effects.
 */

interface BeforeInstallPromptEvent extends Event {
  prompt(): Promise<void>;
}

let deferred: BeforeInstallPromptEvent | null = null;

window.addEventListener("beforeinstallprompt", (e) => {
  deferred = e as BeforeInstallPromptEvent;
});
window.addEventListener("appinstalled", () => {
  deferred = null;
});

export function getInstallPrompt(): BeforeInstallPromptEvent | null {
  return deferred;
}

export function clearInstallPrompt(): void {
  deferred = null;
}
