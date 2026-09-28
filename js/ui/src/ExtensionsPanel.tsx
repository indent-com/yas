/** Concurrent extension management and connection-time install/update offers. */
import {
  createMemo,
  createRenderEffect,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import type {
  YasExtensionRecord,
  YasWorkspace,
  ConnectionId,
  TerminalPalette,
  YasNativeProductFamilies,
} from "@yas-run/core";
import {
  YAS_EXTENSION_CONTROL_DISABLE,
  YAS_EXTENSION_CONTROL_ENABLE,
  YAS_EXTENSION_CONTROL_RESTART,
  YAS_EXTENSION_CONTROL_START,
  YAS_EXTENSION_CONTROL_STOP,
  YAS_EXTENSION_DEFINITION_PERSISTENT,
} from "@yas-run/core";
import {
  defaultRegistry,
  disableAndRemoveExtension,
  fetchRegistry,
  installFromRegistry,
  isOutdated,
  mergeExtensionInventory,
  mergeExtensions,
  nativeExtensionHost,
  upsertExtensionRecord,
  type ExtensionRow,
  type Registry,
} from "./extensionRegistry";
import {
  canRecommend,
  checkExtensionViability,
  extensionOfferFingerprint,
  type Viability,
} from "./extensionViability";
import { extensionOperations } from "./extensionOperations";
import { ExtensionRowView } from "./ExtensionRowView";
import { TapButton } from "./TapButton";
import { themeFor, uiScale } from "./theme";
import { t, tp } from "./i18n";
import "./extensions.css";

export function ExtensionsPanel(props: {
  workspace: YasWorkspace;
  connectionId: ConnectionId;
  palette: TerminalPalette;
  fontSize: number;
  offer?: { label: string; target: string };
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);
  // Unknown inventory must not make installed extensions look installable.
  const [installed, setInstalled] = createSignal<
    readonly YasExtensionRecord[] | null
  >(null);
  const [registry, setRegistry] = createSignal<Registry | null>(null);
  const [registryUrl, setRegistryUrl] = createSignal(defaultRegistry());
  const [inventoryError, setInventoryError] = createSignal<string | null>(null);
  const [registryError, setRegistryError] = createSignal<string | null>(null);
  const [inventoryLoading, setInventoryLoading] = createSignal(false);
  const [registryLoading, setRegistryLoading] = createSignal(false);
  const [checking, setChecking] = createSignal(false);
  const [viability, setViability] = createSignal(new Map<string, Viability>());
  // Default every eligible install/update to selected. Explicit choices survive
  // rechecks, and newly eligible rows don't inherit a stale selection snapshot.
  const [choices, setChoices] = createSignal(new Map<string, boolean>());
  const [offeredNames, setOfferedNames] = createSignal(new Set<string>());
  const [offerExpanded, setOfferExpanded] = createSignal(false);
  const [registryOpen, setRegistryOpen] = createSignal(false);
  const [dismissed, setDismissed] = createSignal(false);
  const host = () =>
    nativeExtensionHost(props.workspace.getConnection(props.connectionId));
  const [operations, setOperations] = extensionOperations(host());
  let offerFingerprint = "";
  let dismissalKey = "";
  let checkRequest = 0;
  let checkAbort: AbortController | undefined;
  let inventoryRequest = 0;
  let registryRequest = 0;
  let disposed = false;
  const native = (): YasNativeProductFamilies | null => {
    const value = host()?.native as YasNativeProductFamilies | undefined;
    return value?.connection?.onReady ? value : null;
  };
  const errorText = (failure: unknown) =>
    failure instanceof Error ? failure.message : String(failure);

  const checkRegistry = async () => {
    const source = registry();
    if (!source) return;
    const request = ++checkRequest;
    checkAbort?.abort();
    const abort = new AbortController();
    checkAbort = abort;
    setChecking(true);
    try {
      const connection = native();
      const inventory = installed() ?? (await host()?.listExtensions());
      const results = connection
        ? await checkExtensionViability(
            connection,
            source.extensions,
            abort.signal,
          )
        : new Map(
            source.extensions.map((entry) => [
              entry.name,
              {
                status: "unknown" as const,
                reasons: [t("extensions.checkUnavailable")],
              },
            ]),
          );
      if (request !== checkRequest || disposed) return;
      setViability(results);
      if (props.offer && connection?.connection.hello) {
        const hello = connection.connection.hello;
        offerFingerprint = extensionOfferFingerprint(
          hello,
          source.extensions,
          source.url,
        );
        dismissalKey = `yas.extensionOffer.${JSON.stringify([props.offer.target, hello.serverName])}`;
        try {
          setDismissed(localStorage.getItem(dismissalKey) === offerFingerprint);
        } catch {
          /* Storage may be disabled. */
        }
        // Keep all candidates visible in Review, with reasons for anything not
        // selected. A failed probe must not silently hide three of four rows.
        setOfferedNames(
          new Set(
            mergeExtensions(installed() ?? inventory ?? [], source.extensions)
              .filter(
                (row) => row.offered && (!row.installed || isOutdated(row)),
              )
              .map((row) => row.label),
          ),
        );
      }
    } catch (failure) {
      if (request !== checkRequest || disposed) return;
      setViability(
        new Map(
          source.extensions.map((entry) => [
            entry.name,
            { status: "unknown" as const, reasons: [errorText(failure)] },
          ]),
        ),
      );
    } finally {
      if (request === checkRequest) setChecking(false);
    }
  };

  const refresh = async () => {
    const connection = host();
    const request = ++inventoryRequest;
    setInventoryLoading(true);
    if (!connection) {
      setInstalled(null);
      setInventoryError(t("extensions.connectionUnavailable"));
      setInventoryLoading(false);
      return;
    }
    try {
      const records = await connection.listExtensions();
      if (request !== inventoryRequest) return;
      setInstalled((previous) => mergeExtensionInventory(previous, records));
      setInventoryError(null);
    } catch (failure) {
      if (request !== inventoryRequest) return;
      setInstalled(null);
      setInventoryError(errorText(failure));
    } finally {
      if (request === inventoryRequest) setInventoryLoading(false);
    }
  };
  const loadRegistry = async () => {
    const request = ++registryRequest;
    setRegistryLoading(true);
    try {
      const loaded = await fetchRegistry(registryUrl());
      if (request !== registryRequest) return;
      setRegistry(loaded);
      setRegistryError(null);
      void checkRegistry();
    } catch (failure) {
      if (request !== registryRequest) return;
      setRegistry(null);
      setRegistryError(errorText(failure));
    } finally {
      if (request === registryRequest) setRegistryLoading(false);
    }
  };
  onMount(() => {
    const unsubscribe = host()?.subscribeExtensions((records) => {
      if (records === null) {
        setInstalled(null);
        return;
      }
      // Live state supersedes list snapshots awaiting delivery.
      inventoryRequest++;
      setInventoryLoading(false);
      setInstalled((previous) => mergeExtensionInventory(previous, records));
      setInventoryError(null);
    });
    const connection = native()?.connection;
    const removeReady = connection?.onReady(() => {
      void checkRegistry();
    });
    const removeCatalog = connection?.onCatalogChange(() => {
      void checkRegistry();
    });
    onCleanup(() => {
      unsubscribe?.();
      removeReady?.();
      removeCatalog?.();
    });
    void refresh();
    void loadRegistry();
  });
  onCleanup(() => {
    disposed = true;
    inventoryRequest++;
    registryRequest++;
    checkRequest++;
    checkAbort?.abort();
  });

  const rows = createMemo<ExtensionRow[]>((previous) => {
    const inventory = installed();
    if (inventory === null) return [];
    const prior = new Map(previous.map((row) => [row.key, row]));
    return mergeExtensions(inventory, registry()?.extensions ?? [])
      .filter(
        (row) =>
          !props.offer || (row.offered && offeredNames().has(row.offered.name)),
      )
      .sort(
        (a, b) => a.label.localeCompare(b.label) || a.key.localeCompare(b.key),
      )
      .map((row) => {
        const known = prior.get(row.key);
        // Preserve unrelated buttons even during a pointer-down/click pair.
        return known &&
          known.installed === row.installed &&
          known.offered === row.offered
          ? known
          : row;
      });
  }, []);
  const operationKey = (row: ExtensionRow) =>
    row.offered ||
    (row.installed?.name &&
      row.installed.flags & YAS_EXTENSION_DEFINITION_PERSISTENT)
      ? `name:${row.label}`
      : row.key;
  const operation = (row: ExtensionRow) => operations().get(operationKey(row));
  const pendingRows = () =>
    rows().filter((row) => row.offered && (!row.installed || isOutdated(row)));
  const eligibleRows = () =>
    pendingRows().filter((row) => canRecommend(viability().get(row.label)));
  const selectedRows = () =>
    eligibleRows().filter((row) => choices().get(row.label) !== false);
  const select = (name: string, checked: boolean) =>
    setChoices((previous) => new Map(previous).set(name, checked));
  const installsBusy = (row: ExtensionRow) =>
    operation(row)?.busy === true ||
    registryLoading() ||
    checking() ||
    viability().get(row.label)?.status === "unavailable";
  const summary = () =>
    [
      eligibleRows().filter((row) => !row.installed).length
        ? tp("extensions.availableCount", {
            count: eligibleRows().filter((row) => !row.installed).length,
          })
        : "",
      pendingRows().filter(isOutdated).length === 1
        ? t("extensions.oneUpdate")
        : pendingRows().filter(isOutdated).length
          ? tp("extensions.updateCount", {
              count: pendingRows().filter(isOutdated).length,
            })
          : "",
      rows().filter((row) => row.installed && !isOutdated(row)).length
        ? tp("extensions.installedCount", {
            count: rows().filter((row) => row.installed && !isOutdated(row))
              .length,
          })
        : "",
      pendingRows().filter((row) => !canRecommend(viability().get(row.label)))
        .length && !checking()
        ? tp("extensions.attentionCount", {
            count: pendingRows().filter(
              (row) => !canRecommend(viability().get(row.label)),
            ).length,
          })
        : "",
    ]
      .filter(Boolean)
      .join(" · ");
  const batchLabel = () =>
    selectedRows().every((row) => !row.installed)
      ? t("extensions.installSelected")
      : selectedRows().every(isOutdated)
        ? t("extensions.updateSelected")
        : t("extensions.applySelected");
  const dismissOffer = () => {
    setDismissed(true);
    try {
      localStorage.setItem(dismissalKey, offerFingerprint);
    } catch {
      /* Keep session dismissal. */
    }
  };
  const mutateInventory = (
    update: (
      records: readonly YasExtensionRecord[] | null,
    ) => readonly YasExtensionRecord[] | null,
  ) => {
    if (disposed) return;
    inventoryRequest++;
    setInventoryLoading(false);
    setInstalled(update);
  };
  const act = async (
    row: ExtensionRow,
    stage: string,
    noteKey: string,
    action: (progress: (stage: string) => void) => Promise<unknown>,
  ) => {
    const key = operationKey(row);
    if (disposed || operations().get(key)?.busy) return;
    const update = (busy: boolean, message: string, error = false) =>
      setOperations((previous) =>
        new Map(previous).set(key, { busy, message, error }),
      );
    const progress = (stage: string) =>
      update(true, t(`extensions.progress.${stage}`));
    progress(stage);
    try {
      await action(progress);
      update(false, tp(noteKey, { name: row.label }));
    } catch (failure) {
      update(false, errorText(failure), true);
      if (!disposed) void refresh();
    }
  };
  const install = (row: ExtensionRow) => {
    const connection = host();
    const source = registry();
    if (!connection || !source || !row.offered || installsBusy(row)) return;
    void act(
      row,
      "checking",
      isOutdated(row) ? "extensions.updated" : "extensions.installed",
      async (progress) => {
        const updated = await installFromRegistry(
          connection,
          source,
          row.offered!,
          fetch,
          progress,
        );
        mutateInventory((records) => upsertExtensionRecord(records, updated));
      },
    );
  };
  const remove = (row: ExtensionRow) => {
    const record = row.installed!;
    const connection = host();
    if (!connection) return;
    void act(row, "disabling", "extensions.removed", async (progress) => {
      await disableAndRemoveExtension(connection, record, undefined, progress);
      mutateInventory(
        (records) =>
          records?.filter(
            (item) => item.extensionHandle !== record.extensionHandle,
          ) ?? null,
      );
    });
  };
  const control = (row: ExtensionRow, action: number, noteKey: string) => {
    const connection = host();
    if (!connection) return;
    const stage = {
      [YAS_EXTENSION_CONTROL_START]: "starting",
      [YAS_EXTENSION_CONTROL_STOP]: "stopping",
      [YAS_EXTENSION_CONTROL_RESTART]: "restarting",
      [YAS_EXTENSION_CONTROL_ENABLE]: "enabling",
      [YAS_EXTENSION_CONTROL_DISABLE]: "disabling",
    }[action]!;
    void act(row, stage, noteKey, async () => {
      const updated = await connection.controlExtension(
        row.installed!.extensionHandle,
        action,
      );
      if (updated)
        mutateInventory((records) => upsertExtensionRecord(records, updated));
    });
  };
  const offerVisible = () =>
    !dismissed() &&
    rows().length > 0 &&
    rows().some(
      (row) => canRecommend(viability().get(row.label)) || operation(row),
    );

  return (
    <Show when={!props.offer || offerVisible()}>
      <section
        class="yas-extensions"
        data-offer={props.offer ? "" : undefined}
        aria-label={t("extensions.title")}
        style={{
          "--ext-bg": theme().solidPanelBg,
          "--ext-fg": theme().fg,
          "--ext-muted": `color-mix(in srgb, ${theme().fg} 68%, ${theme().solidPanelBg})`,
          "--ext-border": `color-mix(in srgb, ${theme().border} 65%, ${theme().solidPanelBg})`,
          "--ext-hover": theme().hoverBg,
          "--ext-accent": `color-mix(in srgb, ${theme().accent} 75%, ${theme().fg})`,
          "--ext-error": theme().error,
          "--ext-success": theme().success,
          "--ext-warning": theme().warning,
          "--ext-font": `${scale().md}px`,
          "--ext-input": theme().inputBg,
        }}
      >
        <header class="yas-extensions-heading">
          <div class="yas-extensions-title">
            <strong>
              {props.offer
                ? tp("extensions.offer", { name: props.offer.label })
                : t("extensions.title")}
            </strong>
            <span>{summary() || t("extensions.loading")}</span>
          </div>
          <div class="yas-extensions-tools">
            <Show
              when={props.offer}
              fallback={
                <>
                  <TapButton
                    class="yas-ext-button quiet"
                    aria-expanded={registryOpen()}
                    onClick={() => setRegistryOpen((value) => !value)}
                  >
                    {t("extensions.registryTitle")}
                  </TapButton>
                  <TapButton
                    class="yas-ext-button quiet"
                    disabled={
                      inventoryLoading() || registryLoading() || checking()
                    }
                    onClick={() => {
                      void refresh();
                      void loadRegistry();
                    }}
                  >
                    {t("extensions.reload")}
                  </TapButton>
                </>
              }
            >
              <TapButton
                class="yas-ext-button"
                aria-expanded={offerExpanded()}
                onClick={() => setOfferExpanded((value) => !value)}
              >
                {offerExpanded()
                  ? t("extensions.hide")
                  : t("extensions.review")}
              </TapButton>
              <TapButton class="yas-ext-button quiet" onClick={dismissOffer}>
                {t("extensions.dismiss")}
              </TapButton>
            </Show>
          </div>
        </header>
        <Show when={!props.offer || offerExpanded()}>
          <For
            each={[inventoryError(), registryError()].filter(
              (error): error is string => error !== null,
            )}
          >
            {(error) => (
              <div role="alert" class="yas-extensions-error">
                {error}
              </div>
            )}
          </For>
          <Show when={registryOpen() && !props.offer}>
            <form
              class="yas-extensions-registry"
              onSubmit={(event) => {
                event.preventDefault();
                void loadRegistry();
              }}
            >
              <input
                data-registry-url
                aria-label={t("extensions.registryTitle")}
                value={registryUrl()}
                onInput={(event) => setRegistryUrl(event.currentTarget.value)}
              />
              <TapButton
                type="submit"
                class="yas-ext-button"
                disabled={registryLoading()}
              >
                {t("extensions.useRegistry")}
              </TapButton>
            </form>
          </Show>
          <Show when={pendingRows().length > 0 || checking()}>
            <div class="yas-extensions-selection">
              <label>
                <input
                  type="checkbox"
                  aria-label={t("extensions.selectAll")}
                  checked={
                    eligibleRows().length > 0 &&
                    selectedRows().length === eligibleRows().length
                  }
                  ref={(input) =>
                    createRenderEffect(() => {
                      input.indeterminate =
                        selectedRows().length > 0 &&
                        selectedRows().length < eligibleRows().length;
                    })
                  }
                  disabled={checking() || eligibleRows().length === 0}
                  onChange={(event) => {
                    const checked = event.currentTarget.checked;
                    setChoices((previous) => {
                      const next = new Map(previous);
                      for (const row of eligibleRows())
                        next.set(row.label, checked);
                      return next;
                    });
                  }}
                />
                <span>
                  {checking()
                    ? t("extensions.checking")
                    : tp("extensions.selectedCount", {
                        count: selectedRows().length,
                        total: pendingRows().length,
                      })}
                </span>
              </label>
              <div class="yas-extensions-tools">
                <TapButton
                  class="yas-ext-button quiet"
                  disabled={checking()}
                  onClick={() => {
                    void refresh();
                    void loadRegistry();
                  }}
                >
                  {t("extensions.recheck")}
                </TapButton>
                <TapButton
                  class="yas-ext-button primary"
                  disabled={!selectedRows().some((row) => !installsBusy(row))}
                  onClick={() => {
                    for (const row of selectedRows()) install(row);
                  }}
                >
                  {batchLabel()}
                </TapButton>
              </div>
            </div>
          </Show>
          <div class="yas-extensions-list">
            <For
              each={rows()}
              fallback={
                <div class="yas-extensions-empty">
                  {inventoryLoading() || registryLoading()
                    ? t("extensions.loading")
                    : inventoryError() || registryError()
                      ? ""
                      : t("extensions.none")}
                </div>
              }
            >
              {(row) => (
                <ExtensionRowView
                  row={row}
                  viability={viability().get(row.label)}
                  checking={checking()}
                  operation={operation(row)}
                  selected={
                    canRecommend(viability().get(row.label)) &&
                    choices().get(row.label) !== false
                  }
                  installDisabled={installsBusy(row)}
                  onSelect={(checked) => select(row.label, checked)}
                  onInstall={() => install(row)}
                  onRemove={() => remove(row)}
                  onControl={(action, note) => control(row, action, note)}
                />
              )}
            </For>
          </div>
        </Show>
      </section>
    </Show>
  );
}
