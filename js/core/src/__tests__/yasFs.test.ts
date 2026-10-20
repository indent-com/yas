import { describe, expect, it } from "vitest";
import {
  YAS_FS_LIMIT_MAX_CATALOG_ENTRIES,
  YAS_FS_MAX_CATALOG_ENTRIES,
  YAS_FS_MAX_PUBLISH_BYTES,
  YAS_FS_PRECONDITION_ABSENT,
  YAS_FS_RECORD_MOVE,
  YAS_GOLDEN_VECTORS,
  YAS_STATE_ADD,
  YAS_STATE_DELTA,
  YAS_STATE_SNAPSHOT_BEGIN,
  YAS_STATE_SNAPSHOT_END,
  YAS_STATE_SNAPSHOT_RECORDS,
  YasFsCatalog,
  YasProtocolError,
  YasWriter,
  decodeFsApply,
  decodeFsApplyResult,
  decodeFsClose,
  decodeFsCommit,
  decodeFsCommitResult,
  decodeFsConflictDetail,
  decodeFsEntry,
  decodeFsFetch,
  decodeFsGrep,
  decodeFsIndex,
  decodeFsMove,
  decodeFsOpen,
  decodeFsPath,
  decodeFsPublish,
  decodeFsPublishResult,
  decodeFsQueryGrepFileRecord,
  decodeFsQueryGrepMatchRecord,
  decodeFsQueryPage,
  decodeFsQueryPathRecord,
  decodeFsQueryReadRecord,
  decodeFsQueryRecordBatch,
  decodeFsRead,
  decodeFsSearch,
  decodeFsStageWrite,
  decodeFsUnwatch,
  decodeFsWatch,
  encodeFsApply,
  encodeFsApplyResult,
  encodeFsClose,
  encodeFsCommit,
  encodeFsCommitResult,
  encodeFsConflictDetail,
  encodeFsEntry,
  encodeFsFetch,
  encodeFsGrep,
  encodeFsIndex,
  encodeFsMove,
  encodeFsOpen,
  encodeFsPath,
  encodeFsPublish,
  encodeFsPublishResult,
  encodeFsPrecondition,
  encodeExtensions,
  encodeFsQueryGrepFileRecord,
  encodeFsQueryGrepMatchRecord,
  encodeFsQueryPage,
  encodeFsQueryPathRecord,
  encodeFsQueryReadRecord,
  encodeFsQueryRecordBatch,
  encodeFsRead,
  encodeFsSearch,
  encodeFsStageWrite,
  encodeFsUnwatch,
  encodeFsWatch,
  type YasFsSnapshot,
  type YasFsPublish,
  type YasFsPrecondition,
  type YasStateBatch,
} from "../yas";

function bytes(name: string): Uint8Array {
  const hex = YAS_GOLDEN_VECTORS.vectors.find(
    (entry) => entry.name === name,
  )!.hex;
  return Uint8Array.from(hex.match(/../g)!, (byte) =>
    Number.parseInt(byte, 16),
  );
}

function catalogConnection(): never {
  return {
    onInvalidation: () => () => undefined,
    family: () => ({
      limits: [
        {
          tag: YAS_FS_LIMIT_MAX_CATALOG_ENTRIES,
          value: new YasWriter().u32(YAS_FS_MAX_CATALOG_ENTRIES).finish(),
        },
      ],
    }),
  } as never;
}

