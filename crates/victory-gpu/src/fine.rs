//! The title's small changes to copies in the card's memory under flights'
//! shader writes (`VICTORY_MERGE_FINE=1`; off by default).
//!
//! `Shadow::merge_under_writes` sends up exactly the bytes the title changed,
//! a buffer copy of its own for each run of them. In gameplay those are
//! 20,000 to 42,000 runs a frame of two or three bytes (0.05 to 0.11 MB), far
//! over the 512 a submission may send, and at the cap the submitting thread
//! waits for everything in the air: 24 to 27 ms a frame.
//!
//! Here the changed bytes are bits of a map per 64 KiB of the copy, and one
//! dispatch writes the words that hold them, each under a mask of its bytes
//! ([`SCATTER`]). No other byte of the copy is written, and none is taken for
//! the title's by the copy-back: as with the runs.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

const PAGE: usize = 64 << 10;

/// Whether it is on.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::host::relay() && std::env::var_os("VICTORY_MERGE_FINE").is_some_and(|v| v == "1"))
}

/// A set of bytes of a copy: a bit each, by 64 KiB.
#[derive(Default, Clone)]
pub(crate) struct Bytes(BTreeMap<usize, Box<[u64; PAGE / 64]>>);

impl Bytes {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Adds `[from, to)`.
    pub(crate) fn add(&mut self, from: usize, to: usize) {
        let mut at = from;
        while at < to {
            let page = at / PAGE;
            let end = to.min((page + 1) * PAGE);
            let bits = self.0.entry(page).or_insert_with(|| Box::new([0; PAGE / 64]));
            let (mut a, b) = (at % PAGE, end - page * PAGE);
            while a < b {
                let n = (64 - a % 64).min(b - a);
                bits[a / 64] |= (u64::MAX >> (64 - n)) << (a % 64);
                a += n;
            }
            at = end;
        }
    }

    /// The runs of `[from, to)` that are in the set, put into `out` in order.
    pub(crate) fn runs_in(&self, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
        if from >= to {
            return;
        }
        let begin = out.len();
        for (page, bits) in self.0.range(from / PAGE..=(to - 1) / PAGE) {
            let base = page * PAGE;
            let (lo, hi) = (from.max(base) - base, to.min(base + PAGE) - base);
            for (w, &all) in bits.iter().enumerate().take((hi - 1) / 64 + 1).skip(lo / 64) {
                let mut word = all;
                while word != 0 {
                    let first = word.trailing_zeros() as usize;
                    let n = (word >> first).trailing_ones() as usize;
                    let (a, b) = ((w * 64 + first).max(lo), (w * 64 + first + n).min(hi));
                    if a < b {
                        match out[begin..].last_mut() {
                            Some(last) if last.1 == base + a => last.1 = base + b,
                            _ => out.push((base + a, base + b)),
                        }
                    }
                    if first + n >= 64 {
                        break;
                    }
                    word &= u64::MAX << (first + n);
                }
            }
        }
    }

    /// The 4-byte words that hold a byte of the set: offset, and the mask of
    /// its bytes in the word.
    pub(crate) fn words(&self, mut put: impl FnMut(usize, u32)) {
        for (page, bits) in &self.0 {
            for (w, &word) in bits.iter().enumerate() {
                let mut left = word;
                while left != 0 {
                    let k = left.trailing_zeros() as usize / 4;
                    let four = (word >> (4 * k)) & 15;
                    let mask = (0..4).filter(|b| four >> b & 1 != 0).fold(0u32, |m, b| m | 0xff << (8 * b));
                    put(page * PAGE + w * 64 + 4 * k, mask);
                    left &= !(15 << (4 * k));
                }
            }
        }
    }

    /// No fewer than [`Bytes::words`] gives.
    pub(crate) fn most_words(&self) -> usize {
        self.0.values().flat_map(|bits| bits.iter()).map(|w| w.count_ones().min(16) as usize).sum()
    }

