import { TapButton } from "./TapButton";
import {
  createSignal,
  createEffect,
  ErrorBoundary,
  onCleanup,
  Show,
} from "solid-js";
import {
  YAS_WEBSOCKET_SUBPROTOCOL,
  YasEdgeWebSocketTransport,
} from "@yas-run/core";
import { YasWebTransportTransport } from "@yas-run/core/transports";
import type {
  YasTransport,
  YasWasmModule,
  YasWorkspace,
  YasWorkspaceConnection,
} from "@yas-run/core";
import { YasMark } from "./Logo";
import { themeFor } from "./theme";
import { t } from "./i18n";
import { ConnectedWorkspace } from "./ConnectedWorkspace";
import { PASSPHRASE_KEY } from "./passphrase-storage";
import { preferredPalette } from "./storage";
import { consumePassphraseFromHash } from "./workspaceSessionUrl";
import { getOrCreateWorkspaceSessionDeviceId } from "./workspaceSessionDevice";
import {
  discoverEdgeWebTransport,
  fetchEdgeCertificateHash,
  type EdgeWebTransportConfig,
} from "./edgeWebTransport";

function readPassphrase(): string | null {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(PASSPHRASE_KEY);
  } catch {}

  const consumed = consumePassphraseFromHash(location.hash);
  if (!consumed.found) return stored;

  // First contact — secret is being delivered via the URL fragment. Move it
  // to localStorage and strip it from the URL so it does not end up in
  // browser history or get re-shared accidentally.
  const newHash = consumed.hash;
  const newUrl =
    location.pathname + location.search + (newHash ? `#${newHash}` : "");
  history.replaceState(null, "", newUrl);
  if (consumed.passphrase) {
    try {
      localStorage.setItem(PASSPHRASE_KEY, consumed.passphrase);
    } catch {}
    return consumed.passphrase;
  }
  return stored;
}

readPassphrase();

export interface ConnectionSpec {
  id: string;
  label: string;
  /** Prebuilt typed YAS product connection for production browser paths. */
  connection?: YasWorkspaceConnection;
  /** Custom/embed transport. Omitted by native product connections. */
  transport?: YasTransport;
  /** Called when Workspace has materialized (or removed) the connection. */
  onConnection?: (connection: WorkspaceConnection | null) => void;
  /** The connection is read-only (an `.ro` share): the server refuses
   *  writes, so its terminals render without input affordances rather
   *  than swallowing keystrokes silently. */
  readOnly?: boolean;
}

type WorkspaceConnection = NonNullable<
  ReturnType<YasWorkspace["getConnection"]>
>;

/** The protocol-transparent edge endpoint connected only to the home server. */
function edgeWsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return proto + "//" + location.host + "/edge";
}

export function App(props: { wasm: YasWasmModule }) {
  const [passphrase, setPassphrase] = createSignal(readPassphrase());
  const [workspaceSessionDeviceId, setWorkspaceSessionDeviceId] = createSignal<
    string | null
  >(null);
  const [workspaceSessionDeviceError, setWorkspaceSessionDeviceError] =
    createSignal<unknown>(null);
  const [edgeWebTransport, setEdgeWebTransport] = createSignal<
    EdgeWebTransportConfig | null | undefined
  >(undefined);
  let disposed = false;
  void getOrCreateWorkspaceSessionDeviceId().then(
    (id) => {
      if (!disposed) setWorkspaceSessionDeviceId(id);
    },
    (error) => {
      if (!disposed) setWorkspaceSessionDeviceError(error);
    },
  );
  void discoverEdgeWebTransport().then(setEdgeWebTransport);
  onCleanup(() => {
    disposed = true;
  });

  createEffect(() => {
    const onHashChange = () => {
      setPassphrase(readPassphrase());
    };
    window.addEventListener("hashchange", onHashChange);
    onCleanup(() => window.removeEventListener("hashchange", onHashChange));
  });

  function handleAuth(pass: string) {
    try {
      localStorage.setItem(PASSPHRASE_KEY, pass);
    } catch {}
    setPassphrase(pass);
  }

  function handleAuthError() {
    try {
      localStorage.removeItem(PASSPHRASE_KEY);
    } catch {}
    setPassphrase(null);
  }

  // Last-resort boundary. Individual tiles contain their own failures
  // (see YasTile), but a throw in the shell — the dock, the status bar,
  // LayoutContainer itself — has nothing above it and would leave a blank
  // page with the reason only in the console. Show it, and offer the one
  // action that reliably helps.
  return (
    <ErrorBoundary fallback={(err: unknown) => <AppCrash err={err} />}>
      <Show
        when={workspaceSessionDeviceError()}
        fallback={
          <Show when={passphrase()} fallback={<AuthApp onAuth={handleAuth} />}>
            {(pass) => (
              <Show
                when={
                  workspaceSessionDeviceId() && edgeWebTransport() !== undefined
                    ? workspaceSessionDeviceId()
                    : null
                }
                fallback={<WorkspaceSessionDeviceLoading />}
              >
                {(deviceId) => (
                  <ConnectedApp
                    wasm={props.wasm}
                    passphrase={pass()}
                    workspaceSessionDeviceId={deviceId()}
                    edgeWebTransport={edgeWebTransport() ?? null}
                    onAuthError={handleAuthError}
                  />
                )}
              </Show>
            )}
          </Show>
        }
      >
        {(error) => <AppCrash err={error()} />}
      </Show>
    </ErrorBoundary>
  );
}

/**
 * What is on screen before the workspace is.
 *
 * The mark and nothing else. A sentence about fetching sessions from the home
 * server is a progress report for a step that is usually over before it can be
 * read, and it is the first thing anyone sees of YAS — so it is the mark, at
 * the weight of the surrounding text, and no words.
 */
