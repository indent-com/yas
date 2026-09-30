export { YasWorkspace, consoleLogger, nullLogger } from "./YasWorkspace.js";
export type { YasLogger, YasWorkspaceConnection } from "./YasWorkspace.js";
export { YasNativeWorkspaceConnection } from "./YasNativeWorkspaceConnection.js";
export { noteBrowserClipboardMayHaveChanged } from "./clipboardAuthority.js";
export { YasNativeRelayTransport } from "./yas/nativeRelayTransport.js";
export { YasActivityStore } from "./activity.js";
export type {
  YasActivity,
  YasActivityHandle,
  YasActivityUpdate,
} from "./activity.js";

export {
  YAS_TERMINAL_CATALOG_SEARCH_SOURCE_TITLE as SEARCH_SOURCE_TITLE,
  YAS_TERMINAL_CATALOG_SEARCH_SOURCE_VISIBLE as SEARCH_SOURCE_VISIBLE,
  YAS_TERMINAL_CATALOG_SEARCH_SOURCE_SCROLLBACK as SEARCH_SOURCE_SCROLLBACK,
  YAS_STATUS_RESOURCE_EXHAUSTED,
} from "./yas/generated.js";
export { YasResultError } from "./yas/wire.js";
export type {
  AwaitSessionExitOptions,
  CreateSessionOptions,
  SurfaceTarget,
} from "./workspaceConnectionTypes.js";

export type { YasWasmModule } from "./TerminalStore.js";
export { AudioPlayer } from "./AudioPlayer.js";
export {
  releaseRecordingAudioSession,
  retainRecordingAudioSession,
} from "./audioSession.js";
export {
  NumberRing,
  SurfaceFrameHistory,
  SurfaceStore,
} from "./SurfaceStore.js";
export type {
  SurfaceFrameCallback,
  SurfaceEventCallback,
  SurfaceFrameSample,
  ServerClockSample,
  RemoteSurfaceInput,
  RemoteSurfacePointer,
  SurfaceCursorImage,
  SurfaceCursorRect,
  SurfaceTextInputEvent,
  SurfaceTextInputState,
} from "./SurfaceStore.js";
export {
  estimateSourceToReceiveMs,
  sourceTimestampDelta,
  wrappingTimestampDelta,
} from "./SurfaceStore.js";

export { clampZoom, driveSurfaceResize } from "./surfaceResize.js";
export type { SurfaceResizeTarget, SurfaceZoom } from "./surfaceResize.js";

export { measureCell, cssFontFamily } from "./measure.js";
export type { CellMetrics } from "./measure.js";

export {
  assessUrl,
  escapeUrlForDisplay,
  openUrlSafely,
} from "./urlSecurity.js";
export type { UrlAssessment, UrlVerdict, UrlReason } from "./urlSecurity.js";

export { createShareTransport } from "./transports/webrtc-share.js";

/** YAS v1 session, Transfer, Relay, Font, and browser-edge clients. */
export * from "./yas/index.js";
export * from "./workspaceSessionKv.js";

/** HTTP/1.1 over a relayed stream, for the preview service worker. */
export * from "./http1.js";
/** Preview targets and the /x/ bootstrap prefix. */
export * from "./preview.js";
/** Durable backend workspaces. */
export * from "./workspaceSessions.js";
/** Durable per-device workspace attachment ordering. */
export * from "./workspaceSessionDevices.js";