    /// Puts the bytes of `title` that differ from `held` (of one length, both
    /// of the copy's bytes from offset `at` on) into `held`, and their
    /// offsets into the set. How many.
    pub(crate) fn merge(&mut self, at: usize, title: &[u8], held: &mut [u8]) -> usize {
        let n = title.len().min(held.len());
        let (title, held) = (&title[..n], &mut held[..n]);
        let mut found = 0;
        // To a multiple of 8 of the copy, then eight bytes at a time.
        let head = (at.next_multiple_of(8) - at).min(n);
        let tail = head + (n - head) / 8 * 8;
        for i in (0..head).chain(tail..n) {
            if title[i] != held[i] {
                held[i] = title[i];
                self.add(at + i, at + i + 1);
                found += 1;
            }
        }
        let mut last = (0usize, 0u64);
        let mut i = head;
        while i < tail {
            // Equal stretches, by their longest steps.
            let step = (tail - i).min(512);
            if title[i..i + step] == held[i..i + step] {
                i += step;
                continue;
            }
            for k in (i..i + step).step_by(8) {
                let mut a = [0u8; 8];
                let mut b = [0u8; 8];
                a.copy_from_slice(&title[k..k + 8]);
                b.copy_from_slice(&held[k..k + 8]);
                let x = u64::from_le_bytes(a) ^ u64::from_le_bytes(b);
                if x == 0 {
                    continue;
                }
                // A bit for each byte that is not zero, the lowest first.
                let low = 0x7f7f_7f7f_7f7f_7f7fu64;
                let set = (((x & low) + low) | x) & !low;
                let bits = (set >> 7).wrapping_mul(0x0102_0408_1020_4080) >> 56;
                held[k..k + 8].copy_from_slice(&a);
                found += bits.count_ones() as usize;
                let offset = at + k;
                // Put into the set a word of 64 at a time.
                if last.0 != offset / 64 {
                    self.put(last);
                    last = (offset / 64, 0);
                }
                last.1 |= bits << (offset % 64);
            }
            i += step;
        }
        self.put(last);
        found
    }

    /// Adds 64 bytes' bits, those from offset 64 times the first on.
    fn put(&mut self, (word, bits): (usize, u64)) {
        if bits != 0 {
            let page = self.0.entry(word / (PAGE / 64)).or_insert_with(|| Box::new([0; PAGE / 64]));
            page[word % (PAGE / 64)] |= bits;
        }
    }

    /// The 64 KiB stretches with a byte of the set.
    pub(crate) fn pages(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.0.keys().map(|p| (p * PAGE, (p + 1) * PAGE))
    }
}

/// What a copy in the card's memory still has to send up this way, and what
/// it sent, each with its send's number, until a later fetch is merged.
#[derive(Default)]
pub(crate) struct Fine(Mutex<State>);

type State = (Bytes, Vec<(u64, Bytes)>);

impl Fine {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// [`Bytes::merge`] into what is to be sent.
    pub(crate) fn merge(&self, at: usize, title: &[u8], held: &mut [u8]) -> usize {
        self.lock().0.merge(at, title, held)
    }

    /// Takes what is to be sent, as send `number` (kept as such with
    /// `keep`); sends up to `fetched` are of no use any more.
    pub(crate) fn take(&self, number: u64, fetched: u64, keep: bool) -> Bytes {
        let mut f = self.lock();
        f.1.retain(|s| s.0 > fetched);
        let taken = std::mem::take(&mut f.0);
        if keep && !taken.is_empty() {
            f.1.push((number, taken.clone()));
        }
        taken
    }

    /// The bytes of `[from, to)` the card had not received when a fetch sent
    /// after send `fetched_after` ran: see `Shadow::card_merge_relay`.
    pub(crate) fn newer(&self, from: usize, to: usize, fetched_after: u64, out: &mut Vec<(usize, usize)>) {
        if from >= to {
            return;
        }
        let f = self.lock();
        for page in from / PAGE..=(to - 1) / PAGE {
            // The sends' sets as one, then its runs: the same pages are in
            // most of some thirty sends, and each set's runs, put out one
            // after the other, were sorted and joined by the caller.
            let mut all: Option<Box<[u64; PAGE / 64]>> = None;
            let newer = f.1.iter().filter(|s| s.0 > fetched_after).map(|s| &s.1);
            for bits in std::iter::once(&f.0).chain(newer).filter_map(|b| b.0.get(&page)) {
                match &mut all {
                    None => all = Some(bits.clone()),
                    Some(all) => all.iter_mut().zip(bits.iter()).for_each(|(a, b)| *a |= b),
                }
            }
            if let Some(all) = all {
                Bytes(BTreeMap::from([(page, all)])).runs_in(from, to, out);
            }
        }
    }
}

