import type {
  YasTransport,
  YasTransportOptions,
  ConnectionStatus,
} from "../types.js";

export interface UnixSocketTransportOptions extends YasTransportOptions {}

/**
 * Shared implementation of the YAS local-IPC byte stream. The socket
 * carries the raw preface followed by `u32` little-endian length-prefixed
 * frames (see `docs/design/yas.md`, "Transport framing"); the transport
 * passes bytes through unchanged and {@link YasConnection} does the framing.
 *
 * Concrete subclasses plug in a socket backend (Node's `net` module,
 * Bun's `Bun.connect`, ...) via {@link openRawSocket}.  They receive
 * bytes through {@link ingestChunk} and must report lifecycle events
 * via {@link onRawConnect}, {@link onRawClose} and {@link onRawError}.
 */
export abstract class AbstractUnixSocketTransport implements YasTransport {
  readonly yasFraming = "stream" as const;
  readonly maxDatagramSize = 0;
  private _status: ConnectionStatus = "disconnected";
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private connectTimer: ReturnType<typeof setTimeout> | null = null;
  private currentDelay: number;
  private disposed = false;
  private messageListeners = new Set<(data: ArrayBuffer) => void>();
  private statusListeners = new Set<(status: ConnectionStatus) => void>();
  /** Local sockets have no passphrase authentication. */
  authRejected = false;
  lastError: string | null = null;

  /** Sentinel that tracks which socket attempt events belong to. */
  protected currentAttempt: symbol | null = null;

  protected readonly path: string;
  private readonly _reconnect: boolean;
  private readonly initialDelay: number;
  private readonly maxDelay: number;
  private readonly backoff: number;
  private readonly connectTimeoutMs: number;

  constructor(path: string, options?: UnixSocketTransportOptions) {
    this.path = path;
    this._reconnect = options?.reconnect ?? true;
    this.initialDelay = options?.reconnectDelay ?? 500;
    this.maxDelay = options?.maxReconnectDelay ?? 10000;
    this.backoff = options?.reconnectBackoff ?? 1.5;
    this.connectTimeoutMs = options?.connectTimeoutMs ?? 10000;
    this.currentDelay = this.initialDelay;
  }

  get status(): ConnectionStatus {
    return this._status;
  }

  send(data: Uint8Array): void {
    if (this._status !== "connected") return;
    this.writeRaw(data);
  }

  close(): void {
    this.disposed = true;
    this.clearReconnectTimer();
    this.clearConnectTimer();
    this.currentAttempt = null;
    this.destroyRawSocket();
    this.setStatus("closed");
  }

  suspend(): void {
    if (this.disposed) return;
    this.clearReconnectTimer();
    this.clearConnectTimer();
    this.currentAttempt = null;
    this.destroyRawSocket();
    this.currentDelay = this.initialDelay;
    this.setStatus("disconnected");
  }

  reconnect(): void {
    if (this.disposed) return;
    this.clearReconnectTimer();
    this.clearConnectTimer();
    this.currentAttempt = null;
    this.destroyRawSocket();
    this.currentDelay = this.initialDelay;
    this.setStatus("disconnected");
    this.connect();
  }

  addEventListener(
    type: "message",
    listener: (data: ArrayBuffer) => void,
  ): void;
  addEventListener(
    type: "datagram",
    listener: (data: ArrayBuffer | Uint8Array) => void,
  ): void;
  addEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
  addEventListener(type: string, listener: (...args: never[]) => void): void {
    if (type === "message") {
      this.messageListeners.add(listener as (data: ArrayBuffer) => void);
    } else if (type === "statuschange") {
      this.statusListeners.add(listener as (status: ConnectionStatus) => void);
    }
  }

  removeEventListener(
    type: "message",
    listener: (data: ArrayBuffer) => void,
  ): void;
  removeEventListener(
    type: "datagram",
    listener: (data: ArrayBuffer | Uint8Array) => void,
  ): void;
  removeEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
  removeEventListener(
    type: string,
    listener: (...args: never[]) => void,
  ): void {
    if (type === "message") {
      this.messageListeners.delete(listener as (data: ArrayBuffer) => void);
    } else if (type === "statuschange") {
      this.statusListeners.delete(
        listener as (status: ConnectionStatus) => void,
      );
    }
  }

  connect(): void {
    if (this.disposed) return;
    if (this.reconnectTimer !== null) {
      this.clearReconnectTimer();
      this.currentDelay = this.initialDelay;
    }
    if (
      this._status === "connecting" ||
      this._status === "authenticating" ||
      this._status === "connected"
    ) {
      return;
    }
    this.setStatus("connecting");

    const attempt = Symbol("unix-attempt");
    this.currentAttempt = attempt;

    this.clearConnectTimer();
    this.connectTimer = setTimeout(() => {
      this.connectTimer = null;
      if (this.currentAttempt !== attempt || this.disposed) return;
      if (this._status === "connecting") {
        this.lastError = "connect timeout";
        this.setStatus("error");
        this.destroyRawSocket();
        this.currentAttempt = null;
        this.scheduleReconnect();
      }
    }, this.connectTimeoutMs);

    this.openRawSocket(attempt);
  }

  // ---- concrete-subclass API ----

  /**
   * Open a backend-specific socket connected to {@link path}.  The
   * subclass must call {@link onRawConnect} on successful connect,
   * {@link ingestChunk} on every chunk, {@link onRawClose} on peer
   * close and {@link onRawError} on error.  The {@link attempt}
   * argument must be echoed back so late events from a superseded
   * socket can be discarded.
   */
  protected abstract openRawSocket(attempt: symbol): void;

  /** Write raw bytes to the currently open socket, if any. */
  protected abstract writeRaw(data: Uint8Array): void;

  /** Close and discard the current socket without firing events. */
  protected abstract destroyRawSocket(): void;

  // ---- helpers for subclasses ----

  protected onRawConnect(attempt: symbol): void {
    if (this.currentAttempt !== attempt || this.disposed) return;
    this.clearConnectTimer();
    this.lastError = null;
    this.currentDelay = this.initialDelay;
    this.setStatus("connected");
  }

  protected ingestChunk(attempt: symbol, chunk: Uint8Array): void {
    if (this.currentAttempt !== attempt || this.disposed) return;
    if (chunk.byteLength === 0) return;
    const ab = new ArrayBuffer(chunk.byteLength);
    new Uint8Array(ab).set(chunk);
    for (const l of this.messageListeners) l(ab);
  }

  protected onRawError(attempt: symbol, message: string): void {
    if (this.currentAttempt !== attempt || this.disposed) return;
    this.lastError = message;
    this.setStatus("error");
  }

  protected onRawClose(attempt: symbol): void {
    if (this.currentAttempt !== attempt || this.disposed) return;
    this.clearConnectTimer();
    this.currentAttempt = null;
    this.setStatus("disconnected");
    this.scheduleReconnect();
  }

  // ---- internals ----

  private setStatus(status: ConnectionStatus): void {
    if (this._status === status) return;
    this._status = status;
    for (const l of this.statusListeners) l(status);
  }

  private clearConnectTimer(): void {
    if (this.connectTimer !== null) {
      clearTimeout(this.connectTimer);
      this.connectTimer = null;
    }
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private scheduleReconnect(): void {
    if (this.disposed || !this._reconnect) return;
    this.clearReconnectTimer();
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.disposed) this.connect();
    }, this.currentDelay);
    this.currentDelay = Math.min(
      this.currentDelay * this.backoff,
      this.maxDelay,
    );
  }
}