// Product-model types and presentation helpers retained by the UI.
export {
  MENU_NODE_CHECKMARK,
  MENU_NODE_ENABLED,
  MENU_NODE_RADIO,
  MENU_NODE_SEPARATOR,
  MENU_NODE_SUBMENU,
  MENU_NODE_VISIBLE,
  TRAY_HAS_MENU,
  TRAY_ITEM_IS_MENU,
  TRAY_MENU_OK,
  TRAY_STATUS_NEEDS_ATTENTION,
  TRAY_STATUS_PASSIVE,
} from "./desktopModel.js";
export type {
  DesktopId,
  DesktopImage,
  DesktopNotification,
  DesktopRevision,
  TrayItem,
  TrayMenu,
  TrayMenuNode,
} from "./desktopModel.js";
export {
  ACTIVE_CAMERA,
  ACTIVE_MICROPHONE,
  AUDIO_CODEC_OPUS,
  AUDIO_CODEC_PCM,
  MPRIS_CAN_CONTROL,
  MPRIS_CAN_GO_NEXT,
  MPRIS_CAN_GO_PREVIOUS,
  MPRIS_CAN_PAUSE,
  MPRIS_CAN_PLAY,
  MPRIS_CAN_RAISE,
  MPRIS_CAN_SEEK,
  RUNTIME_CAMERA,
  RUNTIME_MICROPHONE,
  VIDEO_CODEC_AV1,
  VIDEO_CODEC_AV1_444,
  VIDEO_CODEC_H264,
  VIDEO_CODEC_H264_444,
  VIDEO_CODEC_MJPEG,
  cameraCodecLabel,
  cameraCodecProbeOutcomes,
  cameraCodecProbeReport,
  probeCameraCodecs,
  probeOpusMicrophone,
} from "./mediaModel.js";
export type {
  CameraCodecProbeOutcome,
  CameraQuality,
  MediaId,
  MprisAction,
  MprisArtwork,
  MprisPlayer,
  PortalChoiceValue,
  PortalRequest,
  ScreenCastState,
} from "./mediaModel.js";
export {
  FS_ENTRY_DIR,
  FS_ENTRY_FILE,
  FS_ENTRY_LINK_DIR,
  FS_ENTRY_NO_CONTENT,
  FS_ENTRY_SYMLINK,
  FS_ENTRY_TYPE_MASK,
  FS_ENTRY_UNREADABLE,
  FS_ENTRY_UNSTABLE,
} from "./fsModel.js";
export type {
  FsFileIndex,
  FsGrepFile,
  FsGrepOptions,
  FsGrepResult,
} from "./fsModel.js";
export {
  GIT_CLOSED_CLIENT_REQUEST,
  GIT_CLOSED_CONNECTION_LOST,
  GIT_CLOSED_PERMISSION_LOST,
  GIT_CLOSED_REPO_GONE,
  GIT_CLOSED_RESOURCE_LIMIT,
  GIT_COMMITS_MORE,
  GIT_DIFF_UNTRACKED,
  GIT_ENDPOINT_COMMIT,
  GIT_ENDPOINT_EMPTY,
  GIT_ENDPOINT_INDEX,
  GIT_ENDPOINT_WORKTREE,
  GIT_HEAD_DETACHED,
  GIT_HEAD_UNBORN,
  GIT_LOG_FULL_MESSAGE,
  GIT_LOG_TOPO,
  GIT_OID_NONE,
  GIT_OP_BISECT,
  GIT_OP_CHERRY_PICK,
  GIT_OP_MERGE,
  GIT_OP_REBASE,
  GIT_OP_REVERT,
  GIT_REF_PEELED_VALID,
  GIT_REF_SYMBOLIC,
  GIT_STATUS_ENTRY_CONFLICTED,
  GIT_STATUS_NOT_FOUND,
  GIT_STATUS_OK,
  GIT_UPSTREAM_COUNTS_VALID,
  GIT_UPSTREAM_GONE,
  GIT_WORKTREE_BARE,
  GIT_WORKTREE_CURRENT,
  GIT_WORKTREE_DETACHED,
  GIT_WORKTREE_LOCKED,
  GIT_WORKTREE_MAIN,
  GIT_WORKTREE_PRUNABLE,
  gitOidFromHex,
  gitOidHex,
  gitStatusText,
} from "./gitModel.js";
export type { GitOid, GitPatchRecord, GitWorktreeRecord } from "./gitModel.js";
export { GitStateMirror, GitStatusError } from "./gitModel.js";
export {
  LSP_COMPLETION_DEPRECATED,
  LSP_COMPLETION_PRESELECT,
  LSP_COMPLETION_SNIPPET,
  LSP_MARKUP_MARKDOWN,
  LSP_PHASE_FAILED,
  LSP_PHASE_INDEXING,
  LSP_PHASE_INITIALIZING,
  LSP_PHASE_READY,
  LSP_PHASE_SPAWNING,
  LSP_SEVERITY_ERROR,
  LSP_SEVERITY_INFO,
  LSP_SEVERITY_WARNING,
  LSP_STATUS_OK,
  LSP_STATUS_WARMING,
  lspStatusText,
} from "./lspModel.js";
export type {
  YasNativeChannelHandle as ChannelHandle,
  YasNativeChannelNamesWatch as ChannelNamesWatch,
  YasNativeChannelOpenOptions as ChannelOpenOptions,
} from "./yas/nativeChannelFacade.js";
export type { NetOpenOptions, NetStream } from "./netModel.js";
export { formatExtensionId, parseModuleDigest } from "./extensionModel.js";

