import { createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import {
  YAS_FAMILY_RELAY,
  YasConnection,
  YasNativeRelayTransport,
  YasNativeWorkspaceConnection,
  YasRelayClient,
  WorkspaceSessionDeviceStore,
  WorkspaceSessionStore,
  yasBrowserConnectionOptions,
  type YasRelayRoute,
  type YasTransport,
  type YasWasmModule,
} from "@yas-run/core";
import type { ConnectionSpec } from "./App";
import { Workspace } from "./Workspace";
import { t } from "./i18n";
import {
  boundedRelayRoutes,
  RelayConnectionCache,
} from "./relayTransportCache";
import { installPreviewNetBroker } from "./previewNetProtocol";
import { shellCapabilities } from "./shellCapabilities";
import { createWorkspaceSessionController } from "./workspaceSession";
import { reconcileWorkspaceSessionRelayConnections } from "./workspaceSessionRemotes";

const RELAY_RECONNECT_MIN_MS = 500;
const RELAY_RECONNECT_MAX_MS = 10_000;

/** The same home-server workspace shell over Edge or a full-control share. */
export function ConnectedWorkspace(props: {
  wasm: YasWasmModule;
  transport: YasTransport;
  workspaceSessionDeviceId: string;
  onAuthError: () => void;
}) {
  const transport = props.transport;
  const homeYas = new YasConnection(transport, yasBrowserConnectionOptions());
  const homeConnection = new YasNativeWorkspaceConnection(
    "local",
    homeYas,
    props.wasm,
    false,
  );
  let disposeConnectedResources: (() => void) | undefined;
  onCleanup(() => {
    try {
      disposeConnectedResources?.();
    } finally {
      try {
        homeConnection.close();
      } finally {
        homeConnection.dispose();
      }
    }
  });
  homeConnection.connect();
  const [relayCacheRevision, setRelayCacheRevision] = createSignal(0);
  const relayCache = new RelayConnectionCache(() =>
    setRelayCacheRevision((revision) => revision + 1),
  );
  const [relayRoutes, setRelayRoutes] = createSignal<readonly YasRelayRoute[]>(
    [],
  );
  const [relayClient, setRelayClient] = createSignal<YasRelayClient | null>(
    null,
  );
  const onHomeStatus = () => {
    if (transport.authRejected) props.onAuthError();
  };
  transport.addEventListener("statuschange", onHomeStatus);
  onCleanup(() => transport.removeEventListener("statuschange", onHomeStatus));

  createEffect(() => {
    let stopped = false;
    let stopRouteWatch: (() => void) | undefined;
    let routeWatchRetryTimer: ReturnType<typeof setTimeout> | undefined;
    let routeWatchRetryDelay = RELAY_RECONNECT_MIN_MS;
    let hasRouteSnapshot = false;

    const clearRouteWatchRetry = () => {
      if (routeWatchRetryTimer !== undefined) {
        clearTimeout(routeWatchRetryTimer);
        routeWatchRetryTimer = undefined;
      }
    };

    const stopWatching = () => {
      clearRouteWatchRetry();
      const stop = stopRouteWatch;
      stopRouteWatch = undefined;
      stop?.();
      hasRouteSnapshot = false;
    };
    const scheduleWatchRetry = () => {
      if (routeWatchRetryTimer !== undefined) return;
      const delay = routeWatchRetryDelay;
      routeWatchRetryDelay = Math.min(
        routeWatchRetryDelay * 2,
        RELAY_RECONNECT_MAX_MS,
      );
      routeWatchRetryTimer = setTimeout(() => {
        routeWatchRetryTimer = undefined;
        refresh();
      }, delay);
    };
    const refresh = async () => {
      stopWatching();
      try {
        await homeYas.connect();
        if (stopped) return;
        const relay = relayClient() ?? new YasRelayClient(homeYas);
        setRelayClient(relay);
        stopRouteWatch = relay.routes.subscribe((state) => {
          if (state.revision === 0n) {
            // A reset is not an empty route catalogue. Keep remote workspaces
            // alive while reconnecting.
            if (hasRouteSnapshot) scheduleWatchRetry();
            return;
          }
          hasRouteSnapshot = true;
          clearRouteWatchRetry();
          routeWatchRetryDelay = RELAY_RECONNECT_MIN_MS;
          setRelayRoutes(boundedRelayRoutes(state.routes));
        });
        await relay.routes.watch();
      } catch {
        if (stopped) return;
        stopWatching();
        if (homeYas.ready && !homeYas.families.has(YAS_FAMILY_RELAY))
          setRelayRoutes([]);
        // Keep the home connection usable if Relay is temporarily unavailable.
        scheduleWatchRetry();
      }
    };

    void refresh();
    onCleanup(() => {
      stopped = true;
      stopWatching();
    });
  });

  const sessionStore = new WorkspaceSessionStore(homeYas);
  const sessionDeviceStore = new WorkspaceSessionDeviceStore(
    homeYas,
    props.workspaceSessionDeviceId,
  );
  const sessionController = createWorkspaceSessionController({
    store: sessionStore,
    deviceStore: sessionDeviceStore,
    initialHash: location.hash,
  });

  void sessionController.start();

  // Only wake a dropped transport: reconnecting a live session interrupts
  // background audio and every nested Relay session.
  createEffect(() => {
    const wake = () => {
      if (document.visibilityState === "hidden") return;
      if (transport.status === "disconnected" || transport.status === "error") {
        homeConnection.reconnect();
      }
    };
    window.addEventListener("online", wake);
    document.addEventListener("visibilitychange", wake);
    onCleanup(() => {
      window.removeEventListener("online", wake);
      document.removeEventListener("visibilitychange", wake);
    });
  });

  const connections = createMemo<ConnectionSpec[]>(() => {
    relayCacheRevision();
    const next: ConnectionSpec[] = [
      {
        id: "local",
        label: t("common.local"),
        connection: homeConnection,
      },
    ];
    const relay = relayClient();
    next.push(
      ...reconcileWorkspaceSessionRelayConnections(
        relayRoutes(),
        sessionController.current()?.activeRemotes ?? [],
        relayCache,
        relay
          ? (route) => {
              const transport = new YasNativeRelayTransport(relay, route);
              return new YasNativeWorkspaceConnection(
                route.name,
                new YasConnection(transport, yasBrowserConnectionOptions()),
                props.wasm,
              );
            }
          : null,
      ),
    );
    return next;
  });

  // Preview sockets use the existing authenticated home or nested Relay session.
  const stopPreviewNetBroker = shellCapabilities().previews
    ? installPreviewNetBroker((dest) => {
        const spec = connections().find((candidate) => candidate.id === dest);
        return spec?.connection?.native.net ?? null;
      })
    : undefined;

  disposeConnectedResources = () => {
    stopPreviewNetBroker?.();
    sessionController.dispose();
    sessionDeviceStore.dispose();
    sessionStore.dispose();
    relayCache.clear();
    relayClient()?.dispose();
  };

  // Keep protocol connections mounted across keyed workspace-session screens:
  // a second parser on the same live transport cannot replay its handshake.
  return (
    <Workspace
      connections={connections}
      wasm={props.wasm}
      onAuthError={props.onAuthError}
      relayRoutes={() => relayRoutes()}
      workspaceSession={sessionController.binding}
      workspaceSessions={sessionController}
      transportOwnership="external"
    />
  );
}