const cases: readonly [string, (payload: Uint8Array) => Uint8Array][] = [
  ["fs.open.payload", (payload) => encodeFsOpen(decodeFsOpen(payload))],
  [
    "fs.close.payload",
    (payload) => {
      const value = decodeFsClose(payload);
      return encodeFsClose(value.rootHandle, value.extensions);
    },
  ],
  [
    "fs.watch.payload",
    (payload) => {
      const value = decodeFsWatch(payload);
      return encodeFsWatch(
        value.rootHandle,
        value.flags,
        value.settleMs,
        value.inlineMax,
        value.ignorePatterns,
        value.encodedStateWatch,
      );
    },
  ],
  [
    "fs.unwatch.payload",
    (payload) => encodeFsUnwatch(decodeFsUnwatch(payload)),
  ],
  ["fs.fetch.payload", (payload) => encodeFsFetch(decodeFsFetch(payload))],
  ["fs.read.payload", (payload) => encodeFsRead(decodeFsRead(payload))],
  ["fs.search.payload", (payload) => encodeFsSearch(decodeFsSearch(payload))],
  ["fs.index.payload", (payload) => encodeFsIndex(decodeFsIndex(payload))],
  ["fs.grep.payload", (payload) => encodeFsGrep(decodeFsGrep(payload))],
  [
    "fs.stage_write.payload",
    (payload) => encodeFsStageWrite(decodeFsStageWrite(payload)),
  ],
  ["fs.publish.payload", (payload) => encodeFsPublish(decodeFsPublish(payload))],
  [
    "fs.publish_result.payload",
    (payload) => encodeFsPublishResult(decodeFsPublishResult(payload)),
  ],
  ["fs.commit.payload", (payload) => encodeFsCommit(decodeFsCommit(payload))],
  [
    "fs.commit_result.payload",
    (payload) => encodeFsCommitResult(decodeFsCommitResult(payload)),
  ],
  [
    "fs.conflict_detail.payload",
    (payload) => encodeFsConflictDetail(decodeFsConflictDetail(payload)),
  ],
  ["fs.apply.payload", (payload) => encodeFsApply(decodeFsApply(payload))],
  [
    "fs.apply.in_place.payload",
    (payload) => encodeFsApply(decodeFsApply(payload)),
  ],
  [
    "fs.apply_result.payload",
    (payload) => encodeFsApplyResult(decodeFsApplyResult(payload)),
  ],
  [
    "fs.entry.inline.payload",
    (payload) => encodeFsEntry(decodeFsEntry(payload)),
  ],
  [
    "fs.query.inline.payload",
    (payload) => encodeFsQueryPage(decodeFsQueryPage(payload)),
  ],
  [
    "fs.query.batch.payload",
    (payload) => encodeFsQueryRecordBatch(decodeFsQueryRecordBatch(payload)),
  ],
  [
    "fs.query.read_record.payload",
    (payload) => encodeFsQueryReadRecord(decodeFsQueryReadRecord(payload)),
  ],
  [
    "fs.query.path_record.payload",
    (payload) => encodeFsQueryPathRecord(decodeFsQueryPathRecord(payload)),
  ],
  [
    "fs.query.grep_file_record.payload",
    (payload) =>
      encodeFsQueryGrepFileRecord(decodeFsQueryGrepFileRecord(payload)),
  ],
  [
    "fs.query.grep_match_record.payload",
    (payload) =>
      encodeFsQueryGrepMatchRecord(decodeFsQueryGrepMatchRecord(payload)),
  ],
  ["fs.state.move.payload", (payload) => encodeFsMove(decodeFsMove(payload))],
];