function WorkspaceSessionDeviceLoading() {
  const theme = themeFor(preferredPalette());
  return (
    <main
      role="status"
      aria-label={t("app.loading")}
      style={{
        display: "grid",
        "place-items": "center",
        height: "100%",
        color: theme.dimFg,
        "background-color": theme.bg,
      }}
    >
      <YasMark size={72} />
    </main>
  );
}

/** The shell failed. Deliberately dependency-free: whatever broke may well
 *  be the theme or the workspace this would otherwise read from. */
function AppCrash(props: { err: unknown }) {
  const message = () =>
    props.err instanceof Error
      ? `${props.err.name}: ${props.err.message}\n\n${props.err.stack ?? ""}`
      : String(props.err);
  return (
    <div
      style={{
        position: "fixed",
        inset: "0",
        display: "flex",
        "flex-direction": "column",
        gap: "12px",
        padding: "24px",
        overflow: "auto",
        background: "#1a1a1a",
        color: "#e0e0e0",
        "font-family": "ui-monospace, monospace",
        "font-size": "13px",
      }}
    >
      <b style={{ color: "#f66" }}>{t("app.crashTitle")}</b>
      <div>{t("app.crashRecovery")}</div>
      <div>
        <TapButton
          onClick={() => location.reload()}
          style={{
            padding: "4px 10px",
            background: "#2a2a2a",
            color: "#e0e0e0",
            border: "1px solid #808080",
            "border-radius": "3px",
            cursor: "pointer",
            font: "inherit",
          }}
        >
          {t("common.reload")}
        </TapButton>
      </div>
      <pre
        style={{
          "white-space": "pre-wrap",
          "word-break": "break-word",
          color: "#808080",
          margin: "0",
        }}
      >
        {message()}
      </pre>
    </div>
  );
}

function ConnectedApp(props: {
  wasm: YasWasmModule;
  passphrase: string;
  workspaceSessionDeviceId: string;
  edgeWebTransport: EdgeWebTransportConfig | null;
  onAuthError: () => void;
}) {
  // The edge exposes exactly one native home connection. Relay creates nested
  // server transports inside that authenticated YAS session.
  const edgeTransport = props.edgeWebTransport
    ? new YasWebTransportTransport(
        props.edgeWebTransport.url,
        props.passphrase,
        { serverCertificateHash: fetchEdgeCertificateHash },
      )
    : new YasEdgeWebSocketTransport(edgeWsUrl(), props.passphrase);
  return (
    <ConnectedWorkspace
      transport={edgeTransport}
      wasm={props.wasm}
      workspaceSessionDeviceId={props.workspaceSessionDeviceId}
      onAuthError={props.onAuthError}
    />
  );
}

function AuthApp(props: { onAuth: (pass: string) => void }) {
  const [authError, setAuthError] = createSignal<string | null>(null);

  function connect(pass: string) {
    setAuthError(null);
    const ws = new WebSocket(edgeWsUrl(), YAS_WEBSOCKET_SUBPROTOCOL);
    let authed = false;
    let throttled = false;

    ws.onopen = () => {
      ws.send(pass);
    };

    ws.onmessage = (ev) => {
      const msg = String(ev.data);
      if (msg === "ok") {
        authed = true;
        ws.close();
        props.onAuth(pass);
      } else if (msg === "busy") {
        // Throttled before the passphrase was even checked. Saying
        // "authentication failed" here sends the user hunting for a wrong
        // credential when the only thing to do is wait.
        throttled = true;
        setAuthError(t("auth.busy"));
      } else if (msg.startsWith("error:")) {
        // Authentication succeeded, but the fixed home server could not be
        // reached. Preserve the credential and report the actual boundary
        // failure instead of misdiagnosing it as a bad passphrase.
        throttled = true;
        const detail = msg.slice("error:".length).trim();
        setAuthError(detail || t("auth.homeUnavailable"));
      }
    };

    ws.onerror = () => {};

    ws.onclose = () => {
      if (!authed && !throttled) {
        setAuthError(t("auth.failed"));
      }
    };
  }

  return <AuthScreen error={authError()} onSubmit={(pass) => connect(pass)} />;
}

function AuthScreen(props: {
  error: string | null;
  onSubmit: (pass: string) => void;
}) {
  const theme = themeFor(preferredPalette());
  let inputRef!: HTMLInputElement;

  return (
    <main
      style={{
        display: "flex",
        "align-items": "center",
        "justify-content": "center",
        height: "100%",
        "background-color": theme.bg,
      }}
    >
      <form
        style={{
          display: "flex",
          "flex-direction": "column",
          "align-items": "center",
          gap: "1em",
          color: theme.dimFg,
        }}
        onSubmit={(e) => {
          e.preventDefault();
          const v = inputRef?.value;
          if (v) props.onSubmit(v);
        }}
      >
        {/* The first thing anyone sees of YAS, and the only thing on this
            screen that is not a password field. */}
        <YasMark size={72} />
        <input
          ref={inputRef}
          name="yas-passphrase"
          type="password"
          placeholder={t("auth.placeholder")}
          autofocus
          style={{
            padding: "0.5em 0.75em",
            "font-size": "1em",
            border: "1px solid #444",
            outline: "none",
            width: "20em",
            "font-family": "inherit",
            "background-color": theme.solidInputBg,
            color: theme.fg,
          }}
        />
        <Show when={props.error}>
          {(err) => (
            <output style={{ color: theme.errorText, "font-size": "0.85em" }}>
              {err()}
            </output>
          )}
        </Show>
      </form>
    </main>
  );
}
