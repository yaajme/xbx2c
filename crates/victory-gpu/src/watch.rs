//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection. Blocks found
//! wholly uncommitted are skipped while they remain so; one query can
//! confirm a whole region, including ordinary reservations between views.
//!
//! Writes into physical pages through another view are seen too: the
//! runtime tells us the views of the pages it wrote itself ([`writing`],
//! xmem's `write_physical` and DirectStorage's page destinations), and a
//! write by the title through any view of a guarded page faults like one
//! at the guarded address (the guard makes every view of the page
//! read-only, [`open`] gives them all back; not a page of more than
//! [`MANY_VIEWS`] views). A
//! system call writing into a guarded block fails (none is known to).
//!
//! With `VICTORY_LIGHT_REFRESH` a block that is read-only by our hand,
//! unwritten since, and whose earlier writes are all in the journal has a
//! bit ([`PUBLISHED`]): a range whose blocks all have theirs is answered
//! from the journal alone, with no lock of the guards and no kernel call
//! ([`light`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

/// A title-memory range whose changed pages another subsystem needs to read.
pub(crate) struct Tracked {
    start: usize,
    end: usize,
    seen: Mutex<BTreeMap<usize, u64>>,
}

/// Tracks a runtime GPU-memory range, including physical-page views when
/// `views` is true. Other process memory is not tracked.
pub(crate) fn track(start: usize, len: usize, views: bool) -> Option<Tracked> {
    let end = start.checked_add(len)?;
    if len == 0 || writes(start, end, views).is_none() {
        return None;
    }
    Some(Tracked {
        start,
        end,
        seen: Mutex::default(),
    })
}

struct Journal {
    /// Numbers the askings; 0 is "never".
    asked: u64,
    /// Page number → the asking that last found it written.
    written: BTreeMap<usize, u64>,
    /// Room for the kernel's answer.
    found: Vec<usize>,
    /// Page ranges asked ahead for a submission's refreshes ([`ask_ahead`])
    /// and the asking that did: `writes` answers for a range inside one
    /// from the journal, without another kernel call.
    ahead: Vec<(usize, usize)>,
    ahead_asked: u64,
    /// The ahead asking stands for a whole submission ([`ask_once`]): no
    /// other ahead asking is made, and `ahead` stays, until
    /// [`ask_once_end`].
    kept: bool,
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    asked: 0,
    written: BTreeMap::new(),
    found: Vec::new(),
    ahead: Vec::new(),
    ahead_asked: 0,
    kept: false,
});

/// Ranges of one submission closer than this are asked about together:
/// walking the pages between them costs less than another kernel call (a
/// call that resets written pages costs 30-70 microseconds whatever the
/// range).
const AHEAD_GAP: usize = (8 << 20) / PAGE;
/// The most one asking covers (its answer needs 8 bytes per 4 KiB page).
const AHEAD_MAX: usize = (1 << 30) / PAGE;

/// Asks the kernel about many ranges `(start, end)` at once, before the
/// [`writes`] of each: ranges close together are one asking, and `writes`
/// then answers from the journal until [`ask_ahead_done`]. The world's
/// submissions refresh thousands of host copies (6,664 askings a frame,
/// 456 ms, in run 14). Ranges that are not write-watched, or lie alone, are
/// left to `writes`.
pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) {
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| r.0 < r.1)
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.len() < 2 {
        return;
    }
    pieces.sort_unstable();
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    // The submission's one asking stands ([`ask_once`]): what it does not
    // hold is left to `writes`.
    if j.kept {
        return;
    }
    // View memory the guards hold is left to `writes`, which answers it
    // from the guards ([`known_view`]): the kernel's write watch can only
    // fail there. Asked here with the others, a run holding such a range
    // failed, was asked again by halves down to single ranges, failing
    // each time, and the range then went to `writes` all the same (after
    // triippywhale d0581a55). `VICTORY_VIEW_ASK=1` asks as before.
    #[cfg(feature = "diagnostics")]
    let all = pieces.len();
    pieces.retain(|&(first, _)| !known_view(first * PAGE));
    #[cfg(feature = "diagnostics")]
    AHEAD_VIEWS.fetch_add((all - pieces.len()) as u64, std::sync::atomic::Ordering::Relaxed);
    j.asked += 1;
    j.ahead_asked = j.asked;
    j.ahead.clear();
    let mut i = 0;
    while i < pieces.len() {
        let first = pieces[i].0;
        let mut last = pieces[i].1;
        let mut k = i + 1;
        while k < pieces.len()
            && pieces[k].0 <= last + AHEAD_GAP
            && last.max(pieces[k].1) - first <= AHEAD_MAX
        {
            last = last.max(pieces[k].1);
            k += 1;
        }
        ask_run(j, &pieces[i..k]);
        i = k;
    }
}

/// Since the last frame line: the kernel askings that took a millisecond
/// or more, their microseconds, and the longest asking's. An asking walks
/// page tables, some ten microseconds for most ranges; in run 75 the 2,644
/// askings of a frame took 219 ms in the lines where the pass over the
/// heap's views ran long, 25-50 ms in the others. These numbers say
/// whether a few askings wait for something or all of them are slow.
#[cfg(feature = "diagnostics")]
static SLOW: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(feature = "diagnostics")]
fn slow(us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if us >= 1000 {
        SLOW[0].fetch_add(1, Relaxed);
        SLOW[1].fetch_add(us, Relaxed);
    }
    SLOW[2].fetch_max(us, Relaxed);
}