/// Writes words under masks: twelve bytes a word, its offset, mask and value.
/// Every word of a buffer is there once.
pub(crate) const SCATTER: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), SRV(t0), UAV(u0)"
cbuffer Span : register(b0) {
    uint first;
    uint count;
};
ByteAddressBuffer words : register(t0);
RWByteAddressBuffer to : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= count) return;
    uint3 w = words.Load3((first + id.x) * 12u);
    to.Store(w.x, (to.Load(w.x) & ~w.y) | (w.z & w.y));
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a plain set of bytes.
    #[test]
    fn a_set_of_bytes_gives_its_runs_and_words() {
        const N: usize = 3 * PAGE;
        let mut set = Bytes::default();
        let mut plain = vec![false; N];
        let mut x = 11usize;
        let mut ranges = vec![(60, 70), (PAGE - 3, PAGE + 200), (128, 192), (256, 257), (N - 1, N), (2 * PAGE - 64, 2 * PAGE)];
        for _ in 0..3000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let a = (x >> 30) % (N - 8);
            ranges.push((a, a + 1 + (x >> 50) % 7));
        }
        for (a, b) in ranges {
            set.add(a, b);
            plain[a..b].fill(true);
        }
        let runs_of = |from: usize, to: usize| {
            let mut want: Vec<(usize, usize)> = Vec::new();
            for at in (from..to).filter(|&at| plain[at]) {
                match want.last_mut() {
                    Some(last) if last.1 == at => last.1 = at + 1,
                    _ => want.push((at, at + 1)),
                }
            }
            want
        };
        for (from, to) in [(0, N), (61, 69), (63, 64), (64, 65), (PAGE - 2, PAGE + 2), (PAGE, 2 * PAGE), (100, 100), (N - 1, N)] {
            let mut got = Vec::new();
            set.runs_in(from, to, &mut got);
            assert_eq!(got, runs_of(from, to), "{from}..{to}");
        }
        let mut back = vec![false; N];
        let mut words = std::collections::HashSet::new();
        set.words(|at, mask| {
            assert!(at.is_multiple_of(4) && words.insert(at), "{at}");
            for b in 0..4 {
                let byte = mask >> (8 * b) & 0xff;
                assert!(byte == 0 || byte == 0xff);
                back[at + b] = byte != 0;
            }
        });
        assert!(back == plain);
    }

    /// A merged page is the title's, and the set is the bytes that differed,
    /// wherever the stretch begins and ends.
    #[test]
    fn a_merge_takes_the_bytes_that_differ() {
        let mut x = 3usize;
        let mut next = || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            x >> 33
        };
        for (at, n) in [(0, PAGE), (3, 5), (5, 3), (7, 1), (8, 8), (40, 4000), (PAGE - 9, 30), (PAGE - 1, 2), (123, 2 * PAGE + 77), (16, 0)] {
            for every in [1usize, 2, 5, 700, usize::MAX] {
                let was: Vec<u8> = (0..n).map(|_| next() as u8).collect();
                let mut title = was.clone();
                for b in title.iter_mut() {
                    if next() % every == 0 {
                        *b ^= 1 + (next() % 255) as u8;
                    }
                }
                let mut held = was.clone();
                let mut set = Bytes::default();
                let found = set.merge(at, &title, &mut held);
                assert!(held == title, "{at} {n} {every}");
                let want: Vec<usize> = (0..n).filter(|&i| was[i] != title[i]).map(|i| at + i).collect();
                let mut runs = Vec::new();
                set.runs_in(0, 4 * PAGE, &mut runs);
                let got: Vec<usize> = runs.into_iter().flat_map(|(a, b)| a..b).collect();
                assert!(got == want, "{at} {n} {every}");
                assert_eq!(found, want.len());
                let mut words = 0;
                set.words(|_, _| words += 1);
                assert!(words <= set.most_words());
            }
        }
    }

    /// A send's bytes stay out of merges of fetches not sent after it.
    #[test]
    fn sent_bytes_are_newer_until_a_later_fetch() {
        let fine = Fine::default();
        fine.merge(8, &[0, 0, 1, 1, 0], &mut [0; 5]);
        let newer = |after: u64| {
            let mut out = Vec::new();
            fine.newer(0, 100, after, &mut out);
            out
        };
        assert_eq!(newer(99), [(10, 12)], "not sent yet");
        assert!(!fine.take(5, 0, true).is_empty());
        assert_eq!(newer(4), [(10, 12)]);
        assert!(newer(5).is_empty());
        // Several sends' bytes as one set, in order, each run once.
        let several = Fine::default();
        several.merge(0, &[1; 20], &mut [0; 20]);
        several.take(1, 0, true);
        several.merge(PAGE - 4, &[1; 8], &mut [0; 8]);
        several.take(2, 0, true);
        several.merge(10, &[1; 20], &mut [0; 20]);
        let mut out = Vec::new();
        several.newer(5, 2 * PAGE, 0, &mut out);
        assert_eq!(out, [(5, 30), (PAGE - 4, PAGE), (PAGE, PAGE + 4)]);
        out.clear();
        several.newer(5, 2 * PAGE, 1, &mut out);
        assert_eq!(out, [(10, 30), (PAGE - 4, PAGE), (PAGE, PAGE + 4)]);
        assert!(fine.take(6, 5, true).is_empty());
        assert!(newer(4).is_empty(), "let go");
    }
}
