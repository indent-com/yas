import {
  YAS_FAMILY_CHANNEL,
  YAS_FAMILY_CLIENT,
  YAS_FAMILY_CORE,
  YAS_FAMILY_DESKTOP,
  YAS_FAMILY_ENV,
  YAS_FAMILY_EVENTS,
  YAS_FAMILY_EXTENSION,
  YAS_FAMILY_FONT,
  YAS_FAMILY_FS,
  YAS_FAMILY_GIT,
  YAS_FAMILY_KV,
  YAS_FAMILY_LSP,
  YAS_FAMILY_MEDIA,
  YAS_FAMILY_NET,
  YAS_FAMILY_PROCESS,
  YAS_FAMILY_RELAY,
  YAS_FAMILY_SELECTION,
  YAS_FAMILY_SURFACE,
  YAS_FAMILY_TERMINAL,
  YAS_FAMILY_TRANSFER,
  YAS_STATE_WATCH_RESUME,
  formatExtensionId,
  type YasClientAuxSubscription,
  type YasClientInfo,
} from "@yas-run/core";
import { t, tp } from "./i18n";
import * as g from "@yas-run/core";

export function formatTerminalViewSize(
  cols: number | null,
  rows: number | null,
): string {
  return cols == null || rows == null
    ? t("clients.sizeNotReported")
    : `${cols}×${rows}`;
}

export function formatSurfaceViewSize(
  width: number | null,
  height: number | null,
  scale120: number | null,
): string {
  if (width == null || height == null) return t("clients.sizeNotReported");
  if (scale120 == null) return `${width}×${height}`;
  // Round to 2dp and drop trailing fraction zeros. Chaining .replace(/0$/)
  // after stripping ".00" would also eat a zero off the integer part, turning
  // a 10× scale into "1×".
  const scale = String(Number((scale120 / 120).toFixed(2)));
  return `${width}×${height} @ ${scale}×`;
}

export function formatClientAge(totalSeconds: number): string {
  const seconds = Math.max(0, Math.floor(totalSeconds));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${seconds % 60}s`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ${minutes % 60}m`;
  const days = Math.floor(hours / 24);
  return `${days}d ${hours % 24}h`;
}

export function formatClientBandwidth(bytesPerSecond: number): string {
  const value = Math.max(0, bytesPerSecond);
  if (value < 1_000) return `${Math.round(value)} B/s`;
  if (value < 1_000_000) return `${formatRate(value / 1_000)} kB/s`;
  if (value < 1_000_000_000) return `${formatRate(value / 1_000_000)} MB/s`;
  return `${formatRate(value / 1_000_000_000)} GB/s`;
}

function formatRate(value: number): string {
  return value >= 100 ? value.toFixed(0) : value.toFixed(1).replace(/\.0$/, "");
}

/**
 * What to call a connection in the clients list.
 *
 * An extension is named by its definition, because "Client 7" tells a reader
 * nothing about the one row in the pane they did not open themselves. An
 * unnamed transient `ext run` falls back to `id:…`, the same handle the
 * extensions panel shows and the same one `yas ext status` accepts.
 */
export function formatClientLabel(client: YasClientInfo): string {
  const origin = client.origin;
  if (origin?.kind !== "extension") {
    return tp("clients.clientName", { id: client.id });
  }
  return origin.name || `id:${formatExtensionId(origin.extensionId)}`;
}

/** The short tag beside the label, or null for a connection that is only ever
 *  an ordinary client — most rows, which should stay quiet. */
export function formatClientOriginTag(client: YasClientInfo): string | null {
  switch (client.origin?.kind) {
    case "extension":
      return t("clients.extension");
    // A kind this build has no name for. Saying so beats calling it a browser,
    // and beats saying nothing where the row carries a Kick button.
    case "unknown":
      return t("clients.unrecognized");
    default:
      return null;
  }
}