/// Since the last frame line: ranges of view memory the guards hold that a
/// submission's askings ahead ([`ask_ahead`], [`ask_once`]) left out. Each
/// was a kernel asking that failed, or several, before.
#[cfg(feature = "diagnostics")]
pub(crate) static AHEAD_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`SLOW`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn slow_asks() -> [u64; 3] {
    [0, 1, 2].map(|i| SLOW[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// The submission's refreshes are done: `writes` asks the kernel again.
pub(crate) fn ask_ahead_done() {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if !j.kept {
        j.ahead.clear();
    }
}

/// Whether a submission asks the kernel once, before its first refresh,
/// about every range its lists refresh (`VICTORY_ASK_ONCE=1`; off by
/// default).
///
/// A submission's refreshes are done in groups, between the title's own
/// buffer copies (each copy flushes the refreshes recorded before it), and
/// every group asked the kernel again: 2,644 askings a frame, 70 a
/// submission. With the switch the journal answers all of them from one
/// asking made when the submission starts.
///
/// What changes: memory the title's other threads write while the
/// submission is being prepared is seen by the next submission's asking,
/// not by this one's later groups. What this submission's lists read was
/// written before they were submitted, so before the asking. Our own
/// writes into title memory while preparing are told to the journal
/// ([`wrote`]) or end the asking ([`ask_once_end`]).
pub(crate) fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default since run 81 (with card buffers: 375-450 ms a frame
    // against 573-647, the picture unchanged over four minutes);
    // `VICTORY_ASK_ONCE=0` asks per range, as before.
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASK_ONCE").is_none_or(|v| v != "0"))
}

/// With [`once`]: asks about `ranges` (everything a submission's lists
/// refresh) as [`ask_ahead`] does, and lets that asking stand until
/// [`ask_once_end`].
pub(crate) fn ask_once(ranges: &[(usize, usize)]) {
    if once() {
        ask_once_of(ranges, guard_once());
    }
}

/// [`ask_once`]; `guarded`: of the guards too.
fn ask_once_of(ranges: &[(usize, usize)], guarded: bool) {
    // The light refresh: a few of the holes known by their bits are asked
    // of Windows again, a submission at a time ([`look_at_holes`]).
    if light_kept() {
        look_at_holes(HOLES_A_LOOK);
    }
    ask_once_end();
    // With the guards asked too: view memory they hold is not put to the
    // kernel, whose call only fails there (and is made again by halves).
    let views: Vec<bool> = if guarded && !view_ask() {
        let g = guards();
        ranges
            .iter()
            .map(|r| g.protected.contains(&(r.0 / BLOCK)) || g.absent.contains(&(r.0 / BLOCK)))
            .collect()
    } else {
        vec![false; ranges.len()]
    };
    let watched: Vec<(usize, usize)> = ranges.iter().zip(&views).filter(|(_, view)| !**view).map(|(r, _)| *r).collect();
    ask_ahead(&watched);
    // Uncommitted memory found by this asking: not asked about again in it.
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    if j.ahead.is_empty() {
        // `ask_ahead` asked nothing (fewer than two ranges).
        j.asked += 1;
        j.ahead_asked = j.asked;
    }
    // `ask_ahead` leaves a range that lies alone to `writes`, which asks
    // once per flush: with one asking for the submission that was every
    // such range asked again at each of its 70 flushes (run 76: 2,300
    // askings a frame with the switch on, as without it). They are asked
    // here, once; what is not write-watched stays with `writes`.
    for (&(start, end), &view) in ranges.iter().zip(&views) {
        if start >= end {
            continue;
        }
        // View memory the guards hold is not asked about: the asking can
        // only fail there, one failed kernel call for each such copy at
        // every submission, and `writes` answers it from the guards
        // (after triippywhale cc368c7c). (With the guards asked too, they
        // answer it below, [`guard_once`].)
        if !guarded && known_view(start) {
            #[cfg(feature = "diagnostics")]
            AHEAD_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        let (first, last) = (start / PAGE, end.div_ceil(PAGE));
        if j.ahead.iter().any(|&(a, b)| a <= first && last <= b) {
            continue;
        }
        if (!view && ask_kernel(j, first, last)) || (guarded && ask_guards(j, first, last)) {
            j.ahead.push((first, last));
        }
    }
    j.kept = !j.ahead.is_empty();
}

/// Whether a submission's one asking ([`ask_once`]) asks the guards too
/// (`VICTORY_GUARD_ONCE=1`; off by default). Nearly all a submission refreshes
/// is view memory, which the kernel does not watch: 2,005 of 2,316 askings a
/// frame went to the guards one by one.
pub(crate) fn guard_once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GUARD_ONCE").is_some_and(|v| v == "1"))
}

/// [`ask_kernel`] of the guards, for view memory they hold already.
fn ask_guards(j: &mut Journal, first: usize, last: usize) -> bool {
    if !known_view(first * PAGE) {
        return false;
    }
    let Some(written) = guard(first * PAGE, last * PAGE) else {
        return false;
    };
    let now = j.ahead_asked;
    for page in written {
        j.written.insert(page, now);
    }
    true
}

/// The submission's one asking stands no longer: its refreshes are done,
/// or title memory was written in a way the journal was not told.
pub(crate) fn ask_once_end() {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if j.kept {
        j.kept = false;
        j.ahead.clear();
    }
}

/// We wrote title memory `[at, at + len)` while a submission's one asking
/// stands (the title's buffer copies, done at submission): its pages count
/// as written after that asking, so the refreshes that follow copy them.
pub(crate) fn wrote(at: usize, len: usize) {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if !j.kept || len == 0 {
        return;
    }
    j.asked += 1;
    let now = j.asked;
    // The asking that stands moves with it: `asked` and `writes` give it as
    // "now", and a page noted written after "now" stays stale for every
    // copy that takes it (copied at `now`, written at `now + 1`): copied
    // again at every refresh, and a reason to settle flights
    // (fxceless/victory-fork#410).
    j.ahead_asked = now;
    for page in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
        j.written.insert(page, now);
        // The guards are not asked again in this submission: the page's
        // other views learn of it here (the guards' own note that it has
        // any went when the copy opened them).
        if guard_once() {
            // (Not of the page the title shows wherever it has nothing:
            // thousands of views, each guarded by itself.)
            let shown = crate::inplace::pages_of(page * PAGE);
            if shown.len() <= MANY_VIEWS {
                for alias in shown {
                    j.written.insert(alias / PAGE, now);
                }
            }
        }
    }
}

/// We wrote the 64 KiB page numbered `page` of title memory through a
/// view of our own (`crate::inplace::write_own`): no guard was opened and
/// no kernel write-watch bit set, so the journal is told here. The page,
/// and the same physical page at its other title addresses (`others`,
/// page numbers), count as written at a new asking, which every host copy
/// that holds them takes at its next refresh. Returns the asking that had
/// last found `page` written (0: never) and the new one, for the copy
/// that wrote to note itself current.
// (Upstream's; not wired in this merge: see the merge commit.)
#[allow(dead_code)]
pub(crate) fn wrote_own(page: usize, others: &[usize]) -> (u64, u64) {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    j.asked += 1;
    let now = j.asked;
    // As in [`wrote`]: an asking that stands for a submission, or for a
    // flush, moves with it, or a copy taking the page at the standing
    // asking would find it written "later" at every refresh.
    if j.kept || !j.ahead.is_empty() {
        j.ahead_asked = now;
    }
    let before = j.written.insert(page, now).unwrap_or(0);
    for &other in others {
        if other != page {
            j.written.insert(other, now);
        }
    }
    (before, now)
}

/// The asking that last found the 64 KiB page numbered `page` written (0:
/// never). Read only, for `crate::backreads`.
#[cfg(feature = "diagnostics")]
pub(crate) fn stamp(page: usize) -> u64 {
    JOURNAL.lock().unwrap_or_else(|e| e.into_inner()).written.get(&page).copied().unwrap_or(0)
}

/// One kernel asking for `pieces` (page ranges in order); by halves if it
/// fails, as a run across two reservations or into memory that is not
/// write-watched does. A piece alone is left to [`writes`].
fn ask_run(j: &mut Journal, pieces: &[(usize, usize)]) {
    if pieces.len() < 2 {
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if ask_kernel(j, first, last) {
        j.ahead.push((first, last));
        return;
    }
    let (a, b) = pieces.split_at(pieces.len() / 2);
    ask_run(j, a);
    ask_run(j, b);
}

/// Asks the kernel for the written pages of pages `[first, last)` and notes
/// them in the journal under the ahead asking; false (and nothing changed)
/// if the range is not write-watched memory of one reservation.
fn ask_kernel(j: &mut Journal, first: usize, last: usize) -> bool {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    if failed {
        return false;
    }
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    let now = j.ahead_asked;
    for k in 0..count {
        let page = j.found[k] / PAGE;
        j.written.insert(page, now);
    }
    true
}

/// What [`writes`] would say of `[start, end)` from the journal alone, if
/// the range lies in one asked ahead ([`ask_ahead`], [`ask_once`]): the
/// pages written since tracking began and the asking that last found each
/// written, with the number of the asking that stands (as [`writes`] gives
/// it). `None` if the kernel would have to be asked.
pub(crate) fn asked(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    j.ahead
        .iter()
        .any(|&(a, b)| a <= first && last <= b)
        .then(|| (j.ahead_asked, j.written.range(first..last).map(|(&p, &w)| (p, w)).collect()))
}

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    // The light refresh: its bits kept, and asked or not, as the switch
    // says now.
    let light = if light_kept() { 1 + u8::from(light()) } else { 0 };
    // Always on: the counts every 65,536 askings that allow for view
    // memory (in turn they are said at each change).
    if guarded && light_mode() == 1 {
        static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if (CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1) % 65_536 == 0 {
            let [bits, locked, filled] = light_asks();
            tracing::info!(
                "refreshes of host copies go the light way (VICTORY_LIGHT_REFRESH=1): of the last 65536 askings that allowed for view memory {bits} were answered by the bits of their blocks and {locked} took the guards' lock (the others were of memory the kernel watches); {filled} holes found filled with nobody telling"
            );
        }
    }
    writes_as(start, end, guarded, light)
}

/// [`writes`] with the light refresh as `light` says ([`light`]): 0, as
/// without the switch, no bit is set or asked; 1, the blocks the asking
/// leaves clean get their bits and none is asked (the phase of `alt`
/// without); 2, a range whose blocks all have their bit is answered from
/// the journal.
fn writes_as(start: usize, end: usize, guarded: bool, light: u8) -> Option<(u64, Vec<(usize, u64)>)> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    // Asked ahead with the submission's other ranges: the journal has it.
    if j.ahead.iter().any(|&(a, b)| a <= first && last <= b) {
        return Some((
            j.ahead_asked,
            j.written
                .range(first..last)
                .map(|(&p, &w)| (p, w))
                .collect(),
        ));
    }
    // The light refresh: every block of the range is read-only by our hand
    // and unwritten since an asking noted it, or a hole noted for good
    // ([`PUBLISHED`]). The guards would find nothing written and this
    // asking would note nothing: its number and the journal as it is are
    // the answer, with the guards' lock not taken and no kernel call.
    if guarded && light == 2 && published(j, first * PAGE, last * PAGE) {
        LIGHT_ASKS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        j.asked += 1;
        return Some((
            j.asked,
            j.written
                .range(first..last)
                .map(|(&p, &w)| (p, w))
                .collect(),
        ));
    }
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // View memory the guards already hold cannot be write-watched: the
    // kernel asking would only fail. Copies of it are flushed many times a
    // frame, each time paying that failed call before the guards answer.
    let known_view = guarded && known_view(first * PAGE);
    // SAFETY: the answer buffer holds `count` entries.
    let failed = known_view
        || unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                (first * PAGE) as *const _,
                (last - first) * PAGE,
                j.found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        } != 0;
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        if known_view {
            crate::copystat::C::WatchKnownView
        } else {
            crate::copystat::C::WatchFailed
        },
        failed as u64,
    );
    #[cfg(feature = "diagnostics")]
    if failed && !known_view && guarded {
        failed_asking(first * PAGE, last * PAGE);
    }
    let guarded = if failed {
        count = 0;
        if !guarded {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let guarding = std::time::Instant::now();
        let written = guard(first * PAGE, last * PAGE);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::GuardUs, guarding.elapsed().as_micros() as u64);
        written?
    } else {
        Vec::new()
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    j.asked += 1;
    let now = j.asked;
    let pages: Vec<usize> = j.found[..count].iter().map(|&at| at / PAGE).chain(guarded).collect();
    for &page in &pages {
        j.written.insert(page, now);
    }
    // The light refresh: the guards were asked, and what they found is
    // noted. The blocks they made clean are known by their bits from now
    // on (the journal's lock is still held: no other asking came between).
    if failed && light != 0 {
        LIGHT_ASKS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        publish(j, &pages, first * PAGE, last * PAGE, told());
    }
    Some((
        now,
        j.written
            .range(first..last)
            .map(|(&p, &w)| (p, w))
            .collect(),
    ))
}

/// Diagnostics: one in 2,048 of the askings the kernel's write watch
/// refused for memory the guards did not know (some 200 a frame in the
/// first rooms, each followed by a question to Windows about the memory):
/// what the memory is and what the guards hold of its first block.
#[cfg(feature = "diagnostics")]
fn failed_asking(start: usize, end: usize) {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 2048 != 0 {
        return;
    }
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(start as *const _, &mut info, size_of_val(&info)) };
    let (protected, clean, absent, viewed, aliased) = {
        let g = guards();
        let b = start / BLOCK;
        (g.protected.contains(&b), g.clean.contains(&b), g.absent.contains(&b), g.viewed.contains(&b), g.aliased.contains(&b))
    };
    tracing::info!(
        "a failed write-watch asking (one in 2048): {start:#x}+{:#x}; Windows says (answered {n}): base {:#x} region {:#x} state {:#x} type {:#x} protect {:#x} allocation protect {:#x}; \
         the guards of its first block: protected {protected} clean {clean} absent {absent} viewed {viewed} aliased {aliased}; is a view: {}",
        end - start,
        info.BaseAddress as usize,
        info.RegionSize,
        info.State,
        info.Type,
        info.Protect,
        info.AllocationProtect,
        is_view(start, end)
    );
}

impl Tracked {
    /// Takes changed pages of `[from, to)`, cut to the tracked range. A
    /// partial page stays changed so a later take can read its other bytes.
    pub(crate) fn take(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let (from, to) = (from.max(self.start), to.min(self.end));
        if from >= to {
            return Vec::new();
        }
        let Some((_, pages)) = writes(from, to, true) else {
            return Vec::new();
        };
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for (page, write) in pages {
            if seen.get(&page).is_some_and(|&last| last >= write) {
                continue;
            }
            let (start, end) = ((page * PAGE).max(from), ((page + 1) * PAGE).min(to));
            if start >= end {
                continue;
            }
            // Keep partial pages dirty: the caller has not taken the rest.
            if (page * PAGE).max(self.start) >= from
                && ((page + 1) * PAGE).min(self.end) <= to
            {
                seen.insert(page, write);
            }
            match ranges.last_mut() {
                Some((_, last)) if *last == start => *last = end,
                _ => ranges.push((start, end)),
            }
        }
        ranges
    }
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

/// Whether the block holding `start` is view memory the guards already
/// hold (read-only by our hand, or noted absent): asking the kernel's write
/// watch about it can only fail. `VICTORY_VIEW_ASK=1` (or
/// `VICTORY_GUARD_ASK=1`) asks the kernel anyway, as before.
fn known_view(start: usize) -> bool {
    if view_ask() {
        return false;
    }
    let first = start / BLOCK;
    // The light refresh: a block known by its bit is read-only by our hand
    // or a hole the guards noted, which is what they are asked below.
    if light() && PUBLISHED.all(first, first + 1) {
        return true;
    }
    let g = guards();
    g.protected.contains(&first) || g.absent.contains(&first) || (view_known() && g.viewed.contains(&first))
}

/// Whether the kernel is asked about view memory too ([`known_view`]).
fn view_ask() -> bool {
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ASK.get_or_init(|| {
        ["VICTORY_VIEW_ASK", "VICTORY_GUARD_ASK"]
            .iter()
            .any(|n| std::env::var_os(n).is_some_and(|v| v == "1"))
    })
}

struct Guards {
    /// Blocks we made read-only.
    protected: BTreeSet<usize>,
    /// Of those, the ones not written since a copy took them.
    clean: BTreeSet<usize>,
    /// Blocks already copied as zeros. While they remain wholly uncommitted,
    /// the copy can skip hundreds of MB of failing kernel reads per world frame.
    absent: BTreeSet<usize>,
    /// Blocks whose physical page was guarded together with its other views
    /// (those views' blocks are in it too): a write to one opens them all.
    /// A write range can be huge (`fread`), so only these are looked up.
    aliased: BTreeSet<usize>,
    /// With [`view_known`]: blocks made read-only at least once since the
    /// runtime last mapped or unmapped them. They are view memory whether
    /// their guard stands now or a write has opened it.
    viewed: BTreeSet<usize>,
    /// With [`absent_once`]: for blocks of `absent`, when ([`guard_ms`])
    /// Windows last said they are uncommitted.
    verified: BTreeMap<usize, u64>,
    /// With the light refresh and notes kept for good ([`told`]): the
    /// blocks of `verified` that [`publish`] has looked at. Those of them
    /// that lie in a placeholder reservation ([`placeholder`]) are known by
    /// their bits ([`PUBLISHED`]) until the runtime tells of them; the
    /// others are answered from their notes, which then grow old
    /// ([`note_life`]).
    holes: BTreeSet<usize>,
}

/// Whether a block once guarded stays known as view memory while its
/// guard is open (`VICTORY_VIEW_KNOWN=1`; off by default).
///
/// A range whose first block the title wrote since the last check has
/// that block neither read-only nor noted absent, so nothing says it is a
/// view: the kernel's write watch is asked and fails, and then Windows is
/// asked what the memory is (`is_view`), a walk of the page tables to the
/// end of the pages that share the first one's state. In runs 222 and 225
/// that was 338-450 failed askings a frame and 6.5-7.8 ms a frame of the
/// submitting thread in `VirtualQuery`. With the switch the block is
/// remembered from the time it was first made read-only until the runtime
/// maps or unmaps it ([`title_memory_remapped`]).
///
/// `VICTORY_VIEW_KNOWN=alt`: twenty seconds with, twenty without, in turn,
/// each change said in the log, for measuring it in one scene. (Blocks
/// noted in one phase are forgotten at a remapping in either.)
fn view_known() -> bool {
    /// 0 off, 1 on, 2 in turn.
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    match *MODE.get_or_init(|| match std::env::var("VICTORY_VIEW_KNOWN").as_deref() {
        Ok("1") => 1,
        Ok("alt") => 2,
        _ => 0,
    }) {
        0 => false,
        1 => true,
        _ => {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let on = (EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs() / 20) % 2 == 1;
            static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
            if LAST.swap(u8::from(on), std::sync::atomic::Ordering::Relaxed) != u8::from(on) {
                tracing::info!(
                    "view memory stays known while its guard is open from now (VICTORY_VIEW_KNOWN=alt): {}",
                    if on { "ON" } else { "off, as usual" }
                );
            }
            on
        }
    }
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    clean: BTreeSet::new(),
    absent: BTreeSet::new(),
    aliased: BTreeSet::new(),
    viewed: BTreeSet::new(),
    verified: BTreeMap::new(),
    holes: BTreeSet::new(),
});

