/** Declarative registry requirements plus a small set of read-only host probes. */
import * as yas from "@yas-run/core";
import type { RegistryEntry } from "./extensionRegistry";

export interface ExtensionRequirements {
  version: 1;
  runtime: "wasmi" | "quickjs";
  commandProvider: boolean;
  platforms?: string[];
  families: {
    name: string;
    version: number;
    operations: string[];
    events?: string[];
  }[];
  probes: string[];
  activation?: string;
  minMemoryBytes?: number;
}

export type Viability = {
  status: "available" | "limited" | "unavailable" | "unknown";
  reasons: string[];
};
export const canRecommend = (result: Viability | undefined) =>
  result?.status === "available" || result?.status === "limited";

export function extensionOfferFingerprint(
  hello: yas.YasServerHello,
  entries: readonly RegistryEntry[],
  url: string,
): string {
  // Reconnects retain dismissal, but a new module digest is a new update offer.
  const text = JSON.stringify([
    url,
    yas.serverExtensionSupport(hello.extensions),
    yas.serverPlatform(hello.extensions),
    hello.families.map((f) => [
      f.family,
      f.version,
      f.runtimeState,
      f.operations,
      f.limits.map((limit) => [limit.tag, Array.from(limit.value)]),
    ]),
    entries.map((entry) => [entry.name, entry.blake3, entry.requirements]),
  ]);
  let hash = 2166136261;
  for (let i = 0; i < text.length; i++)
    hash = Math.imul(hash ^ text.charCodeAt(i), 16777619);
  return (hash >>> 0).toString(16);
}

export function parseRequirements(
  value: unknown,
): ExtensionRequirements | undefined {
  if (!value || typeof value !== "object") return;
  const r = value as ExtensionRequirements;
  const strings = (v: unknown): v is string[] =>
    Array.isArray(v) && v.every((s) => typeof s === "string");
  if (
    r.version !== 1 ||
    !["wasmi", "quickjs"].includes(r.runtime) ||
    typeof r.commandProvider !== "boolean" ||
    !strings(r.probes) ||
    (r.platforms !== undefined && !strings(r.platforms)) ||
    (r.activation !== undefined && typeof r.activation !== "string") ||
    (r.minMemoryBytes !== undefined &&
      (!Number.isSafeInteger(r.minMemoryBytes) || r.minMemoryBytes < 0)) ||
    !Array.isArray(r.families) ||
    !r.families.every(
      (f) =>
        f &&
        typeof f.name === "string" &&
        Number.isInteger(f.version) &&
        f.version > 0 &&
        strings(f.operations) &&
        (f.events === undefined || strings(f.events)),
    )
  )
    return;
  return r;
}

const result = (
  status: Viability["status"],
  ...reasons: string[]
): Viability => ({ status, reasons });
const constants = yas as unknown as Record<string, unknown>;

