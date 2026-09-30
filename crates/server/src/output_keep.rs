//! A process output stream kept as its head and tail (SPAWN `KEEP_OUTPUT`): what comes between
//! is dropped as it is read, and counted, so a command writing far more than its client keeps
//! runs at the speed of its pipe rather than of the client's window.
//!
//! The cuts fall between characters as a WHATWG UTF-8 decoder with replacement (a JavaScript
//! `TextDecoder`, Rust's `from_utf8_lossy`) reads the whole stream: the head ends where such a
//! decoder is between characters, or where the next byte cannot continue the character it is in;
//! the tail starts between characters, with no continuation byte. Decoding the head and the tail, apart or
//! one after the other, then gives exactly the characters it gives them within the whole, and
//! the counts of what was dropped (bytes, lines, code points, UTF-16 units) complete it: a client
//! can say how much it did not get as it would have counted it.

use std::collections::VecDeque;

/// What was dropped from a stream: from `offset` on (the head's length), `bytes` bytes that
/// decode to `code_points` characters, `utf16_units` UTF-16 code units, `lines` of them `\n`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Elision {
    pub(crate) offset: u64,
    pub(crate) bytes: u64,
    pub(crate) lines: u64,
    pub(crate) code_points: u64,
    pub(crate) utf16_units: u64,
}

/// A WHATWG UTF-8 decoder's state, reading one byte at a time.
#[derive(Clone, Copy, Debug)]
struct Utf8Scan {
    needed: u8,
    seen: u8,
    lower: u8,
    upper: u8,
}

impl Default for Utf8Scan {
    fn default() -> Self {
        Self {
            needed: 0,
            seen: 0,
            lower: 0x80,
            upper: 0xBF,
        }
    }
}

/// What a byte did to a [`Utf8Scan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Part of a character still incomplete.
    Pending,
    /// It ended a character of this many UTF-16 units (a replacement for an invalid byte is 1).
    Char { units: u8, newline: bool },
    /// It cannot continue the pending character: that one ends as a replacement (1 unit) before
    /// it, and the byte must be read again, from between characters.
    Again,
}

impl Utf8Scan {
    fn between_characters(&self) -> bool {
        self.needed == 0
    }

    fn step(&mut self, byte: u8) -> Step {
        if self.needed == 0 {
            return match byte {
                0x00..=0x7F => Step::Char {
                    units: 1,
                    newline: byte == b'\n',
                },
                0xC2..=0xDF => {
                    self.needed = 1;
                    Step::Pending
                }
                0xE0..=0xEF => {
                    if byte == 0xE0 {
                        self.lower = 0xA0;
                    } else if byte == 0xED {
                        self.upper = 0x9F;
                    }
                    self.needed = 2;
                    Step::Pending
                }
                0xF0..=0xF4 => {
                    if byte == 0xF0 {
                        self.lower = 0x90;
                    } else if byte == 0xF4 {
                        self.upper = 0x8F;
                    }
                    self.needed = 3;
                    Step::Pending
                }
                _ => Step::Char {
                    units: 1,
                    newline: false,
                },
            };
        }
        if !(self.lower..=self.upper).contains(&byte) {
            *self = Self::default();
            return Step::Again;
        }
        self.lower = 0x80;
        self.upper = 0xBF;
        self.seen += 1;
        if self.seen < self.needed {
            return Step::Pending;
        }
        // Four bytes make a code point beyond the BMP: two UTF-16 units.
        let units = if self.needed == 3 { 2 } else { 1 };
        *self = Self::default();
        Step::Char {
            units,
            newline: false,
        }
    }
}

/// Counts what goes through a [`Utf8Scan`], byte by byte.
#[derive(Debug, Default)]
struct Counter {
    scan: Utf8Scan,
    counts: Elision,
}

impl Counter {
    fn count(&mut self, units: u8, newline: bool) {
        self.counts.code_points += 1;
        self.counts.utf16_units += u64::from(units);
        self.counts.lines += u64::from(newline);
    }

    fn byte(&mut self, byte: u8) {
        self.counts.bytes += 1;
        loop {
            match self.scan.step(byte) {
                Step::Pending => return,
                Step::Char { units, newline } => {
                    self.count(units, newline);
                    return;
                }
                Step::Again => self.count(1, false),
            }
        }
    }