describe("YAS FS v1", () => {
  it("round-trips every normative payload and rejects every truncation", () => {
    for (const [name, roundTrip] of cases) {
      const payload = bytes(name);
      expect(roundTrip(payload)).toEqual(payload);
      for (let end = 0; end < payload.length; end++)
        expect(() => roundTrip(payload.subarray(0, end))).toThrow(
          YasProtocolError,
        );
    }
  });

  it("preserves raw path bytes and rejects traversal components", () => {
    const path = {
      components: [new Uint8Array([0xff, 0x61]), new Uint8Array([0x62])],
    };
    expect(decodeFsPath(encodeFsPath(path))).toEqual(path);
    for (const component of [
      new Uint8Array(),
      new Uint8Array([0x2e]),
      new Uint8Array([0x2e, 0x2e]),
      new Uint8Array([0x61, 0x2f, 0x62]),
      new Uint8Array([0x61, 0x5c, 0x62]),
      new Uint8Array([0x61, 0, 0x62]),
    ])
      expect(() => encodeFsPath({ components: [component] })).toThrow(
        YasProtocolError,
      );
  });

  it("round-trips PUBLISH wire fields, raw paths, and every precondition", () => {
    const value = publish();
    const payload = publishWire(value);
    expect(encodeFsPublish(value)).toEqual(payload);
    expect(decodeFsPublish(payload)).toEqual({ ...value, extensions: [] });
    for (let end = 0; end < payload.length; end++)
      expect(() => decodeFsPublish(payload.subarray(0, end))).toThrow(
        YasProtocolError,
      );
    expect(() => decodeFsPublish(new Uint8Array([...payload, 0]))).toThrow(
      YasProtocolError,
    );
    const preconditions: YasFsPrecondition[] = [
      { kind: "any" },
      { kind: "absent" },
      { kind: "revision", revision: 7n },
      { kind: "hash", contentHash: new Uint8Array(32).fill(0xa5) },
    ];
    for (const precondition of preconditions) {
      const request = { ...value, precondition };
      expect(decodeFsPublish(encodeFsPublish(request)).precondition).toEqual(
        precondition,
      );
    }
    const siblings = {
      ...value,
      source: { components: [new Uint8Array([0x73])] },
      destination: { components: [new Uint8Array([0x64])] },
    };
    expect(decodeFsPublish(encodeFsPublish(siblings))).toEqual({
      ...siblings,
      extensions: [],
    });
    for (const byteLength of [0n, BigInt(YAS_FS_MAX_PUBLISH_BYTES)]) {
      const request = { ...value, byteLength };
      expect(decodeFsPublish(encodeFsPublish(request)).byteLength).toBe(
        byteLength,
      );
    }
  });

  it("rejects malformed PUBLISH paths, handles, operation IDs, sizes and preconditions", () => {
    const value = publish();
    const root = { components: [] };
    const invalid: YasFsPublish[] = [
      { ...value, rootHandle: 0n },
      { ...value, source: root },
      { ...value, destination: root },
      { ...value, source: value.destination },
      {
        ...value,
        source: {
          components: [new Uint8Array([0x62]), value.source.components[1]!],
        },
      },
      {
        ...value,
        source: {
          components: [new Uint8Array([0x61]), ...value.source.components],
        },
      },
      {
        ...value,
        destination: {
          components: [new Uint8Array([0x61]), ...value.destination.components],
        },
      },
      { ...value, operationId: new Uint8Array(16) },
      { ...value, byteLength: BigInt(YAS_FS_MAX_PUBLISH_BYTES) + 1n },
    ];
    for (const request of invalid) {
      expect(() => encodeFsPublish(request)).toThrow(YasProtocolError);
      expect(() => decodeFsPublish(publishWire(request))).toThrow(
        YasProtocolError,
      );
    }
    for (const field of ["source", "destination"] as const) {
      for (const component of [
        [],
        [0],
        [0x2e],
        [0x2e, 0x2e],
        [0x2f],
        [0x5c],
      ]) {
        const malformedPath = new YasWriter()
          .u16(1)
          .bytesU16(new Uint8Array(component))
          .finish();
        expect(() =>
          decodeFsPublish(publishWire(value, { [field]: malformedPath })),
        ).toThrow(YasProtocolError);
      }
    }
    for (const precondition of [
      new YasWriter().u8(0xff).bytes(new Uint8Array(3)).finish(),
      new YasWriter().u8(YAS_FS_PRECONDITION_ABSENT).u8(1).u16(0).finish(),
    ])
      expect(() =>
        decodeFsPublish(publishWire(value, { precondition })),
      ).toThrow(YasProtocolError);
    for (const request of [
      { ...value, byteLength: -1n },
      { ...value, operationId: new Uint8Array(15).fill(1) },
      { ...value, contentHash: new Uint8Array(31) },
    ])
      expect(() => encodeFsPublish(request)).toThrow(YasProtocolError);
  });

  it("distinguishes published-but-not-durable results and rejects malformed result bodies", () => {
    for (const durable of [false, true]) {
      const result = { durable, extensions: [] };
      const payload = new YasWriter()
        .u8(durable ? 1 : 0)
        .bytes(new Uint8Array(3))
        .bytes(encodeExtensions())
        .finish();
      expect(encodeFsPublishResult(result)).toEqual(payload);
      expect(decodeFsPublishResult(payload)).toEqual(result);
      for (let end = 0; end < payload.length; end++)
        expect(() => decodeFsPublishResult(payload.subarray(0, end))).toThrow(
          YasProtocolError,
        );
      for (const offset of [0, 1, 2, 3]) {
        const malformed = payload.slice();
        malformed[offset] = offset === 0 ? 2 : 1;
        expect(() => decodeFsPublishResult(malformed)).toThrow(YasProtocolError);
      }
      expect(() =>
        decodeFsPublishResult(new Uint8Array([...payload, 0])),
      ).toThrow(YasProtocolError);
    }
  });

  it("preserves optional PUBLISH extensions and rejects unknown required and malformed tags", () => {
    const extension = {
      tag: 0x7fff,
      required: false,
      value: new Uint8Array([0xff]),
    };
    const value = { ...publish(), extensions: [extension] };
    expect(decodeFsPublish(encodeFsPublish(value)).extensions).toEqual([extension]);
    expect(
      decodeFsPublishResult(
        encodeFsPublishResult({ durable: false, extensions: [extension] }),
      ).extensions,
    ).toEqual([extension]);
    const required = [{ ...extension, required: true }];
    expect(() =>
      decodeFsPublish(encodeFsPublish({ ...value, extensions: required })),
    ).toThrow(YasProtocolError);
    expect(() =>
      decodeFsPublishResult(
        encodeFsPublishResult({ durable: true, extensions: required }),
      ),
    ).toThrow(YasProtocolError);
    const tag = (flags: number) =>
      new YasWriter()
        .u16(extension.tag)
        .u16(flags)
        .bytesU32(extension.value)
        .finish();
    for (const body of [tag(2), new Uint8Array([...tag(0), ...tag(0)])]) {
      const extensions = new YasWriter().bytesU32(body).finish();
      expect(() =>
        decodeFsPublish(publishWire(publish(), { extensions })),
      ).toThrow(YasProtocolError);
      expect(() =>
        decodeFsPublishResult(
          new YasWriter()
            .u8(1)
            .bytes(new Uint8Array(3))
            .bytes(extensions)
            .finish(),
        ),
      ).toThrow(YasProtocolError);
    }
  });

  it("applies snapshot and first-class MOVE records without losing metadata", () => {
    const connection = catalogConnection();
    const catalog = new YasFsCatalog(connection, 1n);
    const apply = (
      catalog as unknown as { apply(batch: YasStateBatch): void }
    ).apply.bind(catalog);
    let latest: YasFsSnapshot | undefined;
    catalog.subscribe((snapshot) => {
      latest = snapshot;
    });
    const entry = decodeFsEntry(bytes("fs.entry.inline.payload"));
    apply(batch(YAS_STATE_SNAPSHOT_BEGIN, 0n, 1n));
    apply(
      batch(YAS_STATE_SNAPSHOT_RECORDS, 1n, 1n, [
        { kind: YAS_STATE_ADD, flags: 0, body: encodeFsEntry(entry) },
      ]),
    );
    apply(batch(YAS_STATE_SNAPSHOT_END, 1n, 1n));
    expect(latest?.entries).toHaveLength(1);

    const move = decodeFsMove(bytes("fs.state.move.payload"));
    apply(
      batch(YAS_STATE_DELTA, 1n, 2n, [
        { kind: YAS_FS_RECORD_MOVE, flags: 0, body: encodeFsMove(move) },
      ]),
    );
    expect(latest?.revision).toBe(2n);
    expect(latest?.entries[0]).toMatchObject({
      entryRevision: entry.entryRevision,
      mode: entry.mode,
      path: move.to,
    });
  });

  it("exposes validated state batches without reconstructing phase boundaries", () => {
    const connection = catalogConnection();
    const catalog = new YasFsCatalog(connection, 1n);
    const apply = (
      catalog as unknown as { apply(batch: YasStateBatch): void }
    ).apply.bind(catalog);
    const phases: number[] = [];
    const remove = catalog.subscribeBatches((state) => {
      phases.push(state.phase);
    });

    apply(batch(YAS_STATE_SNAPSHOT_BEGIN, 0n, 1n));
    apply(batch(YAS_STATE_SNAPSHOT_RECORDS, 1n, 1n));
    apply(batch(YAS_STATE_SNAPSHOT_END, 1n, 1n));
    remove();
    apply(batch(YAS_STATE_DELTA, 1n, 2n));

    expect(phases).toEqual([
      YAS_STATE_SNAPSHOT_BEGIN,
      YAS_STATE_SNAPSHOT_RECORDS,
      YAS_STATE_SNAPSHOT_END,
    ]);
  });
});