/**
 * Which run of the extension this connection belongs to.
 *
 * Worth showing beside the age: a definition that keeps restarting is a
 * climbing attempt number on a row whose age keeps resetting — the two
 * together say "crash loop" where either alone says "new connection".
 *
 * The task id is deliberately not here. It is a random 32-bit handle, not an
 * ordinal, so `task 4035822760` would cost more attention than it repays; it
 * belongs in {@link formatExtensionTitle}, where someone correlating this row
 * with `yas ext status` can still find it.
 */
export function formatExtensionAttempt(client: YasClientInfo): string | null {
  const origin = client.origin;
  if (origin?.kind !== "extension") return null;
  return tp("clients.attempt", { count: String(origin.attempt) });
}

/** The coordinates that address this attempt elsewhere — `ext status`, the
 *  event stream — for the row's tooltip. */
export function formatExtensionTitle(client: YasClientInfo): string | null {
  const origin = client.origin;
  if (origin?.kind !== "extension") return null;
  return tp("clients.extensionTitle", {
    id: formatExtensionId(origin.extensionId),
    revision: String(origin.definitionRevision),
    task: String(origin.taskId),
  });
}

/**
 * The three states of this row's destructive button.
 *
 * Kicking an extension's connection ends the running attempt — a definition
 * with a restart policy will start another — so the button says what the click
 * does rather than leaving "Kick" to imply a peer being disconnected.
 */
export function formatKickAction(client: YasClientInfo): {
  idle: string;
  confirm: string;
  busy: string;
} {
  if (client.origin?.kind === "extension") {
    return {
      idle: t("clients.stopAttempt"),
      confirm: t("clients.confirmStop"),
      busy: t("clients.stopping"),
    };
  }
  return {
    idle: t("clients.kick"),
    confirm: t("clients.confirmKick"),
    busy: t("clients.kicking"),
  };
}

/**
 * What each family calls the thing a watch on it follows.
 *
 * Every family gets a name, not the six somebody happened to need: a client
 * watching Relay, Fonts and Channels used to read "Unknown 2 #0", "Unknown 36
 * #0", "Unknown 66 #0", which tells a reader nothing except that this build is
 * out of date. The ids come from the protocol, so a family added there and
 * missing here is caught by the test rather than shipped as another "Unknown".
 */
const FAMILY_LABELS: Readonly<Record<number, string>> = {
  [YAS_FAMILY_CORE]: "clients.family.session",
  [YAS_FAMILY_TRANSFER]: "clients.family.transfers",
  [YAS_FAMILY_RELAY]: "clients.family.relayRoutes",
  [YAS_FAMILY_TERMINAL]: "clients.family.terminals",
  [YAS_FAMILY_CLIENT]: "clients.family.clients",
  [YAS_FAMILY_SURFACE]: "clients.family.surfaces",
  [YAS_FAMILY_SELECTION]: "clients.family.selection",
  [YAS_FAMILY_DESKTOP]: "clients.family.desktop",
  [YAS_FAMILY_MEDIA]: "clients.family.media",
  [YAS_FAMILY_FONT]: "clients.family.fonts",
  [YAS_FAMILY_FS]: "clients.family.filesystem",
  [YAS_FAMILY_GIT]: "clients.family.git",
  [YAS_FAMILY_LSP]: "clients.family.lsp",
  [YAS_FAMILY_KV]: "clients.family.kv",
  [YAS_FAMILY_PROCESS]: "clients.family.processes",
  [YAS_FAMILY_NET]: "clients.family.network",
  [YAS_FAMILY_CHANNEL]: "clients.family.channels",
  [YAS_FAMILY_EXTENSION]: "clients.family.extensions",
  [YAS_FAMILY_EVENTS]: "clients.family.eventJournal",
  [YAS_FAMILY_ENV]: "clients.family.environment",
};

/**
 * The four families whose watch points at one resource rather than at the
 * family's whole collection. The rest report zero, and a zero printed against
 * a noun is worse than no noun.
 */
const FAMILY_RESOURCE: Readonly<Record<number, string>> = {
  [YAS_FAMILY_KV]: "clients.resource.namespace",
  [YAS_FAMILY_FS]: "clients.resource.root",
  [YAS_FAMILY_GIT]: "clients.resource.repository",
  [YAS_FAMILY_LSP]: "clients.resource.workspace",
};