    /// Bytes in order: runs of ASCII at once.
    fn bytes(&mut self, data: &[u8]) {
        let mut at = 0;
        while at < data.len() {
            if self.scan.between_characters() {
                let run = data[at..]
                    .iter()
                    .position(|byte| !byte.is_ascii())
                    .unwrap_or(data.len() - at);
                if run > 0 {
                    let ascii = &data[at..at + run];
                    let run = run as u64;
                    self.counts.bytes += run;
                    self.counts.code_points += run;
                    self.counts.utf16_units += run;
                    self.counts.lines += ascii.iter().filter(|byte| **byte == b'\n').count() as u64;
                    at += ascii.len();
                    continue;
                }
            }
            self.byte(data[at]);
            at += 1;
        }
    }

    /// A byte that may continue the pending character: false when it cannot (the character
    /// ends as a replacement before it, and the byte is not taken).
    fn continue_with(&mut self, byte: u8) -> bool {
        match self.scan.step(byte) {
            Step::Again => {
                self.count(1, false);
                false
            }
            Step::Pending => {
                self.counts.bytes += 1;
                true
            }
            Step::Char { units, newline } => {
                self.counts.bytes += 1;
                self.count(units, newline);
                true
            }
        }
    }

    /// The end of the stream: a character left incomplete is a replacement.
    fn end(&mut self) {
        if !self.scan.between_characters() {
            self.scan = Utf8Scan::default();
            self.count(1, false);
        }
    }
}

/// One output stream kept as its head and tail.
#[derive(Debug)]
pub(crate) struct KeptOutput {
    /// Bytes to keep at the start, at least: the head ends between characters.
    head: u64,
    /// Bytes to keep at the end, at most.
    tail: usize,
    /// The head so far, and how a decoder stands at its end.
    head_len: u64,
    head_scan: Utf8Scan,
    head_done: bool,
    /// The last bytes read since the head, at most `tail` of them.
    ring: VecDeque<u8>,
    /// What left the ring at its front: dropped, in order.
    dropped: Counter,
    finished: bool,
    /// Once finished: the tail, and how much of it went out.
    tail_out: Vec<u8>,
    tail_sent: usize,
}

impl KeptOutput {
    pub(crate) fn new(head: u64, tail: usize) -> Self {
        Self {
            head,
            tail,
            head_len: 0,
            head_scan: Utf8Scan::default(),
            head_done: false,
            ring: VecDeque::new(),
            dropped: Counter::default(),
            finished: false,
            tail_out: Vec::new(),
            tail_sent: 0,
        }
    }

    /// Whether what is fed now goes out (the head is still being taken): only then need the
    /// stream wait for its reader to take it.
    pub(crate) fn sends_now(&self) -> bool {
        !self.head_done && !self.finished
    }

    /// Whether the stream is finished and its tail all went out.
    pub(crate) fn tail_out(&self) -> bool {
        self.finished && self.tail_sent == self.tail_out.len()
    }

    /// Once finished: the next at most `max` bytes of the tail, taken as sent.
    pub(crate) fn next_tail_chunk(&mut self, max: usize) -> Option<Vec<u8>> {
        let rest = &self.tail_out[self.tail_sent..];
        if !self.finished || rest.is_empty() {
            return None;
        }
        let chunk = rest[..rest.len().min(max)].to_vec();
        self.tail_sent += chunk.len();
        Some(chunk)
    }

    /// The stream stops before its tail was taken (its reader was stopped, or the exit is
    /// reported first): what it kept counts as dropped, to the stream's end. A tail already
    /// being sent is left as it is.
    pub(crate) fn drop_rest(&mut self) {
        if self.finished {
            return;
        }
        self.finish();
        let rest = std::mem::take(&mut self.tail_out);
        self.dropped.bytes(&rest);
        self.dropped.end();
    }

