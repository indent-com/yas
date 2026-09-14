/**
 * AV1 `seq_level_idx` for a frame of this size at 60 fps, as the two-digit
 * string used by WebCodecs codec parameters. Mirrors the server's Table A.3
 * lookup so advertised configs and emitted bitstreams agree.
 */
export function av1LevelString(width: number, height: number): string {
  const pic = width * height;
  const rate = pic * 60;
  const specs: [number, number, number, number, number][] = [
    [0, 147456, 2048, 1152, 4423680],
    [1, 278784, 2816, 1584, 8363520],
    [4, 665856, 4352, 2448, 19975680],
    [5, 1065024, 5504, 3096, 31950720],
    [8, 2359296, 6144, 3456, 70778880],
    [9, 2359296, 6144, 3456, 141557760],
    [12, 8912896, 8192, 4352, 267386880],
    [13, 8912896, 8192, 4352, 534773760],
    [14, 8912896, 8192, 4352, 1069547520],
    [16, 35651584, 16384, 8704, 1069547520],
    [17, 35651584, 16384, 8704, 2139095040],
    [18, 35651584, 16384, 8704, 4278190080],
  ];
  for (const [idx, maxPic, maxW, maxH, maxRate] of specs) {
    if (pic <= maxPic && width <= maxW && height <= maxH && rate <= maxRate) {
      return String(idx).padStart(2, "0");
    }
  }
  return "19";
}

/** Read seq_profile from a low-overhead AV1 sequence-header OBU. Native
 * Surface frames carry the codec family; the bitstream carries the profile. */
export function av1SequenceProfile(data: Uint8Array): number | undefined {
  let offset = 0;
  while (offset < data.length) {
    const header = data[offset++];
    if (header & 0x81) return undefined; // forbidden/reserved bits
    const type = (header >> 3) & 15;
    if (header & 4) {
      if (offset >= data.length || data[offset++] & 7) return undefined;
    }
    let size = data.length - offset;
    if (header & 2) {
      size = 0;
      let complete = false;
      for (let i = 0; i < 8 && offset < data.length; i++) {
        const byte = data[offset++];
        size += (byte & 127) * 2 ** (i * 7);
        if (!(byte & 128)) {
          complete = true;
          break;
        }
      }
      if (!complete || !Number.isSafeInteger(size)) return undefined;
    }
    if (size > data.length - offset) return undefined;
    if (type === 1 && size > 0) {
      const profile = data[offset] >> 5;
      return profile <= 2 ? profile : undefined;
    }
    offset += size;
  }
  return undefined;
}
