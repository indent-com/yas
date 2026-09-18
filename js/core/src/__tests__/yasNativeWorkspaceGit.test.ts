import { describe, expect, it, vi } from "vitest";

import {
  isTransientGitWatchStartError,
  encodeFsPath,
  YasNativeWorkspaceGit,
  YasResultError,
  YAS_GIT_ENTITY_STATUS,
  YAS_GIT_WATCH_STATUS_UNTRACKED,
  YAS_GIT_WORKTREE_STATUS_IGNORED,
  YAS_STATUS_INVALID,
  YAS_STATUS_RESOURCE_EXHAUSTED,
  YAS_STATUS_UNAVAILABLE,
  type YasConnection,
  type YasGitEntityRecord,
  type YasGitRepository,
} from "../yas";

function connection(): YasConnection {
  return {
    onInvalidation: vi.fn(() => () => undefined),
  } as unknown as YasConnection;
}

function nativeRepository(): YasGitRepository {
  return {
    handle: 0xf000_0000_0000_0001n,
    opened: {
      repositoryHandle: 0xf000_0000_0000_0001n,
      repositoryRevision: 1n,
      objectAlgorithm: 1,
      repositoryFlags: 0,
      canonicalWorktreePath: new TextEncoder().encode("/repo"),
      canonicalGitDir: new TextEncoder().encode("/repo/.git"),
      extensions: [],
    },
    onClosed: vi.fn(() => () => undefined),
    close: vi.fn(async () => undefined),
  } as unknown as YasGitRepository;
}

function ignoredStatusEntity(path: string): YasGitEntityRecord {
  return {
    entityKind: YAS_GIT_ENTITY_STATUS,
    key: encodeFsPath({
      components: [new TextEncoder().encode(path)],
    }),
    revision: 1n,
    body: {
      kind: "status",
      indexStatus: YAS_GIT_WORKTREE_STATUS_IGNORED,
      worktreeStatus: YAS_GIT_WORKTREE_STATUS_IGNORED,
      flags: 0,
    },
    extensions: [],
  };
}

describe("YasNativeWorkspaceGit lifecycle", () => {
  it("retries only transient watched-query admission failures", () => {
    const result = (status: number) =>
      new YasResultError(status, new Uint8Array());

    expect(
      isTransientGitWatchStartError(result(YAS_STATUS_RESOURCE_EXHAUSTED)),
    ).toBe(true);
    expect(isTransientGitWatchStartError(result(YAS_STATUS_UNAVAILABLE))).toBe(
      true,
    );
    expect(isTransientGitWatchStartError(result(YAS_STATUS_INVALID))).toBe(
      false,
    );
  });

  it("selects status on the server and filters unrequested classes client-side", async () => {
    const list = vi.fn(async () => ({
      revision: 1n,
      entities: [ignoredStatusEntity("ignored")],
    }));
    const native = Object.assign(nativeRepository(), {
      list,
      catalog: {
        subscribe: vi.fn(() => () => undefined),
      },
    });
    const workspace = new YasNativeWorkspaceGit(connection(), {
      terminalHandle: () => undefined,
      client: {
        open: vi.fn(async () => native),
        discover: vi.fn(),
      },
    });

    const handle = await workspace.openRepo("/repo", {
      status: true,
      untracked: true,
    });

    expect(list).toHaveBeenCalledWith(
      expect.objectContaining({
        statusSelection: YAS_GIT_WATCH_STATUS_UNTRACKED,
      }),
    );
    // An old server that ignored the selection extension can still send
    // ignored records; the client drops them rather than surfacing them.
    expect(handle.state.status.map(({ path }) => path)).toEqual([]);
  });

  it("includes ignored status when requested", async () => {
    const list = vi.fn(async () => ({
      revision: 1n,
      entities: [ignoredStatusEntity("ignored")],
    }));
    const native = Object.assign(nativeRepository(), {
      list,
      catalog: {
        subscribe: vi.fn(() => () => undefined),
      },
    });
    const workspace = new YasNativeWorkspaceGit(connection(), {
      terminalHandle: () => undefined,
      client: {
        open: vi.fn(async () => native),
        discover: vi.fn(),
      },
    });

    const handle = await workspace.openRepo("/repo", {
      status: true,
      untracked: true,
      ignored: true,
    });

    expect(handle.state.status.map(({ path }) => path)).toEqual(["ignored"]);
  });

  it("closes active repositories on facade disposal", async () => {
    vi.stubGlobal("reportError", vi.fn());
    const native = nativeRepository();
    const workspace = new YasNativeWorkspaceGit(connection(), {
      terminalHandle: () => undefined,
      client: {
        open: vi.fn(async () => native),
        discover: vi.fn(),
      },
    });
    await workspace.openRepo("/repo", {
      onClosed: () => {
        throw new Error("close callback failed");
      },
    });

    workspace.dispose();
    await Promise.resolve();

    expect(native.close).toHaveBeenCalledOnce();
    vi.unstubAllGlobals();
  });

  it("self-closes a repository returned after permanent disposal", async () => {
    let resolveOpen!: (repository: YasGitRepository) => void;
    const pendingOpen = new Promise<YasGitRepository>((resolve) => {
      resolveOpen = resolve;
    });
    const native = nativeRepository();
    const workspace = new YasNativeWorkspaceGit(connection(), {
      terminalHandle: () => undefined,
      client: {
        open: vi.fn(() => pendingOpen),
        discover: vi.fn(),
      },
    });

    const pending = workspace.openRepo("/repo");
    workspace.dispose();
    resolveOpen(native);

    await expect(pending).rejects.toThrow(/OPEN was pending/);
    expect(native.close).toHaveBeenCalledOnce();
  });

  it("does not notify state subscribers again after onState disposes reentrantly", async () => {
    let deliver!: (snapshot: {
      revision: bigint;
      entities: readonly never[];
    }) => void;
    const native = nativeRepository();
    Object.assign(native, {
      list: vi.fn(async () => ({ revision: 1n, entities: [] })),
      catalog: {
        subscribe: vi.fn((listener) => {
          deliver = listener;
          return () => undefined;
        }),
      },
    });
    const workspace = new YasNativeWorkspaceGit(connection(), {
      terminalHandle: () => undefined,
      client: {
        open: vi.fn(async () => native),
        discover: vi.fn(),
      },
    });
    let handle: Awaited<ReturnType<typeof workspace.openRepo>> | undefined;
    handle = await workspace.openRepo("/repo", {
      watch: true,
      onState: () => {
        if (handle) workspace.dispose();
      },
    });
    const subscriber = vi.fn();
    handle.subscribe(subscriber);

    deliver({ revision: 2n, entities: [] });

    // One notification belongs to close(); applySnapshot must not emit again.
    expect(subscriber).toHaveBeenCalledOnce();
    expect(native.close).toHaveBeenCalledOnce();
  });
});