/**
 * What one auxiliary subscription is watching.
 *
 * A bare "KV #0" is two numbers with no reading: the first is a family, the
 * second is whatever that family calls a resource. Name both, and carry the
 * watch's own ID, so two watches on one resource are two rows rather than one
 * repeated twice.
 *
 * The resource handle belongs to the watching connection's session, so it
 * can only be resolved through the optional diagnostics supplied by its owner.
 */
export function formatClientSubscription(
  kind: number,
  id: bigint,
  subscriptionId: number,
  detail?: Pick<
    YasClientAuxSubscription,
    | "resource"
    | "requestFlags"
    | "stateWatchFlags"
    | "settleMs"
    | "refsSettleMs"
  >,
): string {
  const watch = tp("clients.watch", { id: subscriptionId });
  const family = FAMILY_LABELS[kind]
    ? t(FAMILY_LABELS[kind])
    : tp("clients.familyUnknown", { id: kind });
  const resource = FAMILY_RESOURCE[kind] ? t(FAMILY_RESOURCE[kind]) : undefined;
  const diagnostics = formatSubscriptionDiagnostics(kind, detail);
  // KV namespace 0 is a handle the peer really opened; an FS/Git/LSP watch the
  // server has no mapping for reports zero, and there the noun is dropped.
  if (resource && (id !== 0n || kind === YAS_FAMILY_KV)) {
    return `${family} ${resource} ${id}${diagnostics} · ${watch}`;
  }
  return `${family}${diagnostics} · ${watch}`;
}

function formatSubscriptionDiagnostics(
  kind: number,
  detail:
    | Pick<
        YasClientAuxSubscription,
        | "resource"
        | "requestFlags"
        | "stateWatchFlags"
        | "settleMs"
        | "refsSettleMs"
      >
    | undefined,
): string {
  if (!detail) return "";
  const parts: string[] = [];
  if (detail.resource !== undefined) {
    const label = formatResourceBytes(detail.resource);
    parts.push(
      tp(
        kind === YAS_FAMILY_KV
          ? "clients.subscriptionPrefix"
          : kind === YAS_FAMILY_GIT || kind === YAS_FAMILY_FS
            ? "clients.subscriptionPath"
            : "clients.subscriptionResource",
        { value: label },
      ),
    );
  }
  if (
    detail.requestFlags !== undefined ||
    detail.stateWatchFlags !== undefined
  ) {
    const flags: string[] = [];
    const requestFlags = detail.requestFlags ?? 0;
    const stateFlags = detail.stateWatchFlags ?? 0;
    if (kind === YAS_FAMILY_GIT) flags.push(...gitWatchFlags(requestFlags));
    else if (kind === YAS_FAMILY_FS)
      flags.push(...namedFlags(requestFlags, FS_WATCH_FLAGS));
    else if (requestFlags !== 0)
      flags.push(
        tp("clients.subscriptionRequest", {
          value: requestFlags.toString(16),
        }),
      );
    if (stateFlags & YAS_STATE_WATCH_RESUME)
      flags.push(t("clients.subscriptionResume"));
    const unknownStateFlags = stateFlags & ~YAS_STATE_WATCH_RESUME;
    if (unknownStateFlags !== 0)
      flags.push(
        tp("clients.subscriptionState", {
          value: unknownStateFlags.toString(16),
        }),
      );
    parts.push(
      tp("clients.subscriptionFlags", {
        flags: flags.join(", ") || t("clients.none"),
      }),
    );
  }
  if (detail.settleMs !== undefined) {
    parts.push(
      tp("clients.subscriptionSettle", { value: String(detail.settleMs) }),
    );
  }
  if (kind === YAS_FAMILY_GIT && detail.refsSettleMs !== undefined) {
    parts.push(
      tp("clients.subscriptionRefsSettle", {
        value: String(detail.refsSettleMs),
      }),
    );
  }
  return parts.length === 0 ? "" : ` · ${parts.join(" · ")}`;
}

