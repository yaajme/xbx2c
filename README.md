# xbx2c (Xbox SeriesX to CoreWin): a faster fork of victory-fork (work in progress)

A fork of [fxceless/victory-fork](https://github.com/fxceless/victory-fork)
(merged up to `f85101b`), which is itself a fork of
[VIctory](https://github.com/Marvisak/VIctory). It adds frame-rate work and a
few fixes. The frame-rate switches are off by default. It is **not playable
yet**.

**This repository contains no game files, code or assets.** You need your own
copy of the game. Not affiliated with or endorsed by its developer or
publisher.

> **The newest commit itself has not reached the world.** Two launches of builds
> that differ from it in two small ways did (one switch more, since removed;
> descriptor passes did not yet keep what an earlier pass had not taken). The run
> of the commit was stopped while loading, when the machine ran low on memory. If it misbehaves, try `-Safe`, or
> go back to `b79832e` (which has the faults named under "Fixed since `b79832e`").

## Use

```powershell
cargo build --release -p victory
powershell -ExecutionPolicy Bypass -File scripts\play-fast.ps1 -Exe "<game dir>\<the title's exe>"
```

| | |
|---|---|
| (nothing) | The switches in the first table |
| `-Experimental` | Those and the ones in the second |
| `-Ambient 1` | An estimate in place of the missing indirect light (`constant:0.6`: the flat light of earlier versions) |
| `-SmallCard` | Without the two switches that want 16 GB of card memory |
| `-Safe` | No switch at all |
| `-Log run.log` | The log goes to a file |

The first start after a new version translates the shaders again and takes
longer. **Reporting a run**: which options, the card and driver, whether it
reached the world, frames a second standing still (window title), what looked
wrong, and the log. Close other games first: one running beside it took a
quarter of the card's memory and half the frame rate here.

## Switches

Tried in the game (the copy queue is victory-fork's own, in place of this fork's
earlier `VICTORY_COPY_FETCH`, which now selects it):

| Switch | What it does |
|--------|--------------|
| `VICTORY_INDIRECT_DIRECT=1` | GPU-driven **mesh** draws read their size straight from the title's buffer. The clamp ran a compute pass before each of some 3,800 of them a frame: 4.26 microseconds a draw on an RTX 4090 against 0.01 for a bare one (`bare_command_cost`). Other indirect calls keep the clamp |
| `VICTORY_QUEUE2=1` | victory-fork's: what shaders wrote comes back on a copy queue |
| `VICTORY_SAFE_COPY=1` | Title memory is read in place with the fault caught, in place of a kernel call for each of some 13,000 small copies a frame |
| `VICTORY_WRITE_BESIDE=1` | Shader results go into title memory through a second mapping, so that the page stays guarded and is not copied, uploaded and hashed again |
| `VICTORY_SKIP_SAME=1` | Of a page found written, only the 4 KiB pieces that differ are taken and sent up (101 to 24 MB a frame) |

With 16 GB of card memory or more, `VICTORY_CARD_WINDOWS_MB=6144` and
`VICTORY_CARD_BUFFERS_MB=4096` (victory-fork's own) keep more on the card.

Newer, tried in a few launches at the one spot below (`-Experimental`):

| Switch | What it does |
|--------|--------------|
| `VICTORY_MERGE_FINE=1` | The title's small changes under shader writes go up as one dispatch that writes exactly their bytes. At the cap of 512 runs the submitting thread waited for everything in the air (24 to 27 ms a frame) |
| `VICTORY_GUARD_ONCE=1` | A submission asks the page guards once, not once for each copy |
| `VICTORY_HEAP_SYNC_PAGES=1`, `VICTORY_SYNC_ASIDE=1` | Descriptor records copied as bytes: a page at a time, and on a thread of its own |
| `VICTORY_WIDE_REFRESH=1` | victory-fork's: copies refreshed on several threads; here textures too |
| `VICTORY_BURY_THREAD=1` | What is let go is freed on a thread of its own |
| `VICTORY_WIDE_HASH=1`, `VICTORY_CARD_REHASH_RUNS=1` | Page hashes on the card by 64 lanes, and (victory-fork's) joined |
| `VICTORY_ASYNC_SUBMIT=1`, `VICTORY_TEXTURE_NO_WAIT=1` | victory-fork's |
| `VICTORY_AMBIENT=1` (`-Ambient 1`) | The indirect light, which needs rays that do not run, is estimated from the light the visible surfaces send out (avatarmanz, [VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)); `constant:<level>` is beloooo's constant ([Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)) |

## Always on

- **Fix: a title write could be undone.** The copy-back put a whole stretch of
  memory back, with title bytes read a moment before; a write landing between
  was lost. Only the words the card wrote are put back now. The fault is in
  victory-fork's default path; a test shows it
  (`warp_small_title_writes_under_shader_writes_reach_their_round`).
- Pixel shaders: a helper lane's `v_readfirstlane` reads its own value (upstream
  `0601955`); a quad is read through quad reads (beloooo, `03629f3`).
- Mesh primitives: edge flags are no part of the index (upstream `c0228d9`).
- Upstream `40f30be`, `7d04943`, `c3a04e2`, `e45bacd`, `97c8783`, `4e155a3`,
  `be3202d`.
- The guards' clean blocks as a bitmap, one protection call for consecutive
  blocks, readers of the heap table beside each other, one barrier for a
  submission's uploads.

- Shadows of hair and see-through things: the program that draws them
  (`CS_ShadowRenderTiledCascadesAlpha`) is translated, per host wave of 32 (an
  approximation). The stepped edges at a shoulder in the sun were gone in the
  one launch with it.
- `VICTORY_WAVE_WIDE` takes a comma-separated list of container digests.
- **Fix: blocks along the bottom of the picture.** Indirect dispatches were cut
  at 65,535 thread groups; the title's tile pass asks for some 67,000, and the
  last tiles never ran. On AMD and NVIDIA cards up to 2^20 now run as asked (a
  test checks every group on the card; `VICTORY_INDIRECT_WIDE=0` turns it off).
  From avatarmanz/VIctory-7900XTX `84eae35`.

## Fixed since `b79832e`

- `VICTORY_WRITE_BESIDE=1` kept its mapping of a section after the title freed
  it: the memory stayed, and a handle used again could have sent writes to
  another section.
- `VICTORY_COPY_FETCH=1` could miss what a later flight wrote to a piece fetched
  beside it.

Both were off by default, and on with `play-fast.ps1`.

## Measured

One machine (RTX 4090 24 GB, Ryzen 7 7800X3D, 32 GB RAM, Windows 11), one fixed
view in the first gameplay room, about 4,900 draws, Series X profile. A few
milliseconds either way are noise.

| | ms a frame | frames a second |
|---|---|---|
| victory-fork `9376fd6`, defaults (1 launch) | 207-228 | 4.4-4.8 |
| this fork `b79832e`, its switches (4 launches) | 105-125 | 8.0-9.6 |

Builds close to `2756021`, with every experimental switch, the constant light at 0.6
and five digests in `VICTORY_WAVE_WIDE`: 103-110 ms (1 launch; standing still, after one to two minutes of 150-1,300 ms frames as the world loads),
102-117 with a profiler on and the hair shadows (1 launch). No faster than `b79832e` by more than the noise; what it buys is a
shorter copy-back and the fixes above.

With the two ports of 26 November (the estimated light at 1, wide indirect
dispatches): 104-118 ms in the same view (1 launch). About 15 GB of card memory and 16 GB of RAM were in use.

## Known risks and problems

- **A GPU hang is possible** with `VICTORY_INDIRECT_DIRECT=1`: mesh draws are not
  bounded. A form that also covered vertex draws hung the card at world load in
  3 launches of 8. With the mesh-only form there was no hang in 17 logged world
  loads up to `b79832e`, all in the first room, not all with every switch on.
  Save your work before playing.
- The sky showed colour banding in one run, with another game starting beside
  it; the cause is not known.
- **Do not turn on `VICTORY_BVH_BUILDER=1` with `VICTORY_FIRSTLANE_OWN=1`.** Every
  program of the structure builder compiles that way, but a run of it wrote over
  the title's memory and crashed it, with `VICTORY_BVH_QUARANTINE=1` too.
- It takes some 16 GB of RAM and a commit of 30 GB.
- Tested at one spot, on one NVIDIA card. AMD cards are untested.
- Shadows are black without `-Ambient` (no ray tracing); a green patch
  and a black shape remain at the lower right of the first room, and its window
  lacks the wet glass.
- Tests: two of `victory-shader`'s rectangle tests fail, as in victory-fork; in
  `victory-gpu` one test of the in-place path fails now and then, and a run
  crashes now and then (seen in victory-fork too; cause not known).

## Credits

fxceless (victory-fork), Marvisak and contributors (VIctory), beloooo, moe-miau
and the authors of the upstream commits named above, and everyone credited
below. Licensed GPL-3.0-or-later, as VIctory is. These changes were developed
with [Claude Code](https://claude.com/claude-code). Everything below is
victory-fork's own README, unchanged.

---

# VIctory fork (work in progress)

A work-in-progress fork of [VIctory](https://github.com/Marvisak/VIctory), a
compatibility layer that runs an Xbox Series title natively on Windows from
files the user supplies. This fork follows upstream
`main` and adds fixes aimed at one goal: getting the game's world to render
correctly and playably on one particular PC.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's
developer or publisher.

## Credits

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors is the base of everything here: the loader, the runtime
  services, the Direct3D 12 back end and the shader recompiler. Licensed
  GPL-3.0-or-later; so is this fork. Upstream's history is kept intact below
  the single commit that carries this fork's changes.
- **[XCL](https://github.com/ivall/xcl)** by ivall and its contributors
  (GPL-2.0-only) was used as a reference for how the same title behaves under
  another layer. Fixes inspired by it were written anew in VIctory's code.
- The guard over every view of a memory page is code by **myokex**
  ([victory-amd](https://github.com/myokex/victory-amd), commit `c6dc4d8`), which
  adapts pull request #115 here by Spexivus, its correction by joroyt and a part of
  VIctory #1122 by tightfall. The read-beside fix for kernel reads into title
  memory is by **Joshua Odom** (commit `10ea38e` of the same fork family). Both
  are carried here unchanged apart from one merge conflict.
- This fork's changes were developed with [Claude Code](https://claude.com/claude-code).

## Status of this fork (2026-11-25)

**Not playable yet: 2.5 to 4 frames per second, and shadowed areas are black.**
Tested on one machine: Windows 11, Intel i7-14700KF, NVIDIA RTX 4070 12 GB,
32 GB RAM. The Series X profile is used throughout.

| Stage | State in this fork |
|-------|--------------------|
| Boot, menus, start screen | works, 30 fps (as upstream) |
| Opening cutscene | plays from start to finish, in order, lit: title cards, credits cards, night and dusk shots |
| Saves | the game autosaves a checkpoint and loads it again |
| First gameplay scene (the prologue) | renders: characters, textures, sky, sunlit exterior; HUD, pause menu and pad input work |
| Light in shadow | missing: the game's ray-traced bounce light and ray-traced lamp and sun shadows do not run yet, so anything the sun does not hit directly is black |
| Frame rate in the world | 2-4 fps: 420-450 ms per frame by default, 240-350 ms with the newer switches (`VICTORY_RELAY=2`, or `VICTORY_LATE_WAIT=3 VICTORY_WIDE_BACK=1 VICTORY_CARD_BUFFERS_MIN_MB=1 VICTORY_CARD_BUFFERS_MB=1024`); it started at 3-6 seconds per frame |
| Stability | no driver hang or device loss in normal runs (a hang introduced by the wave64 update was found and fixed: its barrier form for 64-lane programs is now off unless `VICTORY_WAVE_WIDE=1`) |

### Newest (2026-11-25, later the same day)

**Update, the same night: the first room now renders right on the test
machine** (walls, textures, people, lamps), at 190 to 450 ms a frame (2 to 5
frames a second): the picture work has caught up, the frame rate has not.
What did it, on top of the list below:
- Descriptor records the title copies into its heap as plain bytes are
  mirrored before every submission (pull request #305 by Flyander, after
  upstream VIctory 86a5ed6). Measured here: about 40,000 records a frame
  that no call had announced; their host slots were empty or stale.
- A buffer view that does not start on an element of its buffer's host
  copy gets a copy of its own bytes instead of being refused (after
  upstream 86a5ed6): 3 to 6 such views a frame.
- Room on the card is kept for the render targets and buffer copies a
  world makes in its first frame: 98 buffer copies are made (945 MB) where
  the runs before made 1, and the processor no longer compares 550 to
  710 MB a frame looking for shader writes (25 MB now).
- Levels on demand ran on the card for the first time
  (`VICTORY_LEVELS_ON_DEMAND=1`, still off by default; with the fixes of
  pull request #747 by AscilCH): 3.1 GB of the card not allocated for
  texture levels the title does not hold, no device loss.
- Guarded blocks under storage writes are opened by default; 14 more
  shader programs translate; a program is translated with first-half
  interpolation pairs only if it was refused for one (with that on for
  every program, 8,551 working pixel programs came out changed).
- Frame rate, first cuts on the processor side: the descriptor sync uses a
  cheap fingerprint and leaves draw-table slots to their draws (the pass
  over the heap went from 70-220 ms a frame to about 3), and memory the
  guards already hold is not asked of the write watch (pull request #670
  by Jonirulah). Card copies' pieces can come down inside their own
  flights (pull request #648 by Jonirulah, `VICTORY_CARD_FETCH_INFLIGHT=1`,
  off by default: the copy-back thread's wait went from 181 ms a frame to
  0 here, but the card got busier, so no gain is claimed yet). The frame is
  still 250 to 380 ms: the card is the limit now, most of it the
  per-record clamp before each of some 6,700 indirect draws a frame.
- Pull request #220 (cheaper indirect draws) is IN this tree now, its
  windows off by default (`VICTORY_INDIRECT_WINDOW=1`). History, so nobody
  repeats it: with windows on it lost the device three times here. Its own
  checker (`VICTORY_INDIRECT_WINDOW_CHECK=1`) then showed one list drawing
  90 records of 2^24 vertices from a window, where the per-draw path lets
  eight through; a record of 2^23 vertices an instance or more now runs
  empty. Since that fix: five runs with windows on, no device loss, the
  card's time per frame roughly halved. One loss after the fix had the
  in-flight fetch (#648) on as well; do not combine the two. The in-flight
  fetch itself measured slower here in two runs and stays off.
- Frame rate, second night (an RTX 4070; the first room went from about
  3.5-4 to 6-7 frames a second with ALL of the switches below; the
  defaults are unchanged and give about 4.5-5):
  - `VICTORY_ASYNC_SUBMIT=1`: submissions are prepared and sent by a
    thread of the layer, not by the title's render thread, which a
    profile showed inside the submit path for 257 of 292 ms a frame.
  - `VICTORY_QUEUE2=1`: fetches of what shaders wrote go to a copy queue
    (brought back, with the fix for a piece fetched beside a later
    flight); the copy-back thread's wait fell from about 125 to 7 ms a
    frame.
  - `VICTORY_INDIRECT_WINDOW=1`: see above.
  - `VICTORY_TEXTURE_NO_WAIT=1`: a texture refresh no longer waits while
    the title remaps memory; the loading phase had 5 readings over a
    second where it had 18.
  - `VICTORY_MERGE_GAP=64 VICTORY_MERGE_RUNS=4096`: title bytes put in
    under flights' shader writes are joined over small gaps (NOT exact:
    read the comment at `merge_gap` before using it).
  - On by default: fills of upload memory and read-only buffer copies no
    longer wait for flights and are checked against what the flights then
    wrote (`VICTORY_FILL_WAIT=1` waits as before; no mismatch in any run
    so far); up to 512 idle host lists and texture staging buffers are
    reused instead of made and freed every frame.
  - `VICTORY_PROFILE=1` (diagnostics builds): a sampling profiler inside
    the layer; it is how the above was found.
  Still open: the frame rate dips (5-second readings of 300-500 ms among
  150-215); in some of them texture uploads are 3-5 times the usual, in
  others nothing logged stands out. Not shader compiles (checked: 4-9
  first uses and no waiting in those readings). A per-frame logger is the
  next step.
- Known and counted, not fixed yet: an indirect compute pass asks for
  about 77,500 thread groups in x where one dispatch runs 65,535 (the
  rest is cut off).

Memory sync between the title's memory and the copies on the card:
- **A write through one view of a physical page is seen by the copies taken
  through its other views** (`VICTORY_VIEW_GUARD=0` turns it off). The title
  shows one page at several addresses and a copy was guarded at one of them
  only. Measured on the test machine: 0.8 to 1.0 million such writes in a
  five-minute run, every one of which used to leave a copy stale. Since it
  went in, the first room had its walls in the later frames of 4 runs out of
  4 (2 of 5 before); the room's very first shot still comes up without walls
  in some runs.
- Kernel reads into title memory are read beside and copied in, so a block
  guarded again in between no longer fails the read.
- Writes of the storage layer into physical pages are counted where the title
  has those pages mapped (10,000 to 50,000 guarded blocks of copies lay under
  them in every run) and can open the guards there:
  `VICTORY_PAGE_WRITES=guards` (off by default; `1`, `slots`, `all` do more).
- Failed page-destination reads are retried (`VICTORY_DSTORAGE_RETRIES=0` = as before).

Card memory (the test card is full from the first frame of the world: 85 to
99 % of its budget for a whole run, about 26,000 texture copies let go and
3,400 made again in five minutes, 2.8 GB of render targets that are all in
use):
- Levels on demand, **off by default** (`VICTORY_LEVELS_ON_DEMAND=1`): a
  texture the title fills takes card memory only for the levels the title
  holds (2.2 to 3.0 GB of the copies were levels it does not hold). Its test
  runs on the software adapter; it has not run on a card yet.
- Refused texture slots are held back for a few seconds so that a pass
  reaches the slots behind them (`VICTORY_FULL_CARD_PASS=0` = as before);
  room can be kept for the card copies of title buffers
  (`VICTORY_CARD_ROOM_FOR_BUFFERS=1`, off).

Tools and diagnostics:
- `victory translate <shader file> --warm all --jobs N` translates every
  shader ahead of a run, on the processor only (about 4.5 to 6.5 minutes
  here); the shader cache is keyed on the translator's source, so a new
  build that does not touch the translator starts warm.
- Counters, all off or diagnostics-only: `VICTORY_MERGE_RACE=1` (title bytes
  a copy-back is about to put back to older values: none found in 138,000
  copy-backs), `VICTORY_WALLS_TRACE`, `VICTORY_CARD_ACCOUNTING`, and a census
  of the copies the texture sweep never lets go.

### What this fork changed (summary)

Rendering:
- Meshlet culling read its scene buffers as the wrong kind of view, so the world drew almost nothing; fixed.
- Per-draw descriptor tables could run out, after which draws used slots never made for them and the card hung; fixed.
- Light packed by the game's compute shaders was read back through empty copies (sky, probes, fog); fixed for every mip level and layer.
- The fog reconstruction, shadow-mask, hair, blend-shape skinning, fire and several material programs did not translate; fixed.
- Gathers at a named mip level (with and without an offset) read level 0; fixed.
- Implicit-level samples outside pixel shaders produced invalid DXIL in 730 compute programs; fixed.
- Rectangle lists, depth and stencil twins for depth textures written by compute, texture read-backs.
- Ray tracing, in progress and off by default: the game's own structure-builder programs translate, title memory can be read by raw address from shaders, the hardware ray/box and ray/triangle test is emulated in DXIL (checked against a reference on the GPU), and 59 of the 64 trace programs translate. Nothing is lit by it yet. See [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md).

Speed and memory:
- Waits for memory values, write-watch queries and page queries no longer walk page tables per call.
- Large title buffers get a copy in card memory, with shader writes found by hashing on the card.
- Submissions can be sent ahead of the wait for the card (`VICTORY_LATE_WAIT`), with a relay thread that lands results as the card finishes (`VICTORY_RELAY`): in a first five-minute run the card's idle time behind the submitting thread fell from about 136 to 30 ms per frame.
- Title memory can be opened as a graphics heap so that buffers are used in place instead of copied (`VICTORY_INPLACE`, first stage only; a probe confirmed the approach on the test GPU).
- The bindless texture queue no longer uploads stale views; the view table no longer grows without bound; the pipeline library is mapped instead of read into memory.
- An intermittent device loss was traced to the pipeline library file being replaced while mapped; fixed.

- Translated shaders get clean-up passes over the finished module (dead code, constant folding, reuse of values, branch facts); the pack's host instruction count falls by about 43 %, worth about 20 ms of GPU time per frame on the test machine (`VICTORY_SHADER_OPT=0` turns them off).

Most of the newer speed changes sit behind environment switches that are off
by default (`VICTORY_LATE_WAIT`, `VICTORY_RELAY`, `VICTORY_WIDE_BACK`,
`VICTORY_CARD_BUFFERS_MIN_MB`, `VICTORY_INPLACE`, `VICTORY_BVH_BUILDER`, ...).
Each is described in a comment where it is read.

### Known problems

- Everything in shadow is black (see above).
- The interior of the first gameplay room is sometimes not drawn after the cutscene hands over to gameplay.
- Around 24 GB of memory is in use during play.
- A few style lints are switched off for this fork in the workspace `Cargo.toml` until the newer code is tidied.
- Many changes here are specific to this title and this GPU, and some are marked `shortcut:` in the code where they approximate what the console does.

Everything below this line is upstream VIctory's own README, unchanged; its
status section describes upstream, not this fork.

---

# VIctory

Runs the title (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Status (2026-11-23)

**Not playable yet.** The title boots, runs its menus at its own 30 fps and
loads the world, where rendering fails.

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about 50 s) | works |
| The world (first mission's intro) | reached and runs with indirect draws; its title card renders, the scene after it is still dark; 0.5-1.8 s per frame |

Tested on one machine: Windows 11, AMD RX 6700 XT.

> **Warning.** Entering the world on a real GPU can currently hang it, and
> Windows then resets the display driver (every monitor goes black for a
> moment). To look further, use a diagnostics build with
> `VICTORY_D3D12_WARP=1`, which renders on the software adapter.

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in our process; crash reports name the faulting code and are kept in `crashes.log` | – |
| Runtime services | One signed-in offline user, licences, packages, saves that survive a kill at any step, text entry dialog | Speech is silent; the dialog and relaunch are untested live |
| Network | Offline by design: the title cannot reach the network | – |
| Files and streaming | Archives and streamed data read, zlib and BCPack decoded in parallel | Loading is limited by writes into title memory |
| Input | XInput pads, keyboard, keyboard stand-in pad | Trigger rumble |
| Audio | Menu, video and loading sound; XMA voices and Opus streams decode | Reverb is silent; Opus results unverified against the title |
| GPU: menus | Draws, compute and presentation on host D3D12; only changed title memory is copied | Frame pacing drifts on one screen in release builds |
| GPU: world | Mesh pipelines, shader-written buffers, descriptor lifetimes, indirect execution (arguments clamped), pipelines made by worker threads and kept on disk | Most of the scene is still missing (untranslated shaders drop draws); frames take seconds; real queries, depth bounds, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL at first use, cached on disk. Menus: all. World: mesh 8,191 of 8,506, vertex/pixel 10,617 of 12,346, compute 1,643 of 2,539 | 64-bit image atomics, merged geometry stages, the ray-tracing builder's shaders; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Runs through the menus to the loading screen | The world, as above |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager → Performance → Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. With none connected the keyboard is a pad while the
title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_PIPELINE_WAIT_MS=24` | reduce the per-frame wait for background pipeline creation from the default of 500 ms. A draw can be omitted until its pipeline is ready |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
