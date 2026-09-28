import { createMemo, For } from "solid-js";
import type {
  YasConnectionSnapshot,
  TerminalPalette,
  YasWorkspace,
} from "@yas-run/core";
import { ExtensionsPanel } from "./ExtensionsPanel";
import { nativeExtensionHost } from "./extensionRegistry";

/** Compact, non-modal offers; expanding one never takes keyboard focus. */
export function ExtensionOffers(props: {
  workspace: YasWorkspace;
  connections: readonly YasConnectionSnapshot[];
  readOnly: (id: string) => boolean;
  label: (id: string) => string;
  palette: TerminalPalette;
  fontSize: number;
}) {
  const connections = createMemo(() =>
    props.connections
      .filter(
        (connection) =>
          connection.status === "connected" &&
          connection.ready &&
          connection.supportsExtensions &&
          !props.readOnly(connection.id) &&
          nativeExtensionHost(props.workspace.getConnection(connection.id)),
      )
      .map((connection) => connection.id),
  );
  return (
    <div
      style={{ "max-height": "60vh", "overflow-y": "auto", "flex-shrink": 0 }}
    >
      <For each={connections()}>
        {(id) => (
          <div
            style={{
              display: "flex",
              "flex-direction": "column",
              "max-height": "56vh",
            }}
          >
            <ExtensionsPanel
              workspace={props.workspace}
              connectionId={id}
              palette={props.palette}
              fontSize={props.fontSize}
              offer={{
                label: props.label(id),
                target: `${location.pathname}:${id}`,
              }}
            />
          </div>
        )}
      </For>
    </div>
  );
}