const FS_WATCH_FLAGS: readonly (readonly [number, string])[] = [
  [g.YAS_FS_WATCH_RECURSIVE, "recursive"],
  [g.YAS_FS_WATCH_CONTENT, "content"],
  [g.YAS_FS_WATCH_INCLUDE_HIDDEN, "hidden"],
  [g.YAS_FS_WATCH_GITIGNORE, "gitignore"],
  [g.YAS_FS_WATCH_DOT_IGNORE, "dot-ignore"],
  [g.YAS_FS_WATCH_EXCLUDE_GIT, "exclude-git"],
];

const GIT_DATASETS: readonly (readonly [number, string])[] = [
  [g.YAS_GIT_WATCH_HEAD, "head"],
  [g.YAS_GIT_WATCH_REFS, "refs"],
  [g.YAS_GIT_WATCH_REMOTES, "remotes"],
  [g.YAS_GIT_WATCH_OPERATION, "operation"],
  [g.YAS_GIT_WATCH_STATUS, "status"],
  [g.YAS_GIT_WATCH_UPSTREAMS, "upstreams"],
  [g.YAS_GIT_WATCH_STASHES, "stashes"],
  [g.YAS_GIT_WATCH_WORKTREE_GENERATION, "worktrees"],
];

function namedFlags(
  value: number,
  names: readonly (readonly [number, string])[],
): string[] {
  const result: string[] = [];
  for (const [flag, name] of names) {
    if (value & flag) result.push(name);
    value &= ~flag;
  }
  if (value) result.push(`0x${(value >>> 0).toString(16)}`);
  return result;
}

function gitWatchFlags(value: number): string[] {
  if (value & g.YAS_CLIENT_GIT_QUERY_WATCH) {
    const kind = (value >>> g.YAS_CLIENT_GIT_QUERY_KIND_SHIFT) & 0x7fff;
    const kinds: Record<number, string> = {
      [g.YAS_GIT_QUERY_RESOLVE]: "resolve",
      [g.YAS_GIT_QUERY_MERGE_BASE]: "merge-base",
      [g.YAS_GIT_QUERY_LOG]: "log",
      [g.YAS_GIT_QUERY_TREE]: "tree",
      [g.YAS_GIT_QUERY_BLOB]: "blob",
      [g.YAS_GIT_QUERY_DIFF]: "diff",
      [g.YAS_GIT_QUERY_PATCH]: "patch",
      [g.YAS_GIT_QUERY_INDEX]: "index",
      [g.YAS_GIT_QUERY_DISCOVER]: "discover",
      [g.YAS_GIT_QUERY_BLAME]: "blame",
      [g.YAS_GIT_QUERY_REFLOG]: "reflog",
      [g.YAS_GIT_QUERY_WORKTREES]: "worktrees",
    };
    const flags = [`query=${kinds[kind] ?? kind}`];
    if (value & 0xffff)
      flags.push(`query-flags=0x${(value & 0xffff).toString(16)}`);
    return flags;
  }
  const selection =
    g.YAS_CLIENT_GIT_WATCH_UNTRACKED | g.YAS_CLIENT_GIT_WATCH_IGNORED;
  const flags = namedFlags(value & ~selection, GIT_DATASETS);
  if (value & g.YAS_GIT_WATCH_STATUS) {
    flags.push(`untracked=${!!(value & g.YAS_CLIENT_GIT_WATCH_UNTRACKED)}`);
    flags.push(`ignored=${!!(value & g.YAS_CLIENT_GIT_WATCH_IGNORED)}`);
  }
  return flags;
}

const resourceDecoder = new TextDecoder("utf-8", { fatal: true });

function formatResourceBytes(resource: Uint8Array): string {
  if (resource.length === 0) return `<${t("clients.root")}>`;
  try {
    return JSON.stringify(resourceDecoder.decode(resource));
  } catch {
    return `0x${[...resource]
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("")}`;
  }
}