export {
  DEFAULT_FONT,
  DEFAULT_FONT_SIZE,
  DEFAULT_TEXT_GAMMA,
} from "./types.js";
export {
  CODEC_SUPPORT_H264,
  CODEC_SUPPORT_AV1,
  CODEC_SUPPORT_H264_444,
  CODEC_SUPPORT_AV1_444,
} from "./surfaceModel.js";
export type { YasTransportMessage } from "./types.js";

export {
  EXIT_STATUS_UNKNOWN,
  exitCodeFromStatus,
  formatExitStatus,
} from "./exit-status.js";

export { Notifier } from "./reactive.js";
export type { ReactiveStore } from "./reactive.js";

export type {
  YasConnectionSnapshot,
  YasClientOrigin,
  YasClientInfo,
  YasClientList,
  YasClientAuxSubscription,
  YasClientSurfaceSubscription,
  YasClientTerminalSubscription,
  YasDebug,
  YasSearchResult,
  YasSurface,
  YasSurfaceOrigin,
  YasWorkspaceSnapshot,
  YasTransport,
  YasSession,
  ConnectionId,
  ConnectionStatus,
  CopyRangeResult,
  SessionId,
  SurfaceId,
  TerminalId,
  TerminalPalette,
  TransportConfig,
} from "./types.js";

export {
  SURFACE_POINTER_DOWN,
  SURFACE_POINTER_UP,
  SURFACE_POINTER_MOVE,
  CLIENT_DISCONNECT_REASON_MAX_BYTES,
  clientDisconnectReasonByteLength,
} from "./input.js";

export { PALETTES } from "./palettes.js";

export { MOUSE_DOWN, MOUSE_UP, MOUSE_MOVE } from "./input.js";
export { keyToBytes, ctrlCharToByte, encoder } from "./keyboard.js";

export type { GlRenderer, RendererBackend } from "./gl-renderer.js";
export { createWebGpuRenderer } from "./webgpu-renderer.js";

export {
  YasTerminalSurface,
  isIOS,
  terminalSurfaceForInput,
} from "./YasTerminalSurface.js";
export type {
  YasTerminalSurfaceOptions,
  YasTerminalSurfaceHandle,
  LinkHover,
} from "./YasTerminalSurface.js";

export {
  YAS_SURFACE_TEXT_INPUT_EVENT,
  YasSurfaceCanvas,
  surfaceCanvasForInput,
  detectCodecSupport,
  getCodecSupport,
  getAllowedCodecSupport,
  getProbedCodecSupport,
  setAllowedCodecSupport,
  getMaxDecodeSize,
} from "./YasSurfaceCanvas.js";
export type {
  YasSurfaceTextInputEvent,
  YasSurfaceCanvasOptions,
  SurfaceTouchMode,
} from "./YasSurfaceCanvas.js";

export {
  LAYOUT_MAX_DEPTH,
  LAYOUT_MAX_PANES,
  leafCount,
  validateLayoutNode,
  validateWorkspaceLayout,
  sameLayoutTree,
} from "./layout/model.js";
export type {
  LayoutNode,
  LayoutSplit,
  LayoutChild,
  LayoutLeaf,
} from "./layout/model.js";

export {
  enumeratePanes,
  assignSessionsToPanes,
  buildCandidateOrder,
  reconcileAssignments,
  adjustWeights,
} from "./layout/tree.js";
export type {
  WorkspaceLayout,
  LayoutPane,
  LayoutAssignments,
} from "./layout/tree.js";

export { YasUplinkTransport } from "./transports/uplink.js";
export { YasNoiseTransport } from "./transports/noise.js";
export type { YasNoiseTransportOptions } from "./transports/noise.js";
export {
  generateUplinkKeyPair,
  uplinkPublicKey,
} from "./transports/uplink-crypto.js";