function batch(
  phase: number,
  fromRevision: bigint,
  toRevision: bigint,
  records: YasStateBatch["records"] = [],
): YasStateBatch {
  return { phase, flags: 0, fromRevision, toRevision, records };
}

function publish(): YasFsPublish {
  return {
    rootHandle: 9n,
    source: { components: [new Uint8Array([0xff, 0x61]), new Uint8Array([0x73])] },
    destination: {
      components: [new Uint8Array([0xff, 0x61]), new Uint8Array([0x64])],
    },
    precondition: { kind: "absent" },
    operationId: new Uint8Array(16).fill(1),
    byteLength: 3n,
    contentHash: new Uint8Array(32).fill(0xa5),
  };
}

// Independent layout vector also permits deliberately malformed nested payloads.
function publishWire(
  value: YasFsPublish,
  raw: {
    source?: Uint8Array;
    destination?: Uint8Array;
    precondition?: Uint8Array;
    extensions?: Uint8Array;
  } = {},
): Uint8Array {
  return new YasWriter()
    .u64(value.rootHandle)
    .bytesU32(raw.source ?? encodeFsPath(value.source))
    .bytesU32(raw.destination ?? encodeFsPath(value.destination))
    .bytesU32(raw.precondition ?? encodeFsPrecondition(value.precondition))
    .bytes(value.operationId)
    .u64(value.byteLength)
    .bytes(value.contentHash)
    .bytes(raw.extensions ?? encodeExtensions(value.extensions))
    .finish();
}