    /// Take `data` as read from the stream, and answer what of it goes out now: the start of
    /// it that belongs to the head (nothing once that is complete, or once finished).
    pub(crate) fn feed<'a>(&mut self, data: &'a [u8]) -> &'a [u8] {
        if self.finished {
            return &[];
        }
        let mut taken = 0;
        while !self.head_done && taken < data.len() {
            if self.head_len >= self.head && self.head_scan.between_characters() {
                self.head_done = true;
                break;
            }
            match self.head_scan.step(data[taken]) {
                // The pending character ends (a replacement) before this byte: past the head's
                // size, so does the head; else the byte is read again, between characters.
                Step::Again if self.head_len >= self.head => self.head_done = true,
                Step::Again => {}
                Step::Pending | Step::Char { .. } => {
                    taken += 1;
                    self.head_len += 1;
                }
            }
        }
        self.keep(&data[taken..]);
        &data[..taken]
    }

    /// Push bytes past the head into the ring; what that pushes out of it is dropped.
    fn keep(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let excess = (self.ring.len() + data.len()).saturating_sub(self.tail);
        let from_ring = excess.min(self.ring.len());
        if from_ring > 0 {
            let (first, second) = self.ring.as_slices();
            let in_first = from_ring.min(first.len());
            self.dropped.bytes(&first[..in_first]);
            self.dropped.bytes(&second[..from_ring - in_first]);
            self.ring.drain(..from_ring);
        }
        let from_data = excess - from_ring;
        self.dropped.bytes(&data[..from_data]);
        self.ring.extend(&data[from_data..]);
    }

    /// The stream ended (or stops being forwarded): its tail, which goes out last
    /// ([`next_tail_chunk`](Self::next_tail_chunk)), is what the ring holds from the first
    /// character boundary on. Then nothing more is taken.
    pub(crate) fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // The dropped bytes may end within a character: the ring's first bytes complete it, or
        // it ends as a replacement before them.
        while !self.dropped.scan.between_characters() {
            let Some(&byte) = self.ring.front() else {
                self.dropped.end();
                break;
            };
            if !self.dropped.continue_with(byte) {
                break;
            }
            self.ring.pop_front();
        }
        // The head may end within a character (one that the next byte broke): a tail starting
        // with a byte that could continue it would decode differently after it than apart.
        // Once anything was dropped, the tail starts with no continuation byte; those it would
        // start with are lone ones there, dropped as a replacement each.
        if self.dropped.counts.bytes > 0 {
            while let Some(&byte) = self.ring.front()
                && (0x80..=0xBF).contains(&byte)
            {
                self.dropped.byte(byte);
                self.ring.pop_front();
            }
        }
        self.tail_out = self.ring.drain(..).collect();
        self.ring = VecDeque::new();
    }

    /// What was dropped so far, where; None while nothing was.
    pub(crate) fn elision(&self) -> Option<Elision> {
        (self.dropped.counts.bytes > 0).then_some(Elision {
            offset: self.head_len,
            ..self.dropped.counts
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64: reproducible without a dependency.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: usize) -> usize {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            (x % bound as u64) as usize
        }
    }

    /// Text of every width with newlines, invalid bytes, truncated and overlong sequences, a
    /// surrogate's encoding and a BOM among it.
    fn mixed(rng: &mut Rng, len: usize) -> Vec<u8> {
        const PIECES: &[&[u8]] = &[
            b"a",
            b"\n",
            "é".as_bytes(),
            "中".as_bytes(),
            "文字".as_bytes(),
            "😀".as_bytes(),
            "𝄞".as_bytes(),
            b"\xFF",
            b"\x80",
            b"\xC3",
            b"\xC0\xAF",
            b"\xE4\xB8",
            b"\xF0\x9F\x98",
            b"\xED\xA0\x80",
            b"\xF4\x90\x80\x80",
            b"\xEF\xBB\xBF",
        ];
        let mut out = Vec::with_capacity(len + 4);
        while out.len() < len {
            out.extend_from_slice(PIECES[rng.below(PIECES.len())]);
        }
        out
    }

    fn utf16(bytes: &[u8]) -> Vec<u16> {
        String::from_utf8_lossy(bytes).encode_utf16().collect()
    }

    fn chars(bytes: &[u8]) -> u64 {
        String::from_utf8_lossy(bytes).chars().count() as u64
    }

    fn lines(bytes: &[u8]) -> u64 {
        bytes.iter().filter(|byte| **byte == b'\n').count() as u64
    }

    /// Keep `whole` fed in pieces of at most `piece` bytes; answer the head and tail sent.
    fn keep(
        whole: &[u8],
        head: u64,
        tail: usize,
        pieces: &mut dyn FnMut() -> usize,
    ) -> (Vec<u8>, Vec<u8>, KeptOutput) {
        let mut kept = KeptOutput::new(head, tail);
        let mut sent = Vec::new();
        let mut rest = whole;
        while !rest.is_empty() {
            let n = pieces().clamp(1, rest.len());
            sent.extend_from_slice(kept.feed(&rest[..n]));
            rest = &rest[n..];
        }
        kept.finish();
        let mut tail_sent = Vec::new();
        while let Some(chunk) = kept.next_tail_chunk(7) {
            tail_sent.extend_from_slice(&chunk);
        }
        assert!(kept.tail_out());
        (sent, tail_sent, kept)
    }

    /// The head and tail decode, apart or together, to the whole's first and last characters,
    /// and what was dropped counts the rest in every unit.
    fn check(whole: &[u8], head: u64, tail: usize, pieces: &mut dyn FnMut() -> usize) {
        let (head_sent, tail_sent, kept) = keep(whole, head, tail, pieces);
        let what = format!("{} bytes, head {head}, tail {tail}", whole.len());
        let all = utf16(whole);
        let (first, last) = (utf16(&head_sent), utf16(&tail_sent));
        assert_eq!(
            utf16(&[head_sent.as_slice(), &tail_sent].concat()),
            [first.as_slice(), &last].concat(),
            "{what}"
        );
        assert_eq!(&all[..first.len()], first.as_slice(), "{what}");
        assert_eq!(&all[all.len() - last.len()..], last.as_slice(), "{what}");
        let dropped = kept.elision().unwrap_or_default();
        assert_eq!(
            head_sent.len() as u64 + dropped.bytes + tail_sent.len() as u64,
            whole.len() as u64,
            "{what}"
        );
        assert_eq!(
            first.len() as u64 + dropped.utf16_units + last.len() as u64,
            all.len() as u64,
            "{what}"
        );
        assert_eq!(
            chars(&head_sent) + dropped.code_points + chars(&tail_sent),
            chars(whole),
            "{what}"
        );
        assert_eq!(
            lines(&head_sent) + dropped.lines + lines(&tail_sent),
            lines(whole),
            "{what}"
        );
        // The head is its size or all there is, and at most one character more; the tail at
        // most its size, and at most one character less once anything was dropped.
        assert!(
            head_sent.len() as u64 >= head.min(whole.len() as u64),
            "{what}"
        );
        assert!(head_sent.len() as u64 <= head + 3, "{what}");
        assert!(tail_sent.len() <= tail, "{what}");
        assert!(whole.ends_with(&tail_sent), "{what}");
        if dropped.bytes > 0 {
            assert_eq!(dropped.offset, head_sent.len() as u64, "{what}");
            let first = tail_sent.first();
            assert!(
                first.is_none_or(|byte| !(0x80..=0xBF).contains(byte)),
                "{what}"
            );
        } else {
            assert_eq!([head_sent, tail_sent].concat(), whole, "{what}");
        }
    }

    #[test]
    fn head_and_tail_of_any_text_decode_as_within_the_whole_and_the_counts_make_up_the_rest() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..4000 {
            let len = rng.below(600);
            let whole = mixed(&mut rng, len);
            let head = rng.below(whole.len() + 8) as u64;
            let tail = rng.below(whole.len() + 8);
            let mut seed = Rng(rng.below(usize::MAX) as u64 | 1);
            check(&whole, head, tail, &mut || 1 + seed.below(40));
        }
    }

    #[test]
    fn cuts_within_wide_characters_move_to_their_ends() {
        // An emoji across the head's end, CJK across the tail's start.
        let whole = [
            b"ab".as_slice(),
            "😀".as_bytes(),
            "中".repeat(10).as_bytes(),
            "文".as_bytes(),
            b"yz",
        ]
        .concat();
        for head in 2..8 {
            for tail in 0..12 {
                for piece in [1, 2, 3, 5, 64] {
                    check(&whole, head, tail, &mut || piece);
                }
            }
        }
        let (head_sent, tail_sent, kept) = keep(&whole, 3, 7, &mut || 1);
        assert_eq!(head_sent, [b"ab".as_slice(), "😀".as_bytes()].concat());
        assert_eq!(tail_sent, ["文".as_bytes(), b"yz"].concat());
        assert_eq!(
            kept.elision(),
            Some(Elision {
                offset: 6,
                bytes: 30,
                lines: 0,
                code_points: 10,
                utf16_units: 10
            })
        );
    }

    #[test]
    fn a_head_ending_within_a_character_never_meets_a_tail_that_continues_it() {
        // The head ends with ED (the byte after, A0, cannot continue it); the tail would start
        // with 90 80, which can (ED 90 80 is U+D400): those go.
        let whole = [b"ab\xED\xA0".as_slice(), &[b'x'; 20], b"\x90\x80z"].concat();
        let (head_sent, tail_sent, kept) = keep(&whole, 3, 3, &mut || 1);
        assert_eq!(head_sent, b"ab\xED");
        assert_eq!(tail_sent, b"z");
        let joined = [head_sent.as_slice(), &tail_sent].concat();
        assert_eq!(String::from_utf8_lossy(&joined), "ab\u{FFFD}z");
        // A0, the x's, then 90 and 80: a replacement each.
        assert_eq!(kept.elision().unwrap().utf16_units, 1 + 20 + 2);
        check(&whole, 3, 3, &mut || 1);
    }

    #[test]
    fn nothing_is_dropped_from_what_fits() {
        let whole = "line\n".repeat(100).into_bytes();
        let (head_sent, tail_sent, kept) = keep(&whole, 300, 300, &mut || 7);
        assert_eq!([head_sent, tail_sent].concat(), whole);
        assert_eq!(kept.elision(), None);
    }

    #[test]
    fn a_long_ascii_stream_is_counted_in_bulk() {
        let whole = "y\n".repeat(1 << 20).into_bytes();
        let (head_sent, tail_sent, kept) = keep(&whole, 1000, 500, &mut || 65536);
        assert_eq!(head_sent.len(), 1000);
        assert_eq!(tail_sent.len(), 500);
        let dropped = kept.elision().unwrap();
        assert_eq!(dropped.bytes, (2 << 20) - 1500);
        assert_eq!(dropped.utf16_units, dropped.bytes);
        assert_eq!(dropped.lines, dropped.bytes / 2);
    }

    #[test]
    fn a_stream_stopped_before_its_tail_drops_all_it_kept() {
        let mut whole = "head ✓ then 🙂 and 中文\n".repeat(50).into_bytes();
        // It stops within a character: that ends as a replacement.
        whole.extend_from_slice(&[0xF0, 0x9F]);
        let mut kept = KeptOutput::new(10, 64);
        let sent = kept.feed(&whole).to_vec();
        assert_eq!(sent, b"head \xE2\x9C\x93 t");
        assert!(!kept.sends_now());
        kept.drop_rest();
        assert!(kept.tail_out());
        assert_eq!(kept.next_tail_chunk(1024), None);
        assert!(kept.feed(b"more").is_empty());
        let rest = &whole[sent.len()..];
        assert_eq!(
            kept.elision(),
            Some(Elision {
                offset: sent.len() as u64,
                bytes: rest.len() as u64,
                lines: lines(rest),
                code_points: chars(rest),
                utf16_units: utf16(rest).len() as u64,
            })
        );
        // Once its tail is taken, nothing more is dropped.
        let mut finished = KeptOutput::new(4, 8);
        assert_eq!(finished.feed(b"abcdefghijklmnop"), b"abcd");
        finished.finish();
        assert_eq!(finished.next_tail_chunk(5).unwrap(), b"ijklm");
        finished.drop_rest();
        assert_eq!(finished.next_tail_chunk(5).unwrap(), b"nop");
        assert!(finished.tail_out());
        assert_eq!(finished.elision().unwrap().bytes, 4);
    }
}