export function checkExtensionCapabilities(
  hello: yas.YasServerHello,
  entry: RegistryEntry,
): Viability {
  const requirements = entry.requirements;
  if (!requirements)
    return result(
      "unknown",
      "Registry does not declare supported requirements.",
    );
  const support = yas.serverExtensionSupport(hello.extensions);
  if (support === null)
    return result(
      "unknown",
      "Server does not advertise extension installation policy and runtimes.",
    );
  if (!(support & yas.YAS_CORE_EXTENSION_SUPPORT_PERSISTENT))
    return result(
      "unavailable",
      "Persistent extensions are disabled on this server.",
    );
  const runtime =
    requirements.runtime === "wasmi"
      ? yas.YAS_CORE_EXTENSION_SUPPORT_WASMI
      : yas.YAS_CORE_EXTENSION_SUPPORT_QUICKJS;
  if (!(support & runtime))
    return result(
      "unavailable",
      `${requirements.runtime} runtime is unavailable.`,
    );
  if (
    requirements.commandProvider &&
    !(support & yas.YAS_CORE_EXTENSION_SUPPORT_COMMAND_PROVIDER)
  )
    return result(
      "unavailable",
      "Extension command providers are unavailable.",
    );
  if (requirements.platforms) {
    const platform = yas.serverPlatform(hello.extensions);
    if (!platform) return result("unknown", "Server platform is unknown.");
    if (!requirements.platforms.includes(platform.os))
      return result(
        "unavailable",
        `Requires ${requirements.platforms.join(" or ")}; server runs ${platform.os}.`,
      );
  }
  const warnings: string[] = [];
  const families: ExtensionRequirements["families"] = [
    {
      name: "extension",
      version: 1,
      operations: [
        "WATCH",
        "DEPLOY",
        "CONTROL",
        "OBJECT_BEGIN",
        "OBJECT_COMMIT",
      ],
    },
    ...requirements.families,
  ];
  for (const requirement of families) {
    const prefix = requirement.name.toUpperCase();
    const familyId = constants[`YAS_FAMILY_${prefix}`];
    if (typeof familyId !== "number")
      return result("unknown", `Unknown required family: ${requirement.name}.`);
    const family = hello.families.find((f) => f.family === familyId);
    if (
      !family ||
      family.version !== requirement.version ||
      family.runtimeState === yas.YAS_CORE_RUNTIME_UNAVAILABLE
    )
      return result(
        "unavailable",
        `Requires available ${requirement.name} v${requirement.version}.`,
      );
    for (const operation of requirement.operations) {
      const kind = constants[`YAS_${prefix}_${operation}`];
      if (typeof kind !== "number")
        return result(
          "unknown",
          `Unknown required operation: ${requirement.name}.${operation}.`,
        );
      if (
        !family.operations.some(
          (op) =>
            op.kind === kind &&
            op.class === yas.YAS_CLASS_REQUEST &&
            op.direction & yas.YAS_CORE_DIRECTION_ACCEPTS,
        )
      )
        return result(
          "unavailable",
          `Server does not allow ${requirement.name}.${operation}.`,
        );
    }
    for (const event of requirement.events ?? []) {
      const kind = constants[`YAS_${prefix}_${event}`];
      if (typeof kind !== "number")
        return result(
          "unknown",
          `Unknown required event: ${requirement.name}.${event}.`,
        );
      if (
        !family.operations.some(
          (op) =>
            op.kind === kind &&
            op.class === yas.YAS_CLASS_EVENT &&
            op.direction & yas.YAS_CORE_DIRECTION_SENDS,
        )
      )
        return result(
          "unavailable",
          `Server does not publish ${requirement.name}.${event}.`,
        );
    }
    if (family.runtimeState === yas.YAS_CORE_RUNTIME_DEGRADED)
      warnings.push(`${requirement.name} is degraded.`);
  }
  const family = hello.families.find(
    (f) => f.family === yas.YAS_FAMILY_EXTENSION,
  )!;
  try {
    const limits = yas.extensionLimitsFromExtensions(family.limits);
    if (!Number.isSafeInteger(entry.bytes) || entry.bytes <= 0)
      return result(
        "unknown",
        "Registry does not declare a valid module size.",
      );
    if (BigInt(entry.bytes) > limits.maxObjectBytes)
      return result(
        "unavailable",
        "Module exceeds the server's object size limit.",
      );
    if (
      requirements.minMemoryBytes !== undefined &&
      BigInt(requirements.minMemoryBytes) > limits.maxMemoryBytes
    )
      return result(
        "unavailable",
        "Module requires more memory than the server permits.",
      );
  } catch {
    return result("unknown", "Server extension limits could not be read.");
  }
  return result(warnings.length ? "limited" : "available", ...warnings);
}