fn guards() -> std::sync::MutexGuard<'static, Guards> {
    GUARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the block `page` is read-only by our hand and unwritten since a
/// copy took it (diagnostics of `VICTORY_BACK_OWN_CHECK`).
pub(crate) fn clean(page: usize) -> bool {
    guards().clean.contains(&page)
}

/// `VICTORY_LIGHT_REFRESH`: 0 off, 1 on, 2 in turn ([`light`]).
fn light_mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_LIGHT_REFRESH").as_deref() {
        Ok("1") => 1,
        Ok("alt") => 2,
        _ => 0,
    })
}

/// Whether the bits of the light refresh are kept ([`PUBLISHED`]): with
/// the switch on, and in both phases of `alt`, so that the phase that asks
/// them finds them true whatever was written in the phase before.
fn light_kept() -> bool {
    light_mode() != 0
}

/// Whether an asking about view memory is answered from the bits of its
/// blocks where they can answer (`VICTORY_LIGHT_REFRESH=1`; off by
/// default).
///
/// A frame asks the guards about view memory 2,000 to 3,700 times, 4.5 to
/// 9.4 GB of it, to hear for 98-99 % that nothing was written; every such
/// asking takes the guards' lock, which the title's threads wait for in
/// their write faults, and reads the range's clean blocks off the set:
/// 22.8 ms a frame of the submitting thread in the rooms, 44.5 ms outdoors
/// (run 275). With the switch a range whose blocks all have their bit
/// ([`PUBLISHED`]) is answered from the journal alone ([`writes`],
/// [`nothing_new`]), and its first block is known for view memory without
/// the lock ([`known_view`]).
///
/// What changes in the answers: nothing. A range is known by its bits
/// exactly while the guards would find it all clean, or clean but for
/// holes noted for good ([`told`]); only who is asked changes.
///
/// `VICTORY_LIGHT_REFRESH=alt`: twenty seconds with, twenty without, in
/// turn, each change said in the log with the counts of the phase before.
/// The bits are kept in both phases ([`light_kept`]) and asked in one.
/// (After avatarmanz/VIctory-7900XTX 2aa8134b and 74045e46, by hand.)
pub(crate) fn light() -> bool {
    match light_mode() {
        0 => false,
        1 => true,
        _ => {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let on = (EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs() / 20) % 2 == 1;
            static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
            if LAST.swap(u8::from(on), std::sync::atomic::Ordering::Relaxed) != u8::from(on) {
                let [bits, locked, filled] = light_asks();
                tracing::info!(
                    "refreshes of host copies go the light way from now (VICTORY_LIGHT_REFRESH=alt): {}; since the change before: {bits} askings of view memory answered by the bits of their blocks, {locked} that took the guards' lock, {filled} holes found filled with nobody telling",
                    if on { "ON" } else { "off, as usual (the bits are kept)" }
                );
            }
            on
        }
    }
}

/// Since they were last taken ([`light_asks`]), with the light refresh
/// kept: askings of view memory answered by the bits of their blocks,
/// askings that went to the guards, and holes known by their bits that
/// Windows said were filled with nobody telling ([`look_at_holes`]).
static LIGHT_ASKS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// [`LIGHT_ASKS`], taken.
fn light_asks() -> [u64; 3] {
    [0, 1, 2].map(|i| LIGHT_ASKS[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// The words of a piece of [`Bits`]: the bits of 4 GiB.
const BITS_WORDS: usize = 1 << 10;
/// The pieces of [`Bits`]: 128 TiB, what a process has.
const BITS_PIECES: usize = 1 << 15;

/// A bit per 64 KiB block of the address space, read and cleared with no
/// lock; in pieces made when first needed and never let go.
struct Bits([std::sync::atomic::AtomicPtr<BitsPiece>; BITS_PIECES]);

/// The bits of 4 GiB.
type BitsPiece = [std::sync::atomic::AtomicU64; BITS_WORDS];

impl Bits {
    const fn new() -> Bits {
        Bits([const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; BITS_PIECES])
    }

    /// The piece that holds word `word`, if it was ever made.
    fn piece(&self, word: usize) -> Option<&BitsPiece> {
        let piece = self.0.get(word / BITS_WORDS)?.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: null, or a piece made by `set`, which is never freed.
        unsafe { piece.as_ref() }
    }

    /// Sets `block`'s bit (nothing past the address space).
    fn set(&self, block: usize) {
        use std::sync::atomic::Ordering::{AcqRel, Acquire, SeqCst};
        let word = block / 64;
        let Some(place) = self.0.get(word / BITS_WORDS) else {
            return;
        };
        if place.load(Acquire).is_null() {
            let made: *mut BitsPiece = Box::into_raw(Box::new([const { std::sync::atomic::AtomicU64::new(0) }; BITS_WORDS]));
            if place.compare_exchange(std::ptr::null_mut(), made, AcqRel, Acquire).is_err() {
                // SAFETY: made above and shown to nobody: another thread's
                // piece is in the place.
                drop(unsafe { Box::from_raw(made) });
            }
        }
        if let Some(piece) = self.piece(word) {
            piece[word % BITS_WORDS].fetch_or(1u64 << (block % 64), SeqCst);
        }
    }

    /// Clears `block`'s bit: ahead of what makes the block writable, and
    /// seen by whoever looks after it.
    fn clear(&self, block: usize) {
        let word = block / 64;
        if let Some(piece) = self.piece(word) {
            piece[word % BITS_WORDS].fetch_and(!(1u64 << (block % 64)), std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the bits of all of `[first, last)` are set (not so of none).
    fn all(&self, first: usize, last: usize) -> bool {
        let mut block = first;
        while block < last {
            let word = block / 64;
            let until = ((word + 1) * 64).min(last);
            let mask = (u64::MAX >> (64 - (until - block))) << (block % 64);
            let Some(piece) = self.piece(word) else {
                return false;
            };
            if piece[word % BITS_WORDS].load(std::sync::atomic::Ordering::SeqCst) & mask != mask {
                return false;
            }
            block = until;
        }
        first < last
    }
}

/// The blocks an asking can answer for from the journal alone. A block has
/// its bit only while both of these hold: it is read-only by our hand and
/// noted clean (`Guards::clean`), or it is a hole noted for good that lies
/// in a placeholder reservation (`Guards::holes`); and the journal holds
/// every write to it that an asking found.
///
/// Why that is so. A bit is set in one place, [`publish`]: by the asking
/// that found the block written and had it made clean, after it noted that
/// in the journal and with the journal's lock still held, under the
/// guards' lock, and only if the block is clean at that moment. It is
/// cleared under the guards' lock before the block's write access is given
/// back (`open_here`, which the write fault's handler, [`writing`],
/// [`title_memory_written`] and [`views_mapped`] all go through), and when
/// the runtime tells that what the block shows is new
/// ([`title_memory_remapped`], [`title_memory_committed`]). Clearing does
/// nothing where no bit was ever set, so it is not behind the switch.
///
/// Who asks the bits holds the journal's lock from before the look until
/// it has read the journal ([`published`]), and nobody sets a bit without
/// that lock: a block opened and guarded again between the look and the
/// reading cannot have its bit back in time to be taken for unwritten. A
/// write that comes while the bits are looked at is no different from one
/// that comes just after an asking: its block's bit is gone, and the next
/// asking goes to the guards and finds the block open.
///
/// Our own stores through a view of our own (`crate::inplace::write_own`)
/// open no guard and take no bit: they are noted in the journal
/// ([`wrote_own`]), which is where an asking answered by the bits looks.
static PUBLISHED: Bits = Bits::new();

/// An asking of view memory `[start, end)` found the pages `written` (page
/// numbers) written, had the guards make their blocks read-only and clean
/// ([`guard`]) and has noted them in the journal, whose lock it holds
/// still (`_noted`): the blocks of them that are clean now are known by
/// their bits from here on ([`PUBLISHED`]). With `holes` ([`told`]), so
/// are the blocks of the range noted uncommitted for good that lie in a
/// placeholder reservation; Windows is asked that once for each
/// ([`placeholder`]).
///
/// The blocks an asking leaves clean that have no bit yet are the ones it
/// reports written: every other clean block was left clean by an asking
/// before it, which did this.
fn publish(_noted: &Journal, written: &[usize], start: usize, end: usize, holes: bool) {
    let mut g = guards();
    let g = &mut *g;
    for &page in written {
        let block = page * PAGE / BLOCK;
        // (Not one that a write has opened since the guards were asked,
        // nor one that could not be made read-only.)
        if g.clean.contains(&block) {
            PUBLISHED.set(block);
        }
    }
    if !holes {
        return;
    }
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // The notes of the range not looked at yet. Both sets are read in
    // block order, and every block of `holes` has a note.
    let fresh: Vec<usize> = {
        let mut looked = g.holes.range(first..last).copied().peekable();
        g.verified
            .range(first..last)
            .map(|(&block, _)| block)
            .filter(|&block| {
                while looked.next_if(|&hole| hole < block).is_some() {}
                looked.next_if_eq(&block).is_none()
            })
            .collect()
    };
    for block in fresh {
        g.holes.insert(block);
        // (Asked now, not when the note was made: a view the runtime is
        // mapping there, and has not told of yet, is no placeholder.)
        if g.absent.contains(&block) && placeholder(block) {
            PUBLISHED.set(block);
        }
    }
}

/// Whether every block of `[start, end)` has its bit ([`PUBLISHED`]): what
/// [`writes`] would answer of the range is in the journal. `_held`: the
/// journal's lock, which the caller holds from before this look until it
/// has read the journal (no bit is set meanwhile: [`publish`]).
fn published(_held: &Journal, start: usize, end: usize) -> bool {
    PUBLISHED.all(start / BLOCK, end.div_ceil(BLOCK))
}

/// Whether a block noted uncommitted stays noted until the runtime tells
/// of it (`VICTORY_ABSENT_ONCE=1` with `VICTORY_ABSENT_MS=told`): then, and
/// only then, can such a block be known by its bit ([`publish`]). With
/// notes that grow old, or none, every asking of a range that holds a hole
/// goes to the guards, which put the question to Windows as they do.
fn told() -> bool {
    absent_mode() == 1 && absent_keep_ms() == u64::MAX
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// The kernel's question about a process's memory, `(process, address,
    /// class of the answer, answer out, its room, bytes answered out)`: an
    /// NTSTATUS, negative when it cannot answer. It changes nothing.
    fn NtQueryVirtualMemory(
        process: *mut std::ffi::c_void,
        at: *const std::ffi::c_void,
        class: u32,
        out: *mut std::ffi::c_void,
        len: usize,
        returned: *mut usize,
    ) -> i32;
}

/// Whether `block` lies in a placeholder reservation: memory in which pages
/// come to be only by mapping a view, or making an allocation, in its
/// place, which the runtime does and tells of ([`title_memory_remapped`],
/// [`title_memory_committed`]). An ordinary reservation can be committed
/// by anybody, and free memory taken by anybody, with no word to us:
/// `VirtualQuery` does not tell the three apart, this question does.
fn placeholder(block: usize) -> bool {
    /// `MEMORY_REGION_INFORMATION` (ntddk); only the kinds are read.
    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code)]
    struct Region {
        allocation_base: usize,
        allocation_protect: u32,
        region_type: u32,
        region_size: usize,
        commit_size: usize,
        partition_id: usize,
        node_preference: usize,
    }
    // (`MemoryRegionInformationEx`; class 3 answers with the kind of memory
    // in that word, not the flags.)
    const MEMORY_REGION_INFORMATION_EX: u32 = 7;
    const PLACEHOLDER_RESERVATION: u32 = 1 << 8;
    let mut region = Region::default();
    // SAFETY: the answer's room and its size; the kernel checks the address.
    let status = unsafe {
        NtQueryVirtualMemory(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            (block * BLOCK) as *const _,
            MEMORY_REGION_INFORMATION_EX,
            (&raw mut region).cast(),
            size_of::<Region>(),
            std::ptr::null_mut(),
        )
    };
    status >= 0 && region.region_type & PLACEHOLDER_RESERVATION != 0
}

/// The note that `block` is uncommitted is gone, or is true no longer: the
/// block is not known by its bit any more ([`publish`]).
fn forget_hole(g: &mut Guards, block: usize) {
    if g.holes.remove(&block) {
        PUBLISHED.clear(block);
    }
}

/// How long a note kept for good lives for a hole that lies in no
/// placeholder reservation ([`note_life`]), in milliseconds.
const UNSURE_MS: u64 = 100;

/// How long the note that `block` is uncommitted lives, of `keep`
/// ([`absent_once`]). With the light refresh a note kept for good is kept
/// so only where pages cannot come to be without the runtime's word: in a
/// placeholder reservation, whose holes are known by their bits. A hole
/// that [`publish`] looked at and found in an ordinary reservation or in
/// free memory, which anybody can commit or take with no word to us, is
/// asked of Windows again when its note is [`UNSURE_MS`] old. (Without the
/// light refresh no hole is looked at, and `keep` stands.)
fn note_life(g: &Guards, block: usize, keep: u64) -> u64 {
    if keep == u64::MAX && g.holes.contains(&block) && !PUBLISHED.all(block, block + 1) {
        UNSURE_MS
    } else {
        keep
    }
}

/// Holes asked of Windows again at each submission ([`look_at_holes`]): a
/// question is a microsecond or so, and the submitting thread is the
/// frame's limit.
const HOLES_A_LOOK: usize = 16;

/// With the light refresh: `most` of the holes known by their bits are
/// asked of Windows again, the next ones each time and round again, in
/// case one was filled with nobody telling ([`title_memory_remapped`],
/// [`title_memory_committed`]), which this title is not known to do. One
/// that was loses its bit and its note, so that the next asking of it goes
/// to the guards, which find it; counted, and said once. One whose
/// reservation is a placeholder no longer (let go, or made anew) loses its
/// bit and keeps its note.
///
/// Not while the runtime changes a mapping: a view it has mapped and not
/// told of yet is no hole filled behind our back, and forgotten here it
/// could be made read-only before the runtime's word, which forgets the
/// guard and leaves the protection.
fn look_at_holes(most: usize) {
    static FROM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let Some(_still) = title_memory_reading_now() else {
        return;
    };
    let part: Vec<usize> = {
        let g = guards();
        if g.holes.is_empty() {
            return;
        }
        let from = FROM.load(std::sync::atomic::Ordering::Relaxed);
        let part: Vec<usize> = g.holes.range(from..).take(most).copied().collect();
        FROM.store(part.last().map_or(0, |last| last + 1), std::sync::atomic::Ordering::Relaxed);
        part
    };
    for block in part {
        // (Not the ones looked at that lie in no placeholder: they have no
        // bit, and their notes are the guards' to keep.)
        if !PUBLISHED.all(block, block + 1) {
            continue;
        }
        let filled = block >= uncommitted_end(block);
        if !filled && placeholder(block) {
            continue;
        }
        let mut g = guards();
        // (Told of since the part was taken: forgotten already.)
        if !g.holes.contains(&block) {
            continue;
        }
        PUBLISHED.clear(block);
        if filled {
            g.holes.remove(&block);
            g.verified.remove(&block);
            LIGHT_ASKS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                tracing::warn!(
                    address = format_args!("{:#x}", block * BLOCK),
                    "a hole of view memory was filled with nobody telling: host copies of it held old bytes until now (VICTORY_LIGHT_REFRESH; said once, counted with the light refresh's askings)"
                );
            });
        }
    }
}

/// Whether nothing of `[start, end)` was written that a copy does not
/// hold, known with nothing asked of the guards or the kernel: `copied` is
/// the copy's note, for each page from page `base` on, of the asking after
/// which it last took the page (0: never), as `crate::shadow` keeps it.
/// True only if [`writes`] would answer from the journal (the range was
/// asked ahead, or its blocks are known by their bits) and the answer would
/// have the copy take no page. `false` if something would have to be
/// asked, and always without the light refresh ([`light`]).
pub(crate) fn nothing_new(start: usize, end: usize, base: usize, copied: &[u64]) -> bool {
    light() && nothing_noted(start, end, base, copied)
}

/// [`nothing_new`], whatever the switch says.
fn nothing_noted(start: usize, end: usize, base: usize, copied: &[u64]) -> bool {
    if start >= end {
        return false;
    }
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let Some(taken) = first.checked_sub(base).and_then(|from| copied.get(from..last - base)) else {
        return false;
    };
    // A page never taken is taken at the next refresh (so is, at every
    // refresh, a page the copy's refreshes only ever hold in part).
    if taken.contains(&0) {
        return false;
    }
    let j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    // The bits first, then the journal, both under its lock ([`published`]).
    let by_bits = !j.ahead.iter().any(|&(a, b)| a <= first && last <= b);
    if by_bits && !published(&j, first * PAGE, last * PAGE) {
        return false;
    }
    let same = j.written.range(first..last).all(|(&page, &write)| write <= taken[page - first]);
    if same && by_bits {
        LIGHT_ASKS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    same
}

/// `VICTORY_ABSENT_ONCE`: 0 off, 1 on, 2 in turn ([`absent_once`]).
fn absent_mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_ABSENT_ONCE").as_deref() {
        Ok("1") => 1,
        Ok("alt") => 2,
        _ => 0,
    })
}

