import { createSignal } from "solid-js";
import type { ExtensionHost } from "./extensionRegistry";

type Operation = { busy: boolean; message: string; error?: boolean };
const createOperations = () => createSignal(new Map<string, Operation>());
const states = new WeakMap<
  ExtensionHost,
  ReturnType<typeof createOperations>
>();

/** Offers and Manage share progress and locks, even if the initiating pane closes. */
export function extensionOperations(host: ExtensionHost | null) {
  if (!host) return createOperations();
  let state = states.get(host);
  if (!state) {
    state = createOperations();
    states.set(host, state);
  }
  return state;
}
