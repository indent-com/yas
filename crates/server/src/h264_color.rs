//! Set H.264 SPS color signalling without depending on a backend's VUI API.
//! In particular OpenH264's Rust API does not expose Display-P3 primaries.

struct Bits {
    data: Vec<bool>,
    pos: usize,
}
impl Bits {
    fn read(&mut self, n: usize) -> Option<u32> {
        let end = self.pos.checked_add(n)?;
        let mut value = 0;
        for &bit in self.data.get(self.pos..end)? {
            value = (value << 1) | u32::from(bit);
        }
        self.pos = end;
        Some(value)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut n = 0;
        while self.read(1)? == 0 {
            n += 1;
            if n > 30 {
                return None;
            }
        }
        Some((1 << n) - 1 + self.read(n)?)
    }
    fn se(&mut self) -> Option<i32> {
        let n = self.ue()?;
        Some(if n & 1 != 0 {
            n.div_ceil(2) as i32
        } else {
            -(n as i32 / 2)
        })
    }
}

fn append(bits: &mut Vec<bool>, value: u32, n: usize) {
    bits.extend((0..n).rev().map(|i| value & (1 << i) != 0));
}

fn sps_color(nal: &[u8], color: [u8; 4]) -> Option<Vec<u8>> {
    let mut rbsp = Vec::new();
    let mut zeros = 0;
    for &byte in nal.get(1..)? {
        if zeros == 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let data: Vec<bool> = rbsp
        .iter()
        .flat_map(|b| (0..8).rev().map(move |i| b & (1 << i) != 0))
        .collect();
    // Strip rbsp_trailing_bits, then restore after rewriting the VUI.
    let stop = data.iter().rposition(|&b| b)?;
    let mut bits = Bits {
        data: data[..stop].to_vec(),
        pos: 0,
    };
    let profile = bits.read(8)?;
    bits.read(16)?; // constraints + level
    bits.ue()?; // seq_parameter_set_id
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        let chroma = bits.ue()?;
        if chroma > 3 {
            return None;
        }
        if chroma == 3 {
            bits.read(1)?;
        }
        bits.ue()?;
        bits.ue()?;
        bits.read(1)?;
        if bits.read(1)? != 0 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if bits.read(1)? != 0 {
                    let mut last = 8i32;
                    let mut next = 8i32;
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + bits.se()?).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => {
            bits.ue()?;
        }
        1 => {
            bits.read(1)?;
            bits.se()?;
            bits.se()?;
            let cycle = bits.ue()?;
            if cycle > 255 {
                return None;
            }
            for _ in 0..cycle {
                bits.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    bits.ue()?;
    bits.read(1)?;
    bits.ue()?;
    bits.ue()?;
    if bits.read(1)? == 0 {
        bits.read(1)?;
    }
    bits.read(1)?;
    if bits.read(1)? != 0 {
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    let vui_pos = bits.pos;
    let has_vui = bits.read(1)? != 0;
    let mut signal = Vec::new();
    append(&mut signal, 1, 1); // video_signal_type_present_flag
    append(&mut signal, 5, 3); // unspecified video format
    append(&mut signal, u32::from(color[3] != 0), 1);
    append(&mut signal, 1, 1); // colour_description_present_flag
    for &value in &color[..3] {
        append(&mut signal, u32::from(value), 8);
    }
    if has_vui {
        if bits.read(1)? != 0 && bits.read(8)? == 255 {
            bits.read(32)?;
        }
        if bits.read(1)? != 0 {
            bits.read(1)?;
        }
        let start = bits.pos;
        if bits.read(1)? != 0 {
            bits.read(4)?;
            if bits.read(1)? != 0 {
                bits.read(24)?;
            }
        }
        bits.data.splice(start..bits.pos, signal);
    } else {
        bits.data.truncate(vui_pos);
        bits.data.extend([true, false, false]); // VUI, aspect ratio, overscan
        bits.data.extend(signal);
        bits.data.extend([false; 6]); // remaining optional VUI sections
    }
    bits.data.push(true);
    while !bits.data.len().is_multiple_of(8) {
        bits.data.push(false);
    }
    let mut out = vec![nal[0]];
    zeros = 0;
    for byte in bits
        .data
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| b.iter().fold(0u8, |v, &bit| (v << 1) | u8::from(bit)))
    {
        if zeros == 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    Some(out)
}

pub(crate) fn set_color(packet: &[u8], color: [u8; 4]) -> Option<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= packet.len() {
        let length = if packet[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if packet[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        starts.push((i, length));
        i += length;
    }
    let mut output = Vec::new();
    output.extend_from_slice(&packet[..starts.first().map_or(packet.len(), |v| v.0)]);
    for (index, &(start, length)) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(packet.len(), |v| v.0);
        let nal = &packet[start + length..end];
        output.extend_from_slice(&packet[start..start + length]);
        if nal.first().is_some_and(|v| v & 31 == 7) {
            output.extend_from_slice(&sps_color(nal, color)?);
        } else {
            output.extend_from_slice(nal);
        }
    }
    Some(output)
}