/// Whether a block of view memory that Windows said is uncommitted is not
/// asked about again for [`absent_keep_ms`] (`VICTORY_ABSENT_ONCE=1`; off
/// by default).
///
/// A block noted absent is never "clean", so a range that holds one is
/// walked block by block at every asking, and every such block is a
/// question to Windows (`uncommitted_end`): the title's unmapped pages are
/// placeholders of 64 KiB each, so one question answers for one block. The
/// profile of run 249 has 5.7 ms a frame of the submitting thread in
/// `VirtualQuery` under [`guard`], with the guards' lock held, which the
/// title's threads wait for in their write faults.
///
/// What the runtime maps there it tells ([`title_memory_remapped`]), and
/// that forgets the block and its note at once, with or without the
/// switch. What changes: memory that appears at such an address without
/// the runtime's word (a commit that is not a page mapping) is seen up to
/// [`absent_keep_ms`] later than before; until then its copy holds zeros,
/// as it did while nothing was there.
///
/// `VICTORY_ABSENT_ONCE=alt`: twenty seconds with, twenty without, in
/// turn, each change said in the log with the counts of the phase before.
fn absent_once() -> bool {
    match absent_mode() {
        0 => false,
        1 => true,
        _ => {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let on = (EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs() / 20) % 2 == 1;
            static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
            if LAST.swap(u8::from(on), std::sync::atomic::Ordering::Relaxed) != u8::from(on) {
                let [asked, kept] = absent_looks();
                tracing::info!(
                    "blocks of view memory found uncommitted are asked of Windows again only after {} ms from now (VICTORY_ABSENT_ONCE=alt): {}; since the change before: {asked} questions to Windows about such blocks, {kept} blocks answered from their note",
                    absent_keep_ms(),
                    if on { "ON" } else { "off, at every asking as usual" }
                );
            }
            on
        }
    }
}

/// How long a block found uncommitted is not asked about again
/// ([`absent_once`]): `VICTORY_ABSENT_MS`, 100 by default (about a frame),
/// 1 to 1000.
fn absent_keep_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| match std::env::var("VICTORY_ABSENT_MS").as_deref().map(str::trim) {
        // Until the runtime tells of the block ([`title_memory_remapped`],
        // [`title_memory_committed`]): no question to Windows at all for a
        // block noted uncommitted. Run 279 with 100 ms: 600,000 to 690,000
        // questions in twenty seconds without the notes, and still 357,000
        // to 604,000 with them, because a frame is as long as a note lives.
        Ok("told") => u64::MAX,
        Ok(v) => v.parse::<u64>().map_or(100, |n| n.clamp(1, 1000)),
        Err(_) => 100,
    })
}