// Registry metadata selects a probe ID, never an arbitrary command or script.
const probes: Record<string, string> = {
  systemd: `
command -v systemctl >/dev/null 2>&1 || exit 20
systemctl --system list-units --all --no-pager --no-legend >/dev/null 2>&1 && echo system
if [ -z "\${XDG_RUNTIME_DIR:-}" ] || [ ! -S "$XDG_RUNTIME_DIR/bus" ]; then XDG_RUNTIME_DIR="/run/user/$(id -u)"; fi
export XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
systemctl --user list-units --all --no-pager --no-legend >/dev/null 2>&1 && echo user
command -v gdbus >/dev/null 2>&1 && echo signals
journalctl --no-pager -n 0 >/dev/null 2>&1 && echo journal
exit 0`,
  "muster-config": `
dir="\${YAS_MUSTER_DIR-\${XDG_CONFIG_HOME:-\${HOME:-/root}/.config}/yas/instances/$1/muster}"
if [ -e "$dir" ]; then
  [ -d "$dir" ] || exit 20
  [ -r "$dir" ] && [ -x "$dir" ] || exit 21
else
  while [ ! -e "$dir" ]; do parent=$(dirname "$dir"); [ "$parent" != "$dir" ] || exit 21; dir=$parent; done
  [ -d "$dir" ] && [ -w "$dir" ] && [ -x "$dir" ] || exit 21
fi
echo ready`,
  "xdg-applications": `
set -f
IFS=:
for dir in "\${XDG_DATA_HOME:-$HOME/.local/share}" \${XDG_DATA_DIRS:-/usr/local/share:/usr/share}; do
  if [ -d "$dir/applications" ]; then
    [ -r "$dir/applications" ] && [ -x "$dir/applications" ] || exit 21
    echo applications
  fi
done
exit 0`,
};

export function interpretProbe(
  id: string,
  code: number,
  output: string,
): Viability {
  if (code === 21)
    return result("unknown", "Required host paths could not be accessed.");
  if (code === 20)
    return result(
      "unavailable",
      id === "systemd"
        ? "systemctl is not installed."
        : "Configuration path is not a directory.",
    );
  if (code !== 0) return result("unknown", `Host probe failed (exit ${code}).`);
  const lines = new Set(output.trim().split(/\s+/));
  if (id === "systemd") {
    if (!lines.has("system") && !lines.has("user"))
      return result(
        "unknown",
        "Neither system nor user systemd manager could be queried.",
      );
    const limitations = [
      ...(!lines.has("system") ? ["System manager is inaccessible."] : []),
      ...(!lines.has("user") ? ["User manager is inaccessible."] : []),
      ...(!lines.has("signals")
        ? ["gdbus is absent; unit state will use polling."]
        : []),
      ...(!lines.has("journal")
        ? ["Journal access is unavailable; unit monitoring still works."]
        : []),
    ];
    return result(limitations.length ? "limited" : "available", ...limitations);
  }
  if (id === "xdg-applications" && !lines.has("applications"))
    return result(
      "limited",
      "No XDG application directories found; check XDG_DATA_DIRS.",
    );
  return result("available");
}

