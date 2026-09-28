import { createSignal, For, Show } from "solid-js";
import {
  YAS_EXTENSION_CONTROL_DISABLE,
  YAS_EXTENSION_CONTROL_ENABLE,
  YAS_EXTENSION_CONTROL_RESTART,
  YAS_EXTENSION_CONTROL_START,
  YAS_EXTENSION_CONTROL_STOP,
  YAS_EXTENSION_DEFINITION_ENABLED,
  YAS_EXTENSION_DEFINITION_PERSISTENT,
  YAS_EXTENSION_PHASE_BACKOFF,
  YAS_EXTENSION_PHASE_BLOCKED,
  YAS_EXTENSION_PHASE_NEED_OBJECT,
  YAS_EXTENSION_PHASE_QUEUED,
  YAS_EXTENSION_PHASE_RUNNING,
  YAS_EXTENSION_PHASE_STOPPED,
  YAS_EXTENSION_PHASE_STOPPING,
  YAS_EXTENSION_PHASE_VALIDATING,
  yasExtensionHashHex,
} from "@yas-run/core";
import {
  formatNativeExtensionHandle,
  isOutdated,
  type ExtensionRow,
} from "./extensionRegistry";
import { canRecommend, type Viability } from "./extensionViability";
import { TapButton } from "./TapButton";
import { t, tp } from "./i18n";