/// Since they were last taken ([`absent_looks`]): questions to Windows
/// about blocks noted absent, and such blocks answered from their note
/// ([`absent_once`]). Counted with the switch off too.
static ABSENT_LOOKS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// [`ABSENT_LOOKS`], taken.
fn absent_looks() -> [u64; 2] {
    [0, 1].map(|i| ABSENT_LOOKS[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// Milliseconds since the guards' first asking: the clock of
/// [`absent_once`]'s notes.
fn guard_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    let keep = if absent_once() { absent_keep_ms() } else { 0 };
    let written = guard_at(start, end, if keep != 0 { guard_ms() } else { 0 }, keep);
    // Always on: the counts every 65,536 askings (in turn they are said
    // at each change).
    if absent_mode() == 1 {
        static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if (CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1) % 65_536 == 0 {
            let [asked, kept] = absent_looks();
            tracing::info!(
                "blocks of view memory found uncommitted are asked of Windows again only after {} ms (VICTORY_ABSENT_ONCE=1): in the last 65536 askings of the guards {asked} questions to Windows about such blocks, {kept} blocks answered from their note",
                absent_keep_ms()
            );
        }
    }
    written
}

/// [`guard`] at the moment `now` ([`guard_ms`]). `keep`: for how many
/// milliseconds a block Windows said is uncommitted is not asked about
/// again ([`absent_once`]); 0: it is asked about at every asking.
fn guard_at(start: usize, end: usize, now: u64, keep: u64) -> Option<Vec<usize>> {
    // A range whose first block the guards hold already (read-only by our
    // hand, or noted absent) is a view: no question to Windows. The
    // question walks the page tables to the end of the pages that share the
    // first one's state, and a guarded view is one long run of read-only
    // pages: 0.6 ms for the title's 174 MB heap, 16 times a frame, and most
    // of the 68 ms a frame of "guards" (run 58).
    // `VICTORY_GUARD_ASK=1` asks every time, as before.
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let known = !*ASK.get_or_init(|| std::env::var_os("VICTORY_GUARD_ASK").is_some_and(|v| v == "1")) && {
        let g = guards();
        let first = start / BLOCK;
        g.protected.contains(&first) || g.absent.contains(&first) || (view_known() && g.viewed.contains(&first))
    };
    if !known && !is_view(start, end) {
        return None;
    }
    install();
    let mut g = guards();
    let mut written = Vec::new();
    let mut run: Option<usize> = None;
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // The range's clean blocks, in order: read off the set in one pass,
    // not looked up one block at a time. A frame asks about some 10 GB of
    // view memory, 150,000 blocks, nearly all clean: the lookups were most
    // of the 60-90 ms a frame of "guards" (run 54). What the loop adds to
    // the set meanwhile is the block it stands at or ones before it.
    let clean: Vec<usize> = g.clean.range(first..last).copied().collect();
    if clean.len() == last - first {
        // Nothing written, mapped or new since the range was last taken.
        return Some(written);
    }
    let mut next_clean = 0;
    let mut uncommitted_until = first;
    for block in first..=last {
        while next_clean < clean.len() && clean[next_clean] < block {
            next_clean += 1;
        }
        let is_clean = next_clean < clean.len() && clean[next_clean] == block;
        let mut open = block < last && !is_clean;
        if open && g.absent.contains(&block) {
            // Found uncommitted a moment ago ([`absent_once`]): Windows is
            // not asked again yet. What the runtime maps there it tells,
            // and that takes the block out of `absent` and its note away.
            // (With the light refresh, kept for good only in a placeholder
            // reservation: [`note_life`].)
            let life = note_life(&g, block, keep);
            let noted = life != 0 && g.verified.get(&block).is_some_and(|&at| now < at.saturating_add(life));
            if noted {
                ABSENT_LOOKS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                open = false;
            } else {
                // Ordinary commits need no runtime mapping notification. Reuse
                // this asking's query for the following blocks in the same region.
                if block >= uncommitted_until {
                    ABSENT_LOOKS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    uncommitted_until = uncommitted_end(block);
                }
                open = block >= uncommitted_until;
                if open {
                    g.absent.remove(&block);
                    g.verified.remove(&block);
                    forget_hole(&mut g, block);
                } else if keep != 0 {
                    g.verified.insert(block, now);
                }
            }
        }
        if open {
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            if g.protected.contains(&block) {
                // Written through the runtime, still read-only.
                g.clean.insert(block);
            } else {
                run.get_or_insert(block);
                continue;
            }
        }
        if let Some(from) = run.take() {
            protect(&mut g, from, block);
        }
    }
    Some(written)
}

/// Whether `[start, end)` is view memory. The title's views lie in
/// placeholders, mapped and unmapped block by block as it streams, so a
/// range can begin in a block that is not mapped right now: the first
/// pages of the range that are there decide. A range with nothing there
/// counts as a view (its blocks are noted absent, and looked at again when
/// the runtime maps them).
///
/// Looking at the first address only took such a range for "not a view":
/// its copy was marked not watched for good and copied whole at every
/// refresh, around its unmapped pages (run 54: 300 such ranges, 570 MB
/// copied a frame for them, 255 copies a frame reading around holes).
/// `VICTORY_GUARD_HOLES=0` looks at the first address only, as before.
fn is_view(start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    static HOLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let holes = *HOLES.get_or_init(|| std::env::var_os("VICTORY_GUARD_HOLES").is_none_or(|v| v != "0"));
    let mut at = start;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        if info.Type == MEM_MAPPED {
            return true;
        }
        // Committed memory that is no view (the title's heaps, ours), or
        // the answer as it was before.
        if info.State == MEM_COMMIT || !holes {
            return false;
        }
        // A hole (a placeholder, or free): on to what follows it.
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at || next >= end {
            return true;
        }
        at = next;
    }
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail, and
/// noting those not committed (unmapped) absent. The same physical pages
/// at every other view of them are made read-only too: a write through
/// one of those is this memory written, and only a fault can say so.
/// `VICTORY_VIEW_GUARD=0` leaves the other views as they are, as before.
fn protect(g: &mut Guards, from: usize, to: usize) {
    protect_here(g, from, to);
    if !view_guard() {
        return;
    }
    for (base, bytes) in crate::inplace::views_between(from.saturating_mul(BLOCK), to.saturating_mul(BLOCK)) {
        let first = (base / BLOCK).max(from);
        let last = (base.saturating_add(bytes).div_ceil(BLOCK)).min(to);
        for block in first..last {
            let shown = crate::inplace::pages_of(block * BLOCK);
            // One view (nothing else to guard), or the page the title shows
            // wherever it has nothing (thousands of views): each view of
            // that one is guarded by itself.
            if shown.len() < 2 || shown.len() > MANY_VIEWS {
                continue;
            }
            #[cfg(feature = "diagnostics")]
            ALIASES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            for alias in shown {
                let alias = alias / BLOCK;
                g.aliased.insert(alias);
                if alias != block {
                    protect_alias(g, alias);
                }
            }
        }
    }
}

/// Views of one page past which its views are not guarded together.
pub(crate) const MANY_VIEWS: usize = 64;

/// Whether all the views of a guarded page are guarded together
/// (`VICTORY_VIEW_GUARD`).
fn view_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_GUARD").is_none_or(|v| v != "0"))
}

/// Diagnostics: blocks guarded together with their other views, blocks
/// opened because another view of theirs was written, and blocks opened
/// because a view of theirs was mapped; since the start.
#[cfg(feature = "diagnostics")]
pub(crate) static ALIASES: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Makes `block` read-only: another view of pages a copy has just taken
/// through its own view, so that a write through this one faults too. It is
/// not noted clean. The copies of title memory at this view have not taken
/// the pages: noted clean, the block would not be reported written at their
/// next check and they would keep the bytes from before the write (`guard`
/// reports a block that is read-only and not clean as written, then notes it
/// clean). A block that is clean stays so: had the pages been written since
/// its copies took them, `open` would have opened it.
fn protect_alias(g: &mut Guards, block: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    if g.protected.contains(&block) {
        return;
    }
    let mut old = 0;
    // SAFETY: title view memory; only its protection changes.
    if unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READONLY, &mut old) } != 0 {
        g.protected.insert(block);
        if view_known() {
            g.viewed.insert(block);
        }
    }
}

/// [`protect`] for one range, without the other views of its pages.
fn protect_here(g: &mut Guards, from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.extend(from..to);
        g.clean.extend(from..to);
        if view_known() {
            g.viewed.extend(from..to);
        }
    };
    if set(from, to) {
        note(g, from, to);
        return;
    }
    for block in from..to {
        if set(block, block + 1) {
            note(g, block, block + 1);
        } else {
            #[cfg(feature = "diagnostics")]
            let error = std::io::Error::last_os_error();
            if block < uncommitted_end(block) {
                g.absent.insert(block);
            } else {
                // Diagnostics: a committed block that will not be made
                // read-only is reported written at every asking and never
                // known to the guards (one in 1,024 said).
                #[cfg(feature = "diagnostics")]
                {
                    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 1024 == 0 {
                        tracing::info!("a block of title memory could not be made read-only (one in 1024): {:#x}: {error}", block * BLOCK);
                    }
                }
            }
        }
    }
}

/// The end (exclusive) of the whole uncommitted blocks starting at `block`.
fn uncommitted_end(block: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
        (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
    } else {
        block
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded. The same physical pages at
/// every other view of them are opened too: the write that faulted (or is
/// ours, `crate::host::copy_mapped`) goes through one view, and every
/// guard over the pages must copy them again.
fn open(g: &mut Guards, start: usize, end: usize) -> bool {
    // The light refresh: no view of these pages is known by its bit once
    // one of them can be written (who asks the bits takes no lock of ours;
    // after avatarmanz/VIctory-7900XTX 689df313).
    if light_kept() && start < end && !g.aliased.is_empty() {
        for &block in g.aliased.range(start / BLOCK..end.div_ceil(BLOCK)) {
            for alias in crate::inplace::pages_of(block * BLOCK) {
                PUBLISHED.clear(alias / BLOCK);
            }
        }
    }
    let any = open_here(g, start, end);
    if start >= end || g.aliased.is_empty() {
        return any;
    }
    let aliased: Vec<usize> = g.aliased.range(start / BLOCK..end.div_ceil(BLOCK)).copied().collect();
    for block in aliased {
        g.aliased.remove(&block);
        for alias in crate::inplace::pages_of(block * BLOCK) {
            if alias / BLOCK != block && open_here(g, alias, alias.saturating_add(BLOCK)) {
                #[cfg(feature = "diagnostics")]
                ALIASES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            g.aliased.remove(&(alias / BLOCK));
        }
    }
    any
}

/// [`open`] for one range, without the other views of its pages.
fn open_here(g: &mut Guards, start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let blocks: Vec<usize> = g
        .protected
        .range(start / BLOCK..end.div_ceil(BLOCK))
        .copied()
        .collect();
    for &block in &blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
        // Before the block can be written ([`PUBLISHED`]; nothing to do
        // where no bit was ever set).
        PUBLISHED.clear(block);
    }
    let give = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: blocks we made read-only.
        unsafe { VirtualProtect((from * BLOCK) as *const _, (to - from) * BLOCK, PAGE_READWRITE, &mut old) != 0 }
    };
    // Consecutive blocks with one call; one by one if they span views.
    let mut at = 0;
    while at < blocks.len() {
        let mut n = 1;
        while at + n < blocks.len() && blocks[at + n] == blocks[at] + n {
            n += 1;
        }
        if !give(blocks[at], blocks[at] + n) && n > 1 {
            for &block in &blocks[at..at + n] {
                give(block, block + 1);
            }
        }
        at += n;
    }
    !blocks.is_empty()
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // The length is the title's (`fread`'s size times count, saturated).
    open(&mut guards(), at, at.saturating_add(len));
    // A placed texture there changes with no descriptor written.
    crate::descriptor::host::remapped(at, len);
}

/// Whether what shaders wrote goes back into view memory beside the guards
/// (`VICTORY_WRITE_BESIDE=1`; off by default): see [`write_beside`].
pub(crate) fn beside_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WRITE_BESIDE").is_some_and(|v| v == "1"))
}

/// Writes of ours beside the guards, and those that went the old way.
pub(crate) static BESIDE: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Puts `len` bytes at `from` into title view memory at `at` (inside one
/// page) without opening its guard, if the block is guarded and the title
/// has not written it since a copy took it. `Some((before, now))`: written;
/// the page's last write was numbered `before` and this one is `now`.
///
/// [`writing`] opens the guard (a protection change), the next refresh finds
/// the block written, copies it into the copy the bytes came from, sends it
/// up to the card that wrote them, hashes it there and guards it again: in
/// gameplay 1,235 pages and 60 MB a frame, of 150 MB copied. Here the copy
/// that has the bytes takes `now` as its own, and every other copy of the
/// page (and of the other views of it) finds it written as before. A write
/// of the title's meanwhile faults, waits for the guards' lock and is seen
/// as ever.
pub(crate) fn write_beside(at: usize, from: *const u8, len: usize) -> Option<(u64, u64)> {
    use std::sync::atomic::Ordering::Relaxed;
    if len == 0 || at / PAGE != (at + len - 1) / PAGE {
        return None;
    }
    let page = at / PAGE;
    // In the order `writes` takes them.
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut g = guards();
    if !g.protected.contains(&page) || !g.clean.contains(&page) {
        BESIDE[1].fetch_add(1, Relaxed);
        return None;
    }
    if !crate::inplace::write_beside(at, from, len) {
        BESIDE[1].fetch_add(1, Relaxed);
        return None;
    }
    if g.aliased.contains(&page) {
        for alias in crate::inplace::pages_of(page * PAGE) {
            // Still read-only, not clean: reported written at its next look.
            if alias / BLOCK != page {
                g.clean.remove(&(alias / BLOCK));
            }
        }
    }
    let before_beside = j.written.get(&page).copied();
    j.asked += 1;
    let now = j.asked;
    // As in `wrote`: the asking that stands moves with it.
    if !j.ahead.is_empty() {
        j.ahead_asked = now;
    }
    if guard_once() && !j.ahead.is_empty() && g.aliased.contains(&page) {
        for alias in crate::inplace::pages_of(page * PAGE) {
            j.written.insert(alias / BLOCK, now);
        }
    }
    let before = before_beside.unwrap_or(0);
    j.written.insert(page, now);
    BESIDE[0].fetch_add(1, Relaxed);
    Some((before, now))
}

/// What `VICTORY_PAGE_WRITES` asks to be done with a write the guards do
/// not see once its bytes are there ([`title_memory_written`]), as bits:
/// 1, the guarded blocks under it are opened and count as written (host
/// copies of buffers and textures read through tables take them again);
/// 2, the placed textures picked by index that lie under it are due again;
/// 4, both also for reads to plain addresses when they have finished.
/// Unset or `guards`: bit 1 alone. `0`: nothing, counted only (the default
/// until run 199). `1`: bits 1 and 2. `slots`: bit 2 alone. `all`:
/// everything.
/// (Bit 1 by default: in every run 10,000 to 50,000 guarded blocks of host
/// copies lay under such writes and kept the bytes from before; runs 197
/// and 198 opened them, 9,766 and 10,796 blocks, with nothing worse for
/// it. Bit 2 makes texture slots due and is the one that flooded the
/// queue in an earlier form: still asked for by name.)
pub(crate) fn page_writes() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_PAGE_WRITES").as_deref().map(str::trim) {
        Ok("0") => 0,
        Ok("1") => 3,
        Ok("slots") => 2,
        Ok("all") => 7,
        _ => 1,
    })
}