/** Bounded output and lifetime, including a SPAWN result that arrives late. */
export async function runExtensionProbe(
  native: yas.YasNativeProductFamilies,
  id: string,
  signal?: AbortSignal,
  readEnvironment?: () => Promise<yas.YasEnvSnapshot>,
): Promise<Viability> {
  const script = Object.hasOwn(probes, id) ? probes[id] : undefined;
  if (!script) return result("unknown", `Unknown host probe: ${id}.`);
  const process = native.process;
  if (!process)
    return result(
      "unknown",
      "Process execution is unavailable for host checks.",
    );
  let streams: yas.YasProcessStreams | undefined;
  let cancelled = false;
  const operationId = () => crypto.getRandomValues(new Uint8Array(16));
  const kill = () => {
    if (!streams) return;
    try {
      streams.stdout.reset();
    } catch {
      /* Already invalidated. */
    }
    try {
      streams.stderr?.reset();
    } catch {
      /* Already invalidated. */
    }
    void process
      .control({
        processHandle: streams.processHandle,
        operationId: operationId(),
        action: yas.YAS_PROCESS_CONTROL_KILL,
        value: 0,
      })
      .catch(() => undefined);
  };
  let timer: ReturnType<typeof setTimeout> | undefined;
  let abort = () => {};
  const timeout = new Promise<never>((_, reject) => {
    abort = () => {
      cancelled = true;
      kill();
      reject(new Error("Host check cancelled."));
    };
    timer = setTimeout(() => {
      cancelled = true;
      kill();
      reject(new Error("Host check timed out."));
    }, 5000);
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
  });
  try {
    const work = async () => {
      if (cancelled) throw new Error("Host check cancelled.");
      const encode = (s: string) => new TextEncoder().encode(s);
      const env = native.env;
      if (!env)
        throw new Error("Server environment is unavailable for host checks.");
      const snapshot = await (readEnvironment ? readEnvironment() : env.get());
      if (cancelled) throw new Error("Host check cancelled.");
      const decoder = new TextDecoder();
      const environment = snapshot.entries.filter((entry) =>
        /^(PATH|HOME|LANG|LC_[A-Z_]+|TZ|XDG_CONFIG_HOME|XDG_DATA_HOME|XDG_DATA_DIRS|XDG_RUNTIME_DIR|DBUS_SESSION_BUS_ADDRESS|YAS_MUSTER_DIR)$/.test(
          decoder.decode(entry.key),
        ),
      );
      streams = await process.spawn(
        {
          operationId: operationId(),
          flags: yas.YAS_PROCESS_SPAWN_MERGE_STDERR,
          // ENV_SESSION starts desktop services lazily. A compatibility probe
          // uses the server environment without starting a compositor or bus.
          environmentKind: yas.YAS_PROCESS_ENV_EMPTY,
          cwd: { kind: "server-default" },
          argv: [
            "sh",
            "-c",
            script,
            "yas-extension-check",
            native.connection.hello?.serverName ?? "default",
          ].map(encode),
          environment,
        },
        4096n,
        0n,
      );
      if (cancelled) {
        kill();
        throw new Error("Host check cancelled.");
      }
      streams.stdin?.closeWrite();
      const read = async () => {
        let output = "";
        let length = 0;
        const decoder = new TextDecoder();
        for (;;) {
          const chunk = await streams!.stdout.read();
          if (chunk === null) return output + decoder.decode();
          length += chunk.length;
          if (length > 4096)
            throw new Error("Host check exceeded its output limit.");
          output += decoder.decode(chunk, { stream: true });
        }
      };
      const [output, exit] = await Promise.all([
        read(),
        process.wait(streams.processHandle, 5_000_000_000n),
      ]);
      if (exit.kind !== yas.YAS_PROCESS_EXIT_KIND_CODE)
        throw new Error("Host check did not exit normally.");
      return interpretProbe(id, exit.code, output);
    };
    return await Promise.race([work(), timeout]);
  } catch (error) {
    cancelled = true;
    kill();
    return result(
      "unknown",
      error instanceof Error ? error.message : String(error),
    );
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", abort);
  }
}

export async function checkExtensionViability(
  native: yas.YasNativeProductFamilies,
  entries: readonly RegistryEntry[],
  signal?: AbortSignal,
): Promise<Map<string, Viability>> {
  const hello = native.connection.hello;
  if (!hello)
    return new Map(
      entries.map((entry) => [
        entry.name,
        result("unknown", "Server is not connected."),
      ]),
    );
  const checks = new Map<string, Promise<Viability>>();
  // The environment is identical for every probe in this pass. Share the
  // bounded transfer instead of competing for three copies during startup.
  let environment: Promise<yas.YasEnvSnapshot> | undefined;
  const probeEnvironment = () => (environment ??= native.env!.get());
  return new Map(
    await Promise.all(
      entries.map(async (entry) => {
        const base = checkExtensionCapabilities(hello, entry);
        if (!canRecommend(base)) return [entry.name, base] as const;
        const results = [base];
        for (const id of entry.requirements!.probes) {
          if (signal?.aborted)
            return [
              entry.name,
              result("unknown", "Host check cancelled."),
            ] as const;
          let check = checks.get(id);
          if (!check) {
            check = runExtensionProbe(native, id, signal, probeEnvironment);
            checks.set(id, check);
          }
          results.push(await check);
        }
        const status = results.some((r) => r.status === "unavailable")
          ? "unavailable"
          : results.some((r) => r.status === "unknown")
            ? "unknown"
            : results.some((r) => r.status === "limited")
              ? "limited"
              : "available";
        return [
          entry.name,
          result(status, ...results.flatMap((r) => r.reasons)),
        ] as const;
      }),
    ),
  );
}