export function ExtensionRowView(props: {
  row: ExtensionRow;
  viability?: Viability;
  checking: boolean;
  selected: boolean;
  installDisabled: boolean;
  operation?: { busy: boolean; message: string; error?: boolean };
  onSelect: (checked: boolean) => void;
  onInstall: () => void;
  onRemove: () => void;
  onControl: (action: number, note: string) => void;
}) {
  const [details, setDetails] = createSignal(false);
  const record = () => props.row.installed;
  const pending = () =>
    !!props.row.offered && (!record() || isOutdated(props.row));
  const busy = () => props.operation?.busy === true;
  const enabled = () => !!(record()!.flags & YAS_EXTENSION_DEFINITION_ENABLED);
  const persistent = () =>
    !!(record()!.flags & YAS_EXTENSION_DEFINITION_PERSISTENT);
  const stopped = () =>
    record()!.phase === YAS_EXTENSION_PHASE_STOPPED ||
    record()!.phase === YAS_EXTENSION_PHASE_BLOCKED;
  const phaseName = () =>
    ({
      [YAS_EXTENSION_PHASE_NEED_OBJECT]: "need-object",
      [YAS_EXTENSION_PHASE_VALIDATING]: "validating",
      [YAS_EXTENSION_PHASE_QUEUED]: "queued",
      [YAS_EXTENSION_PHASE_RUNNING]: "running",
      [YAS_EXTENSION_PHASE_BACKOFF]: "backoff",
      [YAS_EXTENSION_PHASE_STOPPED]: "stopped",
      [YAS_EXTENSION_PHASE_BLOCKED]: "blocked",
      [YAS_EXTENSION_PHASE_STOPPING]: "stopping",
    })[record()!.phase] ?? String(record()!.phase);
  const status = () => {
    if (isOutdated(props.row)) return t("extensions.updateAvailable");
    if (record()) return phaseName();
    if (props.checking) return t("extensions.checkingShort");
    return t(`extensions.viability.${props.viability?.status ?? "unknown"}`);
  };
  const tone = () =>
    props.operation?.error
      ? "error"
      : isOutdated(props.row) || props.viability?.status === "limited"
        ? "warning"
        : props.viability?.status === "unavailable"
          ? "muted"
          : record()?.phase === YAS_EXTENSION_PHASE_RUNNING ||
              canRecommend(props.viability)
            ? "success"
            : "muted";
  const short = (hash: string) => hash.slice(0, 12);

  return (
    <article
      class="yas-extension-row"
      data-extension={props.row.label}
      data-tone={tone()}
      data-busy={busy()}
    >
      <div class="yas-extension-main">
        <div class="yas-extension-select">
          <Show when={pending()}>
            <input
              type="checkbox"
              aria-label={tp("extensions.select", { name: props.row.label })}
              checked={props.selected}
              disabled={
                busy() || props.checking || !canRecommend(props.viability)
              }
              onChange={(event) => props.onSelect(event.currentTarget.checked)}
            />
          </Show>
        </div>
        <span class="yas-extension-icon" aria-hidden="true">
          {props.row.label.slice(0, 1).toUpperCase()}
        </span>
        <div class="yas-extension-info">
          <div class="yas-extension-name">
            <strong>{props.row.label}</strong>
            <span class="yas-extension-status">
              <i aria-hidden="true" />
              {status()}
            </span>
          </div>
          <Show when={props.row.description}>
            <p class="yas-extension-description">{props.row.description}</p>
          </Show>
          <Show
            when={
              !props.checking && pending() && props.viability?.reasons.length
            }
          >
            <p class="yas-extension-reason">{props.viability!.reasons[0]}</p>
          </Show>
          <Show when={props.operation?.message}>
            <div
              class="yas-extension-progress"
              role="status"
              aria-live="polite"
              data-error={props.operation?.error || undefined}
            >
              <Show when={busy()}>
                <span class="yas-extension-spinner" aria-hidden="true" />
              </Show>
              {props.operation!.message}
            </div>
          </Show>
        </div>
        <div class="yas-extension-actions" aria-busy={busy()}>
          <Show when={pending()}>
            <TapButton
              type="button"
              class="yas-ext-button primary"
              data-extension-update={isOutdated(props.row) ? "" : undefined}
              disabled={props.installDisabled}
              onClick={props.onInstall}
            >
              {isOutdated(props.row)
                ? t("extensions.update")
                : t("extensions.install")}
            </TapButton>
          </Show>
          <Show when={record() && !pending() && (!enabled() || stopped())}>
            <TapButton
              class="yas-ext-button"
              disabled={busy()}
              onClick={() =>
                props.onControl(
                  enabled()
                    ? YAS_EXTENSION_CONTROL_START
                    : YAS_EXTENSION_CONTROL_ENABLE,
                  enabled() ? "extensions.started" : "extensions.enabledNote",
                )
              }
            >
              {enabled() ? t("extensions.start") : t("extensions.enable")}
            </TapButton>
          </Show>
          <TapButton
            type="button"
            class="yas-ext-button icon quiet"
            title={t("extensions.details")}
            aria-label={tp("extensions.detailsFor", { name: props.row.label })}
            aria-expanded={details()}
            onClick={() => setDetails((value) => !value)}
          >
            <svg
              width="16"
              height="16"
              viewBox="0 0 20 20"
              fill="currentColor"
              aria-hidden="true"
            >
              <circle cx="4" cy="10" r="1.5" />
              <circle cx="10" cy="10" r="1.5" />
              <circle cx="16" cy="10" r="1.5" />
            </svg>
          </TapButton>
        </div>
      </div>
      <div class="yas-extension-details" hidden={!details()}>
        <Show when={props.viability}>
          <div class="yas-extension-diagnostic">
            <strong>
              {t(`extensions.viability.${props.viability!.status}`)}
            </strong>
            <For each={props.viability!.reasons}>
              {(reason) => <p>{reason}</p>}
            </For>
          </div>
        </Show>
        <Show when={props.row.offered?.requirements?.activation}>
          <p>{props.row.offered!.requirements!.activation}</p>
        </Show>
        <dl class="yas-extension-metadata">
          <Show when={record()}>
            <div>
              <dt>{t("extensions.definition")}</dt>
              <dd>
                <code>
                  id:{formatNativeExtensionHandle(record()!.extensionHandle)}
                </code>{" "}
                ·{" "}
                {persistent()
                  ? t("extensions.persistent")
                  : t("extensions.transient")}
                {enabled() ? "" : ` ${t("extensions.disabled")}`}
              </dd>
            </div>
          </Show>
          <Show when={record()}>
            <div>
              <dt>{t("extensions.installedDigest")}</dt>
              <dd>
                <code title={yasExtensionHashHex(record()!.contentHash)}>
                  {short(yasExtensionHashHex(record()!.contentHash))}
                </code>
              </dd>
            </div>
          </Show>
          <Show when={props.row.offered}>
            <div>
              <dt>{t("extensions.registryTitle")}</dt>
              <dd>
                <code title={props.row.offered!.blake3}>
                  {short(props.row.offered!.blake3)}
                </code>
                <Show when={props.row.offered!.brotliBytes}>
                  {" "}
                  · {Math.round(props.row.offered!.brotliBytes / 1024)} KiB
                </Show>
              </dd>
            </div>
          </Show>
        </dl>
        <Show when={record()}>
          <div class="yas-extension-controls">
            <Show when={enabled()}>
              <TapButton
                class="yas-ext-button"
                disabled={busy()}
                onClick={() =>
                  props.onControl(
                    stopped()
                      ? YAS_EXTENSION_CONTROL_START
                      : YAS_EXTENSION_CONTROL_RESTART,
                    stopped() ? "extensions.started" : "extensions.restarted",
                  )
                }
              >
                {stopped() ? t("extensions.start") : t("extensions.restart")}
              </TapButton>
            </Show>
            <Show when={!stopped()}>
              <TapButton
                class="yas-ext-button"
                disabled={busy()}
                onClick={() =>
                  props.onControl(
                    YAS_EXTENSION_CONTROL_STOP,
                    "extensions.stopped",
                  )
                }
              >
                {t("extensions.stop")}
              </TapButton>
            </Show>
            <Show when={persistent()}>
              <TapButton
                class="yas-ext-button"
                disabled={busy()}
                onClick={() =>
                  props.onControl(
                    enabled()
                      ? YAS_EXTENSION_CONTROL_DISABLE
                      : YAS_EXTENSION_CONTROL_ENABLE,
                    enabled()
                      ? "extensions.disabledNote"
                      : "extensions.enabledNote",
                  )
                }
              >
                {enabled() ? t("extensions.disable") : t("extensions.enable")}
              </TapButton>
              <TapButton
                class="yas-ext-button danger"
                disabled={busy()}
                onClick={props.onRemove}
              >
                {t("extensions.remove")}
              </TapButton>
            </Show>
          </div>
        </Show>
      </div>
    </article>
  );
}