/// Since the start: writes into physical pages through the runtime's own
/// views that landed at addresses the title has mapped (one for each run
/// of addresses); their bytes; guarded blocks that lay under them (host
/// copies that would otherwise keep the bytes from before); reads to
/// plain addresses that finished.
pub(crate) static WRITTEN: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The bytes of a write the guards do not see are there: title memory
/// `[at, at + len)`. `pages`: DirectStorage wrote them into physical pages
/// through a view of the runtime's own, and this is an address where the
/// title has those pages mapped (no title address faulted, nothing was
/// mapped or unmapped; the shortcut this file's header names). Otherwise a
/// read to a plain address has finished, of which [`writing`] was told
/// before it began.
///
/// Upstream's c75fdba does the same for its trackers (its `Writing` marks
/// the pages once more when dropped; page destinations are reported at
/// each address showing them). Here it is counted always and acted on as
/// [`page_writes`] says. Called without the runtime's page-table lock;
/// takes the guards' lock, then, alone, the lock of the ranges kept for
/// the texture pass.
pub fn title_memory_written(at: usize, len: usize, pages: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    if len == 0 {
        return;
    }
    let mode = page_writes();
    let end = at.saturating_add(len);
    if pages {
        WRITTEN[0].fetch_add(1, Relaxed);
        WRITTEN[1].fetch_add(len as u64, Relaxed);
    } else {
        WRITTEN[3].fetch_add(1, Relaxed);
        if mode & 4 == 0 {
            return;
        }
    }
    {
        let mut g = guards();
        let guarded = g.protected.range(at / BLOCK..end.div_ceil(BLOCK)).count() as u64;
        if pages {
            WRITTEN[2].fetch_add(guarded, Relaxed);
        }
        if mode & 1 != 0 && guarded != 0 {
            open(&mut g, at, end);
        }
    }
    // A placed texture there changed with no descriptor written.
    crate::descriptor::host::page_written(at, len);
}

