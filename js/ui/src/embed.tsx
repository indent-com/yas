/**
 * Embedding entry point: the full YAS workspace as a mountable component,
 * for hosts that are not the app shell — yas.run's share page is the first.
 *
 * A full-control home connection uses the same ConnectedWorkspace shell as
 * App, including durable workspace sessions and Relay remotes. Read-only
 * shares and custom connection lists mount Workspace directly.
 */

import { render } from "solid-js/web";
import type { YasTransport, YasWasmModule } from "@yas-run/core";
import { Workspace } from "./Workspace";
import { ConnectedWorkspace } from "./ConnectedWorkspace";
import { setDefaultFont } from "./storage";
import { setFontCatalog } from "./fontCatalog";
import { setShellCapabilities } from "./shellCapabilities";
import type { ConnectionSpec } from "./App";
import type { ShellCapabilities } from "./shellCapabilities";
import type { FontChoice } from "./fontCatalog";

export type { ConnectionSpec, FontChoice };
export { shareTransport } from "./nativeShareTransport";
export { getOrCreateWorkspaceSessionDeviceId } from "./workspaceSessionDevice";

interface EmbedPresentationOptions {
  wasm: YasWasmModule;
  /** Remotes default on for a home connection, off for fixed connection lists.
   *  Previews default off: the host must provide the preview service worker. */
  capabilities?: Partial<ShellCapabilities>;
  /** Monospace stack to default to, for a host that ships its own webfont
   *  and wants the workspace on the same face as the page around it. The
   *  visitor's own choice still wins; this replaces the platform fallback
   *  the app-served client is right to use. */
  fontFamily?: string;
  /** Faces bundled into the host page, offered as the font picker's whole
   *  menu. Without these the picker searches families the page cannot fetch
   *  and accepts names it cannot honour. */
  fonts?: readonly FontChoice[];
  /** A transport authenticated once and then refused — a revoked share
   *  passphrase, an expired link. The host owns the surrounding page, so it
   *  owns the apology. */
  onAuthError?: () => void;
}

export type EmbedOptions = EmbedPresentationOptions &
  (
    | {
        /** Full-control home server, with the regular app's workspace manager. */
        home: { transport: YasTransport; workspaceSessionDeviceId: string };
        connections?: never;
      }
    | {
        /** Fixed connections, including read-only shares; each owns its transport. */
        connections: ConnectionSpec[] | (() => ConnectionSpec[]);
        home?: never;
      }
  );

/**
 * Mount the workspace into `root` and return a disposer.
 *
 * The container must have a definite height — the workspace fills it. The
 * app shell's global CSS (border-box sizing, `line-height: 1`, no
 * overscroll) is applied to the container here rather than assumed of the
 * page: YAS is a terminal first and every pane sits on that tight rhythm,
 * but an embedding page has typography of its own that a global reset
 * would trample.
 */
export function mountYasWorkspace(
  root: HTMLElement,
  opts: EmbedOptions,
): () => void {
  setShellCapabilities({
    remotes: !!opts.home,
    previews: false,
    ...opts.capabilities,
  });
  if (opts.fontFamily) setDefaultFont(opts.fontFamily);
  if (opts.fonts) setFontCatalog(opts.fonts);
  root.style.lineHeight = "1";
  root.style.boxSizing = "border-box";
  root.style.overflow = "hidden";
  root.style.overscrollBehavior = "none";
  const dispose = render(
    () =>
      opts.home ? (
        <ConnectedWorkspace
          transport={opts.home.transport}
          workspaceSessionDeviceId={opts.home.workspaceSessionDeviceId}
          wasm={opts.wasm}
          onAuthError={opts.onAuthError ?? (() => {})}
        />
      ) : (
        <Workspace
          connections={opts.connections}
          wasm={opts.wasm}
          onAuthError={opts.onAuthError ?? (() => {})}
        />
      ),
    root,
  );
  return dispose;
}
