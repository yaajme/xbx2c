# xbx2c

xbox series x -> windows. fork of a fork of a fork of [VIctory](https://github.com/Marvisak/VIctory)... see credits at the bottom, its a long chain.

**not playable yet** (well, kinda. it gets into the world. dont expect much)

no game files in here. no code from the game, no assets, nothing. bring your own dump, I'm not helping you get one so dont ask. not affiliated with the dev or publisher of anything.

## what changed (nov 26)

main used to be my own perf branch. it isnt anymore. I tested codec_d's `cool_fox` build (which sits on avatarmanz's VIctory-7900XTX) against mine and it was just better, like not even close, so main is now that tree as-is + a merge commit so my history is still there.

my old stuff (THIN v1, hair shadows, DispatchX 0x20, INDIRECT_DIRECT / SAFE_COPY / WRITE_BESIDE / SKIP_SAME, EARLY_COPY) is **only in history now**, not in the code. going to port the bits that actually help back one at a time and measure each one. or not, if they dont help.

## numbers

one box: rtx 4090, 32gb ram, 32gb fixed pagefile, win11. series S profile (the launcher forces it, lower res than series X so keep that in mind when comparing).

```
menus/intro                            30 fps (capped)
world load + first shader translate    0-9 fps, its translating, ignore it
in world, ~4-5.8k draws/frame          24-25 fps (~41ms)
my old xbx2c THIN build, same box      102-115 ms/frame
```

that's ONE run. single launches swing 10ms+ for me with identical settings so take it with salt. didnt see the screen during that run either so I cant swear it was the same spot as my older numbers

**memory is the real problem.** game sat at ~34.4gb private and free commit went under 1gb at ~5min, my guard killed it. if you have 32gb ram close chrome/discord/steam/whatever first. 64gb your probably fine

## build + run

need: win10/11 x64, AVX2 cpu, d3d12 gpu w/ SM 6.6 + mesh shaders, the pinned rust toolchain, MSVC build tools + windows sdk

```powershell
cargo build --release --locked -p victory
$env:VICTORY_GAME_DIR = 'D:\wherever\your\dump\is'
.\Play-VIctory.cmd pareto-residency
```

dump folder needs `game_scarlett_master.exe` in it or the launcher bails.

first start translates every shader again -> slow + stuttery. cache goes in `%LOCALAPPDATA%\VIctory\cache-pareto` (set `VICTORY_CACHE_DIR` if you want it somewhere else). second run is way better

the launcher turns on a pile of `VICTORY_*` switches (THIN, RELAY=2, CARD_STEPS=2, JOIN_PARTS=8, LEVELS_ON_DEMAND, etc). any you set yourself beforehand win over its defaults. **clear out old VICTORY_ vars from other forks** before running or you get weird mixes, ask me how I know

log goes to stderr, `VICTORY_LOG=warn` by default. pipe it somewhere

every switch has a comment where its read in the code. thats the docs, sorry. theres also [docs/README-fxceless.md](docs/README-fxceless.md) and [docs/findings.md](docs/findings.md) from upstream

## broken / known bad

- the "oil spill" look (blotchy painted walls/skin/clothes + brown haze) was the fake indirect light, `VICTORY_AMBIENT=0.8` + `VICTORY_AMBIENT_AIR=1.5`, that cool_fox's launcher turned on. its averaged on a 64x36 grid and smeared over the screen by depth so you get flat patches w/ hard edges. launcher doesnt set it anymore -> clean image but rooms are darker (real GI needs RT which we dont run). set them yourself if you want it back. working on a smoother version
- the frosted/melty window in the prison cell is CORRECT, thats the games glass. old xbx2c drew it clear which was wrong
- 3 compute shaders dont translate (`no wave32 lowering for this wave64 program`) -> their draws get dropped. no idea yet whats missing on screen bc of it
- textures picked by index keep getting re-uploaded ("changed after its upload with nothing to tell of it"), spams the log, probably costs frames
- trees/plants/far buildings flicker outdoors in places (upstream says, havent checked myself)
- dark rooms too dark w/o ray tracing
- streamed-in textures pop in late
- recording a frame in the world trips the game's own watchdog

tests:
- `victory-shader` rect + barycentric tests fail on my machine, DXIL validator mismatch ("Pipeline State Validation"). fails on every branch I have so its the env not the code
- two `indirect` tests are `#[ignore]`d. codec_d flipped `VICTORY_INDIRECT_REST` to off by default and didnt update them. `VICTORY_INDIRECT_REST=1 cargo test -- --ignored` and they pass
- diagnostics build of victory-gpu has a couple timing tests that flake under load. rerun

## asset policy

repo only holds small text files. no dumps, no logs, no binaries, no screenshots. run `cargo xtask install-hooks` once and every commit runs `cargo xtask policy --staged`. upstream branches love committing multi-MB run logs under `diagnostics/`, strip those before merging anything in. see [docs/asset-policy.md](docs/asset-policy.md)

## credits

this is mostly other peoples work, history is kept, commits pulled from elsewhere say where they came from

- [VIctory](https://github.com/Marvisak/VIctory) - Marvisak + contributors. loader, runtime, d3d12 backend, shader recompiler. the actual hard part
- [fxceless/victory-fork](https://github.com/fxceless/victory-fork) and everyone whose PRs it carries
- [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX) - the base cool_fox is built on
- codec_d (`cool_fox`) - RTX + 32gb ram residency work, nvidia wave32 lowering, XMA audio fixes, series S plane fix. what main is now
- [DexNom/VIctory](https://github.com/DexNom/VIctory) - RT fixes, cube gradients, sparse textures
- [Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD) - ambient light idea, pipeline workers, romainhedouin's stuff
- [FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)
- [myokex/victory-amd](https://github.com/myokex/victory-amd) - guard over every view of a page (w/ Spexivus, joroyt, tightfall), Joshua Odom's title memory reads
- Geckomacabre + cxy0072019-sketch - texture readback (boats float now lol)
- [XCL](https://github.com/ivall/xcl) (GPL-2.0-only) was read to see how the same title behaves under another layer. no code from it in here

a lot of this tree (mine + several of the upstream forks) was written with claude code. figured you should know

## license

GPL-3.0-or-later, same as VIctory. [LICENSE](LICENSE). the one file not covered is named in [docs/asset-policy.md](docs/asset-policy.md)