/// The runtime mapped a view of the physical bytes `[offset, offset + len)`
/// of `section` (`crate::inplace::title_memory_view`): the view is
/// writable, so the same bytes guarded at another view must be checked
/// again — and are, the pages noted written, which also has the next check
/// guard the new view (`protect`).
pub(crate) fn views_mapped(section: usize, offset: u64, len: usize) {
    if !view_guard() {
        return;
    }
    let ranges = crate::inplace::views_of_section(section, offset, len);
    if ranges.is_empty() {
        return;
    }
    let mut g = guards();
    // The light refresh: all of them told as written before the first can
    // be written (as in [`open`]).
    if light_kept() {
        for &(at, n) in &ranges {
            for &block in g.protected.range(at / BLOCK..at.saturating_add(n).div_ceil(BLOCK)) {
                PUBLISHED.clear(block);
            }
        }
    }
    for &(at, n) in &ranges {
        if open_here(&mut g, at, at.saturating_add(n)) {
            #[cfg(feature = "diagnostics")]
            ALIASES[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Held shared by a read of title memory in place, and alone by the runtime
/// while it changes what title memory shows.
///
/// The runtime does not change a mapping in one step. To unmap one page of
/// a view it unmaps the whole view and maps what stays again
/// (`xmem.rs` `unmap`), and a view it maps is a reservation until the next
/// call commits it (`map`, then `commit`). For that moment pages the title
/// never gave up are "reserved, never committed", and a thread reading
/// them in place takes an access violation: run 71 in `Layout::detile`,
/// run 74 in the hash before it, both under `Staging::refresh_now`, which
/// had asked whether the texture was all mapped and been told yes. Asking
/// cannot help; not changing the mapping while such a read is under way
/// does.
static MAPPING: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// What the runtime holds while it changes what title memory shows
/// ([`title_memory_remapping`]).
pub struct Remapping(#[allow(dead_code)] std::sync::RwLockWriteGuard<'static, ()>, std::time::Instant);

impl Drop for Remapping {
    fn drop(&mut self) {
        // From asking for the lock until it is let go (the frame log).
        crate::framelog::add(crate::framelog::T::RemapUs, self.1.elapsed().as_micros() as u64);
    }
}

/// The runtime is about to map, unmap or move physical pages of title
/// memory: until what it returns is dropped, no read of title memory in
/// place is under way and none begins ([`title_memory_reading`]). Taken
/// around the whole change, the unmapping and the mapping again.
pub fn title_memory_remapping() -> Remapping {
    let asked = std::time::Instant::now();
    let held = MAPPING.write().unwrap_or_else(|e| e.into_inner());
    crate::framelog::add(crate::framelog::T::Remaps, 1);
    crate::framelog::add(crate::framelog::T::RemapWaitUs, asked.elapsed().as_micros() as u64);
    Remapping(held, asked)
}

/// What a read of title memory in place holds ([`title_memory_reading`]).
pub struct Reading(#[allow(dead_code)] std::sync::RwLockReadGuard<'static, ()>);

/// For reading title memory in place: until what it returns is dropped,
/// the runtime changes no mapping, so memory found mapped stays mapped.
/// Not to be taken twice by one thread, nor held across anything that
/// maps title memory.
pub fn title_memory_reading() -> Reading {
    Reading(MAPPING.read().unwrap_or_else(|e| e.into_inner()))
}

/// [`title_memory_reading`] if it can be had at once: `None` while the
/// runtime changes a mapping (or waits to), for a caller that has another
/// way to read and would rather not wait.
pub fn title_memory_reading_now() -> Option<Reading> {
    match MAPPING.try_read() {
        Ok(guard) => Some(Reading(guard)),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(Reading(e.into_inner())),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The runtime committed title memory `[at, at + len)` for the title
/// (`XMemVirtualAlloc` with a commit): what [`absent_once`] noted about its
/// blocks is forgotten, so the next asking puts the question to Windows
/// again, as every asking did before the notes. With `VICTORY_ABSENT_MS=told`
/// this and [`title_memory_remapped`] are the only ways a block noted
/// uncommitted is asked about again.
pub fn title_memory_committed(at: usize, len: usize) {
    if len == 0 || absent_mode() == 0 {
        return;
    }
    let mut g = guards();
    if g.verified.is_empty() {
        return;
    }
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    let noted: Vec<usize> = g.verified.range(first..last).map(|(&block, _)| block).collect();
    for block in noted {
        g.verified.remove(&block);
        forget_hole(&mut g, block);
    }
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded (nor known
/// unmapped).
pub fn title_memory_remapped(at: usize, len: usize) {
    // Stored pages followed there end (`VICTORY_BACK_READS`): counted only.
    #[cfg(feature = "diagnostics")]
    crate::backreads::remapped(at, len);
    crate::texback::remapped(at, len);
    // A placed texture there arrives, goes or changes with no descriptor
    // written.
    crate::descriptor::host::remapped(at, len);
    // The views the runtime told us of there are no longer what it shows.
    crate::inplace::views_gone(at, len);
    // And nothing stays in place over it.
    crate::inplace::title_memory_freeing(at, len);
    let mut g = guards();
    let range = at / BLOCK..at.saturating_add(len).div_ceil(BLOCK);
    let absent: Vec<usize> = g.absent.range(range.clone()).copied().collect();
    for block in absent {
        g.absent.remove(&block);
        g.verified.remove(&block);
        forget_hole(&mut g, block);
    }
    let blocks: Vec<usize> = g.protected.range(range).copied().collect();
    for block in blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
        // What it shows is new ([`PUBLISHED`]).
        PUBLISHED.clear(block);
    }
    let aliased: Vec<usize> = g.aliased.range(at / BLOCK..at.saturating_add(len).div_ceil(BLOCK)).copied().collect();
    for block in aliased {
        g.aliased.remove(&block);
    }
    if view_known() {
        // The runtime calls this for two things only (`xmem`: `map` and
        // `unmap`): a range it has just mapped a section's pages into,
        // and a view it is about to make a placeholder again. Either way
        // what stands there is view memory or a hole in it, which is what
        // `is_view` would answer for a range that begins there; and the
        // write watch's asking can only fail there. The title remaps some
        // 83 times a frame, and each time the blocks were forgotten, so
        // that the next asking about each range went to the kernel twice
        // (run 235: 266 to 565 failed askings a frame with the blocks
        // remembered from their guards alone, no fewer than without).
        g.viewed.extend(at / BLOCK..at.saturating_add(len).div_ceil(BLOCK));
        return;
    }
    // What the range shows is new: no longer known to be a view.
    let viewed: Vec<usize> = g.viewed.range(at / BLOCK..at.saturating_add(len).div_ceil(BLOCK)).copied().collect();
    for block in viewed {
        g.viewed.remove(&block);
    }
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        // Held across both checks: another thread may have opened the block
        // since this one faulted, and a copy may have guarded it again since
        // (then it is ours again, not read-only for some other reason).
        let mut g = guards();
        if open(&mut g, at, at + 1) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            crate::framelog::add(crate::framelog::T::GuardFaults, 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if writable(at) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // The switch is read here, not first in the handler ([`open`] asks
    // it): reading the environment allocates.
    light_kept();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (_, before) = writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// With the guards asked too, a submission's one asking answers for a
    /// view: what was written before it, not what is written after it, and
    /// the next asking has that.
    #[test]
    fn one_asking_answers_for_guarded_views() {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        // SAFETY: a pagefile section of four blocks, mapped read-write (and
        // left so).
        let view = unsafe {
            let section =
                CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, (4 * BLOCK) as u32, std::ptr::null());
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 4 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        let (start, end) = (view, view + 4 * BLOCK);
        let first = view / PAGE;
        // Taken once: all of it, and guarded.
        let (taken, all) = writes(start, end, true).expect("a view");
        assert_eq!(all.iter().map(|w| w.0).collect::<Vec<_>>(), (first..first + 4).collect::<Vec<_>>());
        let newer = |pages: &[(usize, u64)], than: u64| -> Vec<usize> { pages.iter().filter(|w| w.1 > than).map(|w| w.0 - first).collect() };
        // SAFETY (each): our view; the guard's handler gives the block back.
        let write = |block: usize| unsafe { ((view + block * BLOCK + 8) as *mut u8).write_volatile(block as u8 + 1) };
        write(1);
        ask_once_of(&[(start, end)], true);
        let (now, pages) = asked(start, end).expect("asked of the guards");
        assert_eq!(newer(&pages, taken), [1]);
        // Written after the asking: not in it, whoever looks.
        write(2);
        let (again, pages) = writes(start, end, true).expect("a view");
        assert_eq!((again, newer(&pages, taken)), (now, vec![1]));
        // The next one has it, and only it.
        ask_once_of(&[(start, end)], true);
        let (_, pages) = asked(start, end).expect("asked of the guards");
        assert_eq!(newer(&pages, now), [2]);
        ask_once_end();
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
        assert_eq!(landed, b"from the disk");
        assert_eq!(
            guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
        title_memory_remapped(view, 2 * BLOCK);
    }

    #[test]
    fn writes_in_partly_committed_view_blocks_are_reported() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            SEC_RESERVE, UnmapViewOfFile,
        };

        const SIZE: usize = 3 * BLOCK;
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                title_memory_remapped(self.0.Value as usize, SIZE);
                // SAFETY: our view is still mapped.
                unsafe { UnmapViewOfFile(self.0) };
            }
        }
        // SAFETY: a new reserved pagefile section. The view holds its own
        // reference after we close the handle, including on assertion failure.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            View(view)
        };
        let start = view.0.Value as usize;
        let commit = |at: usize, len| {
            // SAFETY: these pages belong to our reserved view.
            let committed = unsafe { VirtualAlloc(at as _, len, MEM_COMMIT, PAGE_READWRITE) };
            assert_eq!(committed as usize, at);
        };
        let early = start + BLOCK + 3 * KERNEL_PAGE;
        let late = start + 2 * BLOCK + 3 * KERNEL_PAGE;
        commit(start, BLOCK);
        commit(early, KERNEL_PAGE);
        let refresh = || guard(start, start + SIZE).unwrap();
        assert_eq!(refresh(), [start / PAGE, early / PAGE, late / PAGE]);
        // The last block is still entirely reserved.
        assert!(!refresh().contains(&(late / PAGE)));

        commit(late, KERNEL_PAGE); // Ordinary commit, without a runtime mapping notification.
        for at in [early, late] {
            assert!(writable(at));
            // SAFETY: each address is inside its committed read-write page.
            unsafe { (at as *mut u8).write_volatile(1) };
        }
        assert_eq!(refresh(), [early / PAGE, late / PAGE]);
    }

    /// A range of view memory whose first block is not mapped is a view
    /// all the same: its blocks are reported once, then the ones written
    /// or mapped since.
    #[test]
    fn a_range_that_begins_in_an_unmapped_block_is_guarded() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of three; the middle block is mapped first.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        // Nothing mapped at all: a view with every block absent.
        assert!(is_view(p, p + 3 * BLOCK));
        let middle = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        // The range begins in the placeholder, its middle block is a view.
        assert!(is_view(p, p + 3 * BLOCK));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()), "nothing written since");
        // SAFETY: inside the mapped middle block.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        let first = map(0);
        title_memory_remapped(p, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // Memory of ours that is no view stays none.
        let mut local = [0u8; 16];
        let at = local.as_mut_ptr() as usize;
        assert!(!is_view(at, at + local.len()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// Blocks of a placeholder between views are reported once, then not
    /// again (they cannot be guarded) until the runtime maps them.
    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of two; block 0 shows the section's first block, as the
        // runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// With a note kept ([`absent_once`]): a block Windows said is
    /// uncommitted is not asked about again until the note is old, so what
    /// appears there without a word is seen that much later; what the
    /// runtime tells of is seen at once; and with no note kept (`keep` 0)
    /// everything is seen at the next asking, as before.
    #[test]
    fn a_block_noted_uncommitted_is_asked_about_again_only_when_its_note_is_old() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of four blocks and a pagefile
        // section of four, as the runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                4 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (4 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let (from, to) = (p, p + 4 * BLOCK);
        const KEEP: u64 = 100;
        let first = map(0);
        let all: Vec<usize> = (0..4).flat_map(pages).collect();
        assert_eq!(guard_at(from, to, 1_000, KEEP), Some(all), "the first asking takes every block");
        assert_eq!(guard_at(from, to, 1_000, KEEP), Some(Vec::new()), "blocks 1 to 3 are noted uncommitted");
        // Mapped without a word to the guards.
        let second = map(1);
        assert_eq!(guard_at(from, to, 1_050, KEEP), Some(Vec::new()), "the note is 50 ms old: Windows is not asked");
        assert_eq!(guard_at(from, to, 1_100, KEEP), Some(pages(1).collect()), "the note is old: the block is found");
        assert_eq!(guard_at(from, to, 1_100, KEEP), Some(Vec::new()));
        // Mapped and told, as the runtime does: seen at once, note or not.
        let third = map(2);
        title_memory_remapped(p + 2 * BLOCK, BLOCK);
        assert_eq!(guard_at(from, to, 1_101, KEEP), Some(pages(2).collect()));
        // No note kept: a block mapped without a word is found at the next
        // asking, though its note is one millisecond old.
        let fourth = map(3);
        assert_eq!(guard_at(from, to, 1_101, 0), Some(pages(3).collect()));
        assert_eq!(guard_at(from, to, 1_101, KEEP), Some(Vec::new()));
        // SAFETY: our views and placeholder.
        unsafe {
            for view in [first, second, third, fourth] {
                UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER);
            }
            for block in 0..4 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 4 * BLOCK);
    }

    /// A pagefile section mapped twice: two views of the same physical
    /// pages, the second left for the test; the section handle, the two
    /// addresses.
    fn two_views(bytes: usize) -> (usize, usize, usize) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
        };
        // SAFETY: a fresh pagefile section, mapped twice; the views keep
        // the section alive and the handle is closed by the caller.
        unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                bytes as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            assert_ne!(one, 0);
            assert_ne!(two, 0);
            crate::inplace::title_memory_view(one, bytes, section as usize, 0);
            crate::inplace::title_memory_view(two, bytes, section as usize, 0);
            (section as usize, one, two)
        }
    }

    /// The section of [`two_views`] is closed and its views unmapped; the
    /// guard is told they are gone first.
    fn drop_views(section: usize, views: &[usize], bytes: usize) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        for &view in views {
            title_memory_remapped(view, bytes);
            // SAFETY: a view of the section mapped by `two_views`.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                })
            };
        }
        // SAFETY: the section's last reference.
        unsafe { CloseHandle(section as *mut std::ffi::c_void) };
    }

    /// The title wrote the pages of a guarded texture through another view
    /// of them: the fault at that view is the guard's to see, and the
    /// guarded view's next check reports the pages written.
    #[test]
    fn a_write_through_another_view_is_reported_at_the_guarded_one() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        // The first check takes it in (and every view of its pages).
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        // Writing through the other view faults into the handler, which
        // opens that view and the guarded one: the next check sees it.
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// A view of guarded pages is mapped anew: it is writable, so the
    /// guarded view checks its pages again (and guards the new view).
    #[test]
    fn a_view_mapped_beside_a_guarded_one_makes_it_check_again() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        title_memory_remapped(other, SIZE);
        crate::inplace::title_memory_view(other, SIZE, section, 0);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// A write of the runtime's own, told at one view of the pages
    /// (`writing`): the other views' next checks report the pages written.
    #[test]
    fn a_write_told_at_one_view_is_reported_at_the_others() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        writing(other + BLOCK, BLOCK);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        // Half a page and the next: both blocks, not just the first's.
        writing(other + BLOCK / 2, BLOCK);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// Both views of the pages are checked, each for a copy of its own
    /// (the title shows one physical page at several addresses, and each
    /// address has its host copy): a write through either is reported at
    /// both, whichever is checked first. Guarding the pages for the first
    /// view's check must not note the second view clean, or its copy would
    /// keep the bytes from before the write.
    #[test]
    fn a_write_is_reported_at_every_checked_view() {
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0), pages(one, 1)]));
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0), pages(two, 1)]), "the second view's first check");
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()), "nothing written yet");
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()), "nothing written yet");
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((two + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 1)]));
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 1)]), "written through this view, checked second");
        // SAFETY: the first view, as above.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0)]));
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0)]), "written through this view, checked second");
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()));
        drop_views(section, &[one, two], SIZE);
    }

    /// A page shown in more than `MANY_VIEWS` views is guarded at each by
    /// itself: the others stay writable, and nothing looks through them all.
    #[test]
    fn a_page_of_many_views_is_guarded_one_view_at_a_time() {
        let (section, one, two) = two_views(BLOCK);
        let filler = one + (1 << 45);
        for n in 0..MANY_VIEWS {
            crate::inplace::title_memory_view(filler + n * BLOCK, BLOCK, section, 0);
        }
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        assert!(writable(two), "another view of it is left alone");
        // SAFETY: the guarded view, opened by the handler.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        title_memory_remapped(filler, MANY_VIEWS * BLOCK);
        drop_views(section, &[one, two], BLOCK);
    }

    /// A write or mapping that runs past the end of the address space (a
    /// title's `fread` with a huge count) covers up to the end; the sum used
    /// to overflow. Up there no test has guarded blocks.
    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
    }

    /// The tests of the light refresh take turns: they read its counts,
    /// which no other test moves (the switch is not set in tests, so
    /// [`writes`] itself keeps no bits and asks none).
    fn light_turn() -> std::sync::MutexGuard<'static, ()> {
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TURN.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Askings answered by the bits, and askings that went to the guards.
    fn light_counts() -> [u64; 2] {
        [0, 1].map(|i| LIGHT_ASKS[i].load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Whether `[start, end)` is known by its bits.
    fn known(start: usize, end: usize) -> bool {
        published(&JOURNAL.lock().unwrap_or_else(|e| e.into_inner()), start, end)
    }

    /// The pages of an answer of [`writes`] written after the asking `since`.
    fn since(since: u64, written: Vec<(usize, u64)>) -> Vec<usize> {
        written.into_iter().filter(|w| w.1 > since).map(|w| w.0).collect()
    }

    /// The asking that makes blocks clean gives them their bits; the next
    /// one is answered by them, and the guards are not asked. A write takes
    /// the written block's bit, through the guarded view or through another
    /// view of the same pages, and the next asking reports it.
    #[test]
    fn a_range_is_known_by_its_bits_until_a_write() {
        let _turn = light_turn();
        const SIZE: usize = 3 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let pages = |block: usize| (one + block * BLOCK) / PAGE;
        assert!(!known(one, one + SIZE), "nothing asked yet");
        // The first asking takes every block in, read-only and clean.
        let (first, all) = writes_as(one, one + SIZE, true, 2).expect("a view");
        assert_eq!(all, [(pages(0), first), (pages(1), first), (pages(2), first)]);
        assert!(known(one, one + SIZE), "made clean and noted");
        // The second is answered by the bits.
        let before = light_counts();
        let (second, written) = writes_as(one, one + SIZE, true, 2).expect("a view");
        assert!(second > first);
        assert!(since(first, written).is_empty(), "nothing written");
        assert_eq!(light_counts(), [before[0] + 1, before[1]], "the guards were not asked");
        // A write to the guarded view faults: the handler takes the bit.
        // SAFETY: the first view, read-write once the handler has opened it.
        unsafe { ((one + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert!(!known(one, one + SIZE));
        assert!(known(one, one + BLOCK) && known(one + 2 * BLOCK, one + SIZE), "the blocks beside it keep theirs");
        let before = light_counts();
        let (third, written) = writes_as(one, one + SIZE, true, 2).expect("a view");
        assert_eq!(since(second, written), [pages(1)]);
        assert_eq!(light_counts(), [before[0], before[1] + 1], "the guards were asked");
        assert!(known(one, one + SIZE), "guarded again and noted");
        // A write through the other view of the same pages takes it too.
        // SAFETY: the second view, as above.
        unsafe { ((two + 2 * BLOCK + 8) as *mut u8).write_volatile(1) };
        assert!(!known(one + 2 * BLOCK, one + SIZE), "written through another view");
        assert!(known(one, one + 2 * BLOCK));
        let (fourth, written) = writes_as(one, one + SIZE, true, 2).expect("a view");
        assert_eq!(since(third, written), [pages(2)]);
        // A write of ours that is told of (`writing`) does as well.
        writing(one + 8, 8);
        assert!(!known(one, one + BLOCK));
        let (_, written) = writes_as(one, one + SIZE, true, 2).expect("a view");
        assert_eq!(since(fourth, written), [pages(0)]);
        assert!(known(one, one + SIZE));
        // What the runtime maps anew is not known.
        title_memory_remapped(one + BLOCK, BLOCK);
        assert!(!known(one + BLOCK, one + 2 * BLOCK));
        drop_views(section, &[one, two], SIZE);
    }

    /// `VICTORY_LIGHT_REFRESH=alt`: in the phase without, the bits are not
    /// asked, and they are kept: what is written in that phase, asked about
    /// in it or not, is reported by the first asking of the phase with.
    /// With the switch off no bit is ever set.
    #[test]
    fn bits_kept_through_a_phase_that_does_not_ask_them_are_true_in_the_next() {
        let _turn = light_turn();
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let pages = |block: usize| (one + block * BLOCK) / PAGE;
        // With: taken in, then known by its bits.
        let (first, _) = writes_as(one, one + SIZE, true, 2).expect("a view");
        assert!(known(one, one + SIZE));
        // Without: the bits are there and are not asked.
        let before = light_counts();
        let (second, written) = writes_as(one, one + SIZE, true, 1).expect("a view");
        assert!(since(first, written).is_empty());
        assert_eq!(light_counts(), [before[0], before[1] + 1], "without: the guards answer");
        // Written in the phase without: the fault takes the bit all the
        // same, and that phase's asking gives it back.
        // SAFETY: the first view, read-write once the handler has opened it.
        unsafe { ((one + 8) as *mut u8).write_volatile(1) };
        assert!(!known(one, one + BLOCK));
        let (third, written) = writes_as(one, one + SIZE, true, 1).expect("a view");
        assert_eq!(since(second, written), [pages(0)]);
        assert!(known(one, one + SIZE));
        // Written again in the phase without, through the other view, and
        // not asked about until the phase with: its first asking goes to
        // the guards and reports it.
        // SAFETY: the second view, as above.
        unsafe { ((two + BLOCK + 8) as *mut u8).write_volatile(1) };
        let before = light_counts();
        let (fourth, written) = writes_as(one, one + SIZE, true, 2).expect("a view");
        assert_eq!(since(third, written), [pages(1)], "a bit from before the phase without was trusted");
        assert_eq!(light_counts(), [before[0], before[1] + 1]);
        let before = light_counts();
        let (fifth, written) = writes_as(one, one + SIZE, true, 2).expect("a view");
        assert!(since(fourth, written).is_empty());
        assert_eq!(light_counts(), [before[0] + 1, before[1]]);
        // The switch off: a write still takes the bit, the asking sets none
        // and counts nothing.
        // SAFETY: the first view, as above.
        unsafe { ((one + 8) as *mut u8).write_volatile(2) };
        let before = light_counts();
        let (_, written) = writes_as(one, one + SIZE, true, 0).expect("a view");
        assert_eq!(since(fifth, written), [pages(0)]);
        assert_eq!(light_counts(), before);
        assert!(!known(one, one + BLOCK), "off: no bit is set");
        drop_views(section, &[one, two], SIZE);
    }

    /// A range with unmapped blocks is not known by its bits while its
    /// holes are asked about at every asking. Noted for good, a hole that
    /// lies in a placeholder is; then nothing is asked until the runtime
    /// tells of the block, or the look at the holes finds it filled. A
    /// hole in an ordinary reservation is never known by a bit, and its
    /// note grows old.
    #[test]
    fn holes_are_known_by_their_bits_only_when_noted_for_good() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let _turn = light_turn();
        // SAFETY: a fresh placeholder of four blocks and a pagefile
        // section of four, as the runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                4 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (4 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE;
        let (from, to) = (p, p + 4 * BLOCK);
        let first = map(0);
        assert!(placeholder(p / BLOCK + 1) && placeholder(p / BLOCK + 3), "what is left of the placeholder");
        assert!(!placeholder(p / BLOCK), "a view is none");
        // The first asking takes every block; blocks 1 to 3 are unmapped.
        let (one, all) = writes_as(from, to, true, 2).expect("a view");
        assert_eq!(since(one - 1, all), [pages(0), pages(1), pages(2), pages(3)]);
        assert!(known(p, p + BLOCK) && !known(from, to));
        // Their notes are not for good (no switch says so here): each
        // asking goes to the guards, which ask Windows.
        let before = light_counts();
        let (two, written) = writes_as(from, to, true, 2).expect("a view");
        assert!(since(one, written).is_empty());
        assert_eq!(light_counts(), [before[0], before[1] + 1]);
        assert!(!known(from, to), "a hole not noted for good has no bit");
        // Noted for good, as with `VICTORY_ABSENT_MS=told`, and looked at
        // as the asking after the notes does.
        assert_eq!(guard_at(from, to, 1, u64::MAX), Some(Vec::new()));
        publish(&JOURNAL.lock().unwrap_or_else(|e| e.into_inner()), &[], from, to, true);
        assert!(known(from, to), "holes in a placeholder, noted for good");
        let before = light_counts();
        let (three, written) = writes_as(from, to, true, 2).expect("a view");
        assert!(since(two, written).is_empty());
        assert_eq!(light_counts(), [before[0] + 1, before[1]], "answered by the bits, holes and all");
        // Mapped without a word: not seen, as with the notes alone ...
        let second = map(1);
        let (four, written) = writes_as(from, to, true, 2).expect("a view");
        assert!(since(three, written).is_empty(), "nothing is asked about a hole known by its bit");
        // ... until the look at the holes comes by (it goes round, and
        // waits its turn while another test changes a mapping).
        for _ in 0..2000 {
            if !known(p + BLOCK, p + 2 * BLOCK) {
                break;
            }
            look_at_holes(usize::MAX);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!known(p + BLOCK, p + 2 * BLOCK), "found filled");
        assert!(known(p + 2 * BLOCK, to), "the holes beside it are holes still");
        let (five, written) = writes_as(from, to, true, 2).expect("a view");
        assert_eq!(since(four, written), [pages(1)]);
        assert!(known(from, to));
        // Mapped and told, as the runtime does: the bit goes at once.
        let third = map(2);
        title_memory_remapped(p + 2 * BLOCK, BLOCK);
        assert!(!known(p + 2 * BLOCK, p + 3 * BLOCK));
        let (_, written) = writes_as(from, to, true, 2).expect("a view");
        assert_eq!(since(five, written), [pages(2)]);
        // A hole in an ordinary reservation, noted for good all the same:
        // anybody can commit it with no word to us, so it gets no bit.
        // SAFETY: a fresh reservation of one block, released below.
        let plain = unsafe { VirtualAlloc(std::ptr::null(), BLOCK, MEM_RESERVE, PAGE_NOACCESS) } as usize;
        assert_ne!(plain, 0);
        assert!(!placeholder(plain / BLOCK));
        assert_eq!(guard_at(plain, plain + BLOCK, 1, u64::MAX), Some(vec![plain / PAGE]));
        assert_eq!(guard_at(plain, plain + BLOCK, 2, u64::MAX), Some(Vec::new()), "noted");
        publish(&JOURNAL.lock().unwrap_or_else(|e| e.into_inner()), &[], plain, plain + BLOCK, true);
        assert!(!known(plain, plain + BLOCK));
        // Nor does its note live for good: committed with no word, it is
        // found when the note is old.
        // SAFETY: the reservation made above.
        let committed = unsafe { VirtualAlloc(plain as *const _, BLOCK, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(committed as usize, plain);
        assert_eq!(guard_at(plain, plain + BLOCK, 50, u64::MAX), Some(Vec::new()), "the note is 48 ms old");
        assert_eq!(guard_at(plain, plain + BLOCK, 2 + UNSURE_MS, u64::MAX), Some(vec![plain / PAGE]), "the note is old");
        title_memory_remapped(plain, BLOCK);
        // SAFETY: our views, placeholder and reservation.
        unsafe {
            VirtualFree(plain as *mut _, 0, MEM_RELEASE);
            for view in [first, second, third] {
                UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER);
            }
            for block in 0..4 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 4 * BLOCK);
    }

    /// A copy has nothing new to take of a range known by its bits when it
    /// took every page of it after the page's last noted write, and that is
    /// known with nothing asked.
    #[test]
    fn a_copy_that_holds_every_noted_write_has_nothing_new() {
        let _turn = light_turn();
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let base = one / PAGE;
        assert!(!nothing_noted(one, one + SIZE, base, &[1, 1]), "never asked about: something would have to be asked");
        let (first, _) = writes_as(one, one + SIZE, true, 2).expect("a view");
        let before = light_counts();
        assert!(nothing_noted(one, one + SIZE, base, &[first, first]), "both pages taken at the asking that found them");
        assert_eq!(light_counts(), [before[0] + 1, before[1]]);
        assert!(!nothing_noted(one, one + SIZE, base, &[first, first - 1]), "a page taken before its write was found");
        assert!(!nothing_noted(one, one + SIZE, base, &[first, 0]), "a page never taken");
        assert!(!nothing_noted(one, one + SIZE, base + 1, &[first, first]), "a range the copy's notes do not hold");
        assert!(!nothing_noted(one, one + SIZE, base, &[first]));
        assert!(!nothing_noted(one, one, base, &[first, first]), "no range");
        // Written: the block's bit is gone, and something must be asked.
        // SAFETY: the first view, read-write once the handler has opened it.
        unsafe { ((one + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert!(!nothing_noted(one, one + SIZE, base, &[first, first]));
        assert!(nothing_noted(one, one + BLOCK, base, &[first, first]), "the block beside it is as it was");
        let (second, _) = writes_as(one, one + SIZE, true, 2).expect("a view");
        assert!(!nothing_noted(one, one + SIZE, base, &[first, first]), "the journal has the write");
        assert!(nothing_noted(one, one + SIZE, base, &[first, second]));
        drop_views(section, &[one, two], SIZE);
    }

    /// A range's bits are all of its blocks', whatever lies beside them.
    #[test]
    fn bits_of_ranges() {
        static BITS: Bits = Bits::new();
        let blocks = [0usize, 1, 63, 64, 65, 127, 128, 200, 65535, 65536, 65537, 1 << 20, (1 << 31) - 1];
        for &b in &blocks {
            assert!(!BITS.all(b, b + 1));
            BITS.set(b);
            assert!(BITS.all(b, b + 1));
        }
        assert!(BITS.all(0, 2) && BITS.all(63, 66) && BITS.all(127, 129) && BITS.all(65535, 65538));
        assert!(!BITS.all(0, 3) && !BITS.all(62, 64) && !BITS.all(65534, 65536) && !BITS.all(5, 5));
        for b in 300..700 {
            BITS.set(b);
        }
        assert!(BITS.all(300, 700) && !BITS.all(299, 700) && !BITS.all(300, 701));
        BITS.clear(511);
        assert!(!BITS.all(300, 700) && BITS.all(300, 511) && BITS.all(512, 700));
        BITS.set(1 << 40);
        BITS.clear(1 << 40);
        assert!(!BITS.all(1 << 40, (1 << 40) + 1), "outside the address space");
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::{title_memory_reading, title_memory_remapping};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    /// A change of mapping waits for a read in place that is under way, and
    /// a read in place waits for a change of mapping.
    #[test]
    fn reads_in_place_and_remappings_wait_for_each_other() {
        // A reader holds; the change does not begin until it lets go.
        let changed = Arc::new(AtomicBool::new(false));
        let reading = title_memory_reading();
        let (began, wait) = mpsc::channel();
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _changing = title_memory_remapping();
                changed.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!changed.load(Ordering::SeqCst), "the mapping changed under a read in place");
        drop(reading);
        change.join().unwrap();
        assert!(changed.load(Ordering::SeqCst));
        // A change holds; the read does not begin until it is done.
        let read = Arc::new(AtomicBool::new(false));
        let changing = title_memory_remapping();
        let (began, wait) = mpsc::channel();
        let reader = {
            let read = read.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _reading = title_memory_reading();
                read.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!read.load(Ordering::SeqCst), "a read in place began during a change of mapping");
        drop(changing);
        reader.join().unwrap();
        assert!(read.load(Ordering::SeqCst));
    }
}
