# uncl: rendering architecture review — why it is slow, why full‑screen apps break, and the shortest path to "renders everything natively"

**Scope:** `uncl` @ `ff09124` (1,286 LOC of Rust, MIT, `ratatui 0.29` + `tui-term 0.2` + `vt100 0.15.2` + `portable-pty 0.8`).
**Method:** static review of the whole tree (`src/**`), plus source-level inspection of the candidate engines and of `zellij` as the closest sibling design. The sandbox has no Rust toolchain, so **no numbers below are measured** — every performance claim is a structural argument with a cheap way to verify it, given in §2.8.
**v2 (2026-10-26):** the plan in §6 is revised against the recorded decisions in §0.1 (floating is non-negotiable; images are cheap-tier; mouse required; Windows/Docker nice-to-have; `vt100` upgraded via the vendored adapter).

---

## 0. TL;DR

1. **The parser is not the bottleneck.** `vt100`/`vte` parse at hundreds of MB/s; uncl parses a few KB per frame. The slowness is (a) a 10 ms tick that renders the whole screen ~100×/s unconditionally, and (b) an event loop that processes **exactly one input event per tick**, which caps input and paste throughput at ~100 events/second. Neither has anything to do with `ratatui`'s drawing speed.
2. **`vt100 0.15.2` is the wrong pin, and it is the direct cause of the "poor sequence parsing" you are working around by hand.** `0.16`+ exposes the parser's *callbacks* (unhandled CSI/OSC/ESC) **and** first‑class terminal modes (`mouse_protocol_mode()`, `mouse_protocol_encoding()`, `application_cursor()`, `bracketed_paste()`, `hide_cursor()`, `cursor_style()`). That deletes the `String::from_utf8_lossy` + 20×`contains()` hack in `owner.rs:203–260`, which is both slow and **incorrect across read boundaries**.
3. **BTOP doesn't render because of three stacked geometry bugs plus a mouse-routing bug, not because of the parser.** The float's PTY is sized `rect − 4`, the parser grid is sized `25×80`/`rect`, and the widget paints into `rect − 2` — for a full-screen TUI, every one of those mismatches shows up as a shifted/garbled frame. And when the float is visible, **all mouse events are consumed by drag/resize** — the child never sees a single mouse event (`owner.rs:371`). BTOP is largely mouse-driven.
4. **"Renders everything natively" is not reachable by swapping the parser.** No Rust engine paints a *grid back to ANSI* with fidelity, and the features you are missing (`sixel`, kitty graphics, hyperlinks, `OSC 52`) are dropped **below** the grid layer: `vt100` does not implement `vte::Perform::hook/put/unhook` (DCS payloads vanish), and upstream `vte` routes `SOS/PM/APC` bytes into `anywhere()`, which discards them (kitty graphics APCs vanish for *every* `vte`-based engine).
5. **Decided plan (§0.1 answers applied): four stages, ~2,300 net LOC, ~16 working days for S0–S3 (~20 with the Windows delta and a buffer).**
   - **S0 — stop being slow** (~2–3 days): event-driven redraw, drain-to-empty input, bracketed paste, batching writer, `vt100 0.18.1` via the vendored 123-LOC adapter, delete the byte-sniffing.
   - **S1 — stop lying about geometry and input** (~5 days): one source of truth for the float's size, mouse/key routing by region with mode-aware per-pane encoders, paste, and a **capability-proxying reply channel** (uncl must query the *host* terminal once and relay its answers to children — including the sixel bit, or nothing will ever emit an image).
   - **S2 — own the emitter** (~1 week, **shipped** — Appendix E): `ratatui`+`tui-term` are gone; ~980 LOC turn the grid into ANSI bytes via `vt100`'s `rows_diff`/`rows_formatted` plus our own cursor and border emission (`state_diff` and `cursor_state_formatted` turned out to be unusable — see Appendix E). **This was load-bearing**, because floating is non-negotiable: it is the only route to both speed and pixel fidelity, and the only place out-of-band image bytes can be emitted.
   - **S3 — cheap-tier images** (~3 days, **shipped** — Appendix F): DCS/APC/OSC-1337 tap + placement store + re-emit after the grid. kitty and iTerm2 work out of the box (kitty re-places by id, so a move costs one line); sixel is opt-in and drawn only if it can fit. No re-encoding, no scaling, no crop.
6. **Two things are off the critical path.** *Passthrough takeover* (v1 §6.5) is incompatible with "floating is the product" — demoted to Appendix B, where it survives only as a user-invoked "maximize" and as a debugging aid. *Swapping in `alacritty_terminal`* (S4) stays optional: it buys underline colour, `OSC 8` links and kitty-keyboard input, none of which BTOP-class rendering needs.

## 0.1 Decisions recorded (2026-10-26)

| # | Answer | Consequence for the plan |
|---|---|---|
| 1 | **Floating while a TUI runs is the entire product — non-negotiable.** | Compositing is mandatory forever. No takeover shortcut. S2 becomes load-bearing rather than optional; budget for it and do not let it slip behind S3. |
| 2 | **Inline images: good to have; quality compromises accepted; "whatever gets there cheaply".** | Scope = the cheap tier in §6.4–6.5: passthrough the child's own bytes, no decode, no scale, no re-encode, drop what doesn't fit. kitty gets real cropping for free (the terminal does it); sixel gets none. Windows: images are off (§6.6). |
| 3 | **Mouse interaction is required.** | S1's region routing, per-pane encoding (X10/SGR/UTF-8 by the *child's* reported mode), scroll-wheel mapping and motion filtering are mandatory, not polish. `Home` can no longer be the only local chord, and the float's own drag/resize needs a non-mouse-stomping gesture. |
| 4 | **Windows / Docker "good to have".** | Adds §6.6: raw-ANSI output needs VT output mode (crossterm already turns it on at startup), ConPTY eats child graphics ⇒ images are Unix-only, and the Windows input path is console records rather than VT sequences ⇒ isolate the input encoder behind a trait and `cfg` it. Docker needs only env/size propagation. |
| 5 | **Upgrade `vt100` with the adapter if possible.** | Confirmed as S0: copy `tui-term`'s `src/vt100_imp.rs` (123 LOC) into `src/app/ui/adapter.rs`, adjust for the 0.18 API (`NonZeroU16` sizes, `Screen::size`), delete the `tui-term` dependency, pin `vt100 = "0.18.1"`. This unblocks deleting `owner.rs:203–260` in the same PR. |

---

## 1. What the code does today

```
                 ┌──────────── stdin (raw mode, mouse capture) ────────────┐
                 │                                                        │
  crossterm poll/read (1 event max per 10 ms tick, owner.rs:354)          │
                 │                                                        │
        ┌────────▼────────┐   not visible    ┌───────────────────────┐    │
        │  keyboard.rs    │───────────────►  │  owner PTY (self.tx)  │◄───┘
        │  mouse.rs       │                  └───────────────────────┘
        └────────┬────────┘   float visible
                 └──────────────► tenant PTY (lease.tenant_tx)

  reader threads (spawn_blocking, 8 KiB chunks)
        owner  bytes ──► [20× contains() sniff] ──► vt100::Parser(rows, cols, 0)
        tenant bytes ─────────────────────────────► vt100::Parser(25, 80, 0) → later rect

  frame (every 10 ms tick, owner.rs:445)
        terminal.draw ──► Container::render    (full-screen PseudoTerminal, owner.rs:308)
                        └─► Overlay::render      (PseudoTerminal inside a Block, tenant.rs:251)
```

Three things are shared between threads through `Arc<RwLock<vt100::Parser>>`: two producers (the PTY readers) and one consumer (the renderer). Everything else is synchronous inside one `run()` loop.

---

## 2. Why it is slow — ranked, with evidence

### 2.1 The 10 ms tick renders unconditionally (`owner.rs:341–446`)

```rust
loop {
    ...
    tokio::time::sleep(Duration::from_millis(10)).await;   // :444
    terminal.draw(|f| self.render(f, parser.read().unwrap().screen()))?;  // :445
}
```

There is **no dirty flag anywhere**. Every 10 ms the loop rebuilds two full-screen `ratatui` buffers (the owner screen and, when visible, the float), diffs them against the previous frame, and flushes. For an 80×25 screen that is 2 × 2,000 cells per frame, i.e. ~400,000 cell-style constructions per second at idle. `tui-term`'s adapter (`src/vt100_imp.rs`, 123 LOC) does, per cell, a `Style::reset().fg().bg().add_modifier()` plus colour enum mapping and a `set_symbol` string copy. That work happens whether or not a single byte arrived from either PTY.

Idle CPU is therefore a pure function of the tick rate, not of activity. Expect the process to sit at several percent of a core with nothing happening, and to scale with terminal size.

### 2.2 One input event per tick (`owner.rs:354–423`)

```rust
if poll(Duration::from_millis(0))? {
    match read()? { ... }     // exactly one event
}
```

`crossterm` queues every event it reads from stdin. This loop consumes **one per 10 ms**:

- 100 events/second ceiling for typing, arrow keys, mouse motion, and everything else.
- Up to 10 ms of added latency on every keystroke.
- **Paste is unusable.** uncl never enables bracketed paste on the host terminal, so a paste arrives as N separate `KeyCode::Char` events. Pasting 500 characters takes ~5 seconds of visible character-by-character insertion, and each character is its own `write_all` + `flush` syscall pair to the PTY. `Event::Paste(_)` is explicitly discarded at `owner.rs:411`.
- Mouse motion under a TUI drag floods the queue; the queue then lags further behind.

This is, I suspect, the "slow" a user actually feels. It is also ~15 lines to fix (drain `poll(0)` in a `while` loop; enable bracketed paste; batch writes).

### 2.3 Blocking sends to the PTY inside the UI loop

`handle_keyboard_input(...).await` and the mouse path `await`/`try_send` into a **bounded 32-slot** channel (`lease.rs:21`). When the channel is full — trivially during a paste or a heavy mouse-drag stream — `send().await` parks the event loop, so rendering *and* input stop entirely until the writer thread catches up. `keyboard.rs` also `unwrap()`s every send, so a closed channel panics.

### 2.4 Renderer holds the parser lock; readers need it per chunk

`terminal.draw(|f| ... parser.read().unwrap() ...)` holds the read lock for the entire frame. Both reader threads take `parser.write()` for every 8 KiB chunk. At 100 Hz with ~1 ms frames, the writers are blocked ~10% of wall time; with larger terminals and a busy child (BTOP redraws constantly) the PTY pipe buffer fills and the child's `write()` blocks, which the app experiences as its own slowness.

### 2.5 Byte-sniffing the escape stream (`owner.rs:203–260`)

Per chunk: one `String::from_utf8_lossy` (full copy, allocation) plus ~20 `contains()` scans. Two problems beyond the cost:

- **Correctness:** a DECSET/DECRST sequence that straddles an 8 KiB read boundary is never matched. Mouse mode silently desynchronises — this is exactly the "poor sequence parsing" you describe.
- It is redundant: `vt100 ≥ 0.16` already tracks mouse protocol mode and encoding, DECCKM, keypad mode, bracketed paste and cursor visibility (not DECAWM/`?7`, not focus/`?1004`). The right fix is to *delete this block*, not to make it faster.

### 2.6 `crossterm::terminal::size()` syscall per event (`owner.rs:355`)

`ioctl(TIOCGWINSZ)` on every input event. Trivial on its own; illustrative of the loop's shape.

### 2.7 Miscellaneous hot-path friction

- `render()` re-creates `Block::default()` and a `PseudoTerminal` per frame (cheap, but it also re-`Clear`s the float area every frame — `tui-term`'s `render` starts with `Clear.render(area, buf)`).
- Owner `Screen` is `vt100::Parser::new(rows, cols, 0)` — zero scrollback — so every row the shell scrolls off is parsed and then thrown away. Fine for correctness, wasteful for a shell, and it forecloses feature #5 from your issue tracker.
- `enable_raw_mode()` / `disable_raw_mode()` are called from inside `initialize_pty` *and* from the main loop (`tenant.rs:200`, `tenant.rs:227`, `owner.rs:124`, `owner.rs:439`), so terminal state flips several times during startup; on a slow terminal that produces a visible flash and occasional stuck modes.

### 2.8 How to confirm in 15 minutes (no refactor needed)

```sh
# 1. Idle cost is tick-driven, not parser-driven:
perf stat -e task-clock -p $(pgrep uncl) -- sleep 5      # with an idle shell
# 2. Then comment out only the terminal.draw(...) line, rebuild, repeat: CPU should go to ~0.
#    If it does, ratatui/tui-term are the cost, and §2.1 is confirmed.
# 3. Byte volume per frame is already tiny; this prints syscalls per second:
strace -c -f -e trace=write,ioctl -p $(pgrep uncl)
# 4. Input throughput: hold a key down / paste 200 chars and watch `xev`-style
#    char-by-char arrival in the child, or time `printf 'x%.0s' {1..200}` pasted in.
```

---

## 3. Why BTOP-class apps don't render correctly

Ordered by how much they matter, all verified against the source:

| # | Defect | Evidence | Effect |
|---|---|---|---|
| 1 | **Three different sizes for the same window.** Float PTY = `rect − 4` (76×21 at defaults), `vt100` grid = `25×80` at lease creation then `rect`, widget paints into `Block::inner(rect)` = `rect − 2` (78×23). The title even prints `size.rows/size.cols` (80×25). | `tenant.rs:71–72`, `lease.rs:24–28`, `tenant.rs:251–271` | A full-screen app queries `TIOCGWINSZ`, gets 76×21, draws a 76×21 UI, and uncl displays 78×23 of an 80×25 grid. Guaranteed shift/clip. Resizing re-applies the same `−4` (`tenant.rs:107–108`). |
| 2 | **The float never receives mouse input.** When `tenant_visible`, *all* mouse events go to `handle_mouse` (drag/resize) and are never encoded to the child. `handle_mouse` only implements left-button down/drag/up and doesn't forward scroll at all. | `owner.rs:370–407`, `mouse.rs:12–101` | BTOP/htop/nvim/lazygit mouse is dead inside the float. Nothing to do with parsing. |
| 3 | **No terminal-query replies, ever.** Nothing in the process writes a response to a child's `CSI c` (DA1), `CSI > c` (DA2), `CSI 6n` (DSR/CPR), `DCS $q` (DECRQSS), `OSC 10/11;?`, `CSI ? u` (kitty kbd). The only writes to the master are user input bytes. | `owner.rs:278–292`, `tenant.rs:211–222` | Every app that probes capabilities blocks until its timeout, then guesses. Some (`nushell`, `direnv`-style scripts, some TUIs) misbehave; `CSI 6n` callers stall for seconds. `vt100 0.15.2` cannot even surface these; `0.16+` can (`Callbacks::unhandled_csi`), and `vte 0.15` gives `hook/put/unhook` for DCS if you tap it. |
| 4 | **The cursor is drawn even when the child hid it.** The owner's widget always paints a `RAPID_BLINK` cursor when the float is hidden; the float's widget always paints one. `Screen::hide_cursor()` is never consulted. | `owner.rs:308–317`, `tenant.rs:251–262` | BTOP hides the cursor; uncl draws a blinking block over its UI. Combined with (1), it also *moves* to the wrong place. |
| 5 | **`vt100 0.15.2` predates the fixes that matter.** `0.16`: callbacks, OSC 52, dim, CNL/CPL, resize-request handling. `0.17`: REP, HPA, HVP, DECSCUSR, `CSI 3J`, VS16 emoji width. `0.18`: **xterm-compatible `set_size`** (pulls rows from scrollback instead of trimming the bottom) — i.e. resize corruption is a *known, fixed* bug in the pinned version. | `Cargo.lock:846–849` vs. `vt100` CHANGELOG | Repeated `SIGWINCH` resizes (which resizing the float generates constantly) scramble the grid; `REP`-heavy output (progress bars, BTOP's graphs) mis-paints. |
| 6 | **Host resize updates the parser but not the layout.** `Event::Resize` resizes the parser only; `self.rect`/`self.size` are never recomputed and the float's PTY isn't resized. | `owner.rs:412–421` (`//TODO: fix`) | After resizing the real terminal, uncl keeps rendering into the stale rect. |
| 7 | **Structural fidelity ceiling.** `ratatui::Buffer::Cell` has exactly one char + fg/bg/modifiers. No underline colour, no hyperlinks (`OSC 8`), no images, no zero-width extra data. | `tui-term` `vt100_imp.rs`, `ratatui 0.29` | Anything beyond plain styled text cannot be represented, no matter how good the parser is. |

Note that #1, #2, #4, #6 are **one afternoon of work** and are worth doing before any architectural change, because they are what you are currently attributing to "bad sequence parsing".

---

## 4. What would "renders everything natively" actually require?

Any compositing multiplexer must hold a cell model of both children (the background has to be repainted under/after the float, so its pixels must exist somewhere). "Native" therefore means *the grid + the wire encoder jointly preserve everything the child emitted*. Three things are needed, and they are separable:

1. **A complete state model** — text, SGR incl. underline colour/styles, hyperlinks, modes (mouse/keypad/cursor/paste/focus/sync), cursor shape/visibility, scrollback, alt screen. *This is what a mature emulator gives you.*
2. **A complete state → ANSI encoder** — repaint from the model by emitting SGR/CUP/... to the real terminal. **No off-the-shelf crate does this**; `vt100`'s `rows_diff`/`state_diff`/`contents_formatted` is the only public one, and it covers text+SGR+cursor+input modes only. Alacritty and wezterm both render to a GPU surface, not to bytes.
3. **Out-of-band passthrough for things a grid can't hold** — sixel (`DCS q`), kitty graphics (`APC G`), tmux passthrough (`DCS tmux;`), shell integration (`OSC 133`). These are "blit an image at (x,y)" events. You don't model pixels; you keep a list of `(rect, bytes)` and re-emit on repaint — this is what zellij and tmux (`allow-passthrough`) do.

### 4.1 Where each engine stands

| Engine | Licence / shipability | Size | Text+SGR | Underline colour / styles | `OSC 8` links | Modes + input state | DCS (sixel) visible | APC (kitty graphics) | `→ ANSI` encoder |
|---|---|---|---|---|---|---|---|---|---|
| `vt100 0.18.1` (crates.io) | MIT | 5.1k LOC, 2 deps (`vte`, `unicode-width`) | ✅ | ❌ | ❌ | ✅ mouse/CKM/keypad/paste/cursor, ❌ focus | ❌ (`hook/put/unhook` not implemented) | ❌ | **✅ `rows_diff`, `contents_formatted`, `state_diff`, `cursor_state_formatted`** |
| `alacritty_terminal 0.26` (crates.io) | Apache-2.0 | 11.7k LOC; deps incl. `vte`, `parking_lot`, `regex-automata`, `polling`, `rustix`, `base64` | ✅ | ✅ | ✅ | ✅ incl. full kitty-keyboard (`TermMode`) + `EventListener` | ❌ (no sixel anywhere in the repo) | ❌ | ❌ (you write it) |
| `wezterm-term 0.1.0` (git only) | MIT | ~large subgraph (`termwiz`, `wezterm-{cell,escape-parser,surface,bidi,dynamic}`, `image`, `terminfo`, `csscolorparser`, …) | ✅ | ✅ | ✅ | ✅ | **✅ `sixel.rs`** | **✅ `image.rs`** | ❌ |
| `vte 0.15` (crates.io) | Apache-2.0 OR MIT | 4.1k LOC | — | — | — | — | ✅ raw `hook/put/unhook` | ❌ (`SOS/PM/APC` → `anywhere()` discards) | — |
| `zellij-server` panes (git) | MIT | ~16k LOC (`grid.rs` 7.2k, `terminal_character` 1.4k, `hyperlink_tracker` 1.5k, `grapheme_join` 1.9k, `kitty_graphics` 2.4k, `terminal_pane` 1.9k) | ✅ | ✅ | ✅ | ✅ | ✅ (`sixel::SixelImageStore`) | ✅ | ✅ (emits `CharacterChunk`s/raw VTE) |
| `ansi-to-tui`, `ansi-parser`, `ratatui`'s `Text` | MIT | small | parses *rendered text*, not a terminal | — | — | — | — | — | ❌ not applicable |

Two consequences worth internalising:

- **Nothing vendorable gives you item 2 except `vt100`.** If you move off `vt100` you inherit the job of writing the encoder — which is fine (it's ~300 LOC) but it must be scheduled.
- **Item 3 is unreachable through `vte` for kitty graphics**, because upstream `vte` discards `APC` payload bytes (`State::SosPmApcString => anywhere()`, `vte/src/lib.rs:184`). Sixel is reachable via `hook/put/unhook`; kitty graphics requires your own byte scanner or a patched parser.

---

## 5. Is vendoring an ANSI parser feasible? (the question as asked)

**Feasible, yes. Useful, mostly no.**

- `vte` is 4,159 LOC, two tiny deps (`memchr`, `arrayvec`), `Apache-2.0 OR MIT`, `no_std`-capable. Vendoring it is a copy-paste plus a `[patch.crates-io]`.
- But it is **already in your tree** transitively, and it is a *state machine*, not a terminal: it dispatches `print/execute/csi/esc/osc/hook/put/unhook` and holds no grid. Vendoring it changes nothing about fidelity.
- The gaps that hurt you live (a) in `vt100`'s **grid + missing DCS hooks**, and (b) in the fact that **you have no wire encoder**. Neither is fixed by owning a parser.

**If you want to vendor something, vendor the small, high-value pieces:**

| Candidate | LOC | Why |
|---|---|---|
| `tui-term`'s `vt100_imp.rs` (adapter) | 123 | Lets you drop `tui-term` and pin `vt100 0.18` — **but only together with `ratatui 0.30`**: 0.29 pins `unicode-width` exactly, which makes every `vt100` ≥ 0.16 unresolvable (Appendix C). Copy it into `src/app/ui/adapter.rs`, bump ratatui, delete `tui-term`. |
| `vt100` patch: forward `hook/put/unhook` to `Callbacks` | ~25 | Upstreamable; unlocks sixel/tmux passthrough with the engine you already have. Otherwise use the side-tap in §6.4. |
| alacritty's `input/keyboard.rs` (key → sequence, mode-aware) | ~600 | Only if you adopt `alacritty_terminal`. It is the best-tested implementation of application-cursor / keypad / kitty-keyboard encoding. |
| `alacritty_terminal` itself (dependency, not vendoring) | 11.7k | Apache-2.0 is fine for an MIT project with attribution/NOTICE, but note the licence asymmetry for a "vendored" copy. |
| `wezterm-term` (git dependency) | very large | Only if *images inside the grid* are a hard requirement. The build-time cost is real, and it is not on crates.io with these workspace deps. |

**Recommendation:** do not vendor an emulator. Use `vt100 0.18` as the engine, own ~300 LOC of encoder, and tap DCS/APC for graphics. That is a smaller, more debuggable delta than any emulator swap, and it is the same architecture `zellij` arrived at (own grid → emit ANSI) with 16k LOC instead of your 2k.

---

## 6. The shortest path

Stages are ordered by *dependency*, not by size, and each one is independently shippable. Effort assumes one developer familiar with the code.

### 6.1 S0 — "make it fast" — **shipped**, see Appendix C

1. **Drain-to-empty input.** Replace `if poll(0) { read() }` with `while poll(0)? { handle(read()?) }`, bounded by a per-tick budget (e.g. 512 events) to stay fair to rendering. → §2.2, ~20 LOC.
2. **Dirty-driven redraw.** Add `Arc<AtomicBool>` (or a `tokio::sync::Notify`) set by the reader threads when they process bytes; render only when dirty, or at a 60 Hz floor while an animation/`REP` is in flight, or on input. Replace `sleep(10)` with `select!` on `Notify` + a 16 ms timer. → §2.1, ~40 LOC.
3. **Enable bracketed paste on the host** (`EnableBracketedPaste`) and forward `Event::Paste(s)` as one `\x1b[200~…\x1b[201~` write when the child has `?2004h`, or as a raw bulk write otherwise. → paste goes from ~100 chars/s to instant, ~40 LOC.
4. **Unbounded/batched PTY writer.** Replace the 32-slot channel + `await` with an unbounded channel plus a coalescing writer task (join queued writes, one `write_all` + `flush` per wake-up); remove every `.unwrap()` on send. → §2.3, ~50 LOC.
5. **`vt100 0.15.2 → 0.18.1`** (see §5: vendor the 123-LOC adapter, drop `tui-term`), then **delete `owner.rs:203–260`** and read the modes you need from the screen: `mouse_protocol_mode()`, `mouse_protocol_encoding()`, `application_cursor()`, `bracketed_paste()`, `hide_cursor()`, `cursor_style()`. → §2.5, §3#5, ~+40/−60 LOC, plus the 0.18 API churn (`Parser::new` takes `NonZeroU16`; `set_size` semantics changed; `set_scrollback` moved to `Screen`).

**Exit criteria:** idle CPU ≈ 0 with a still screen; ≤2 ms keystroke→PTY; 5 KB paste lands in one write; `htop` inside the float responds to the mouse *offsets* correctly even if not yet fully routed (mode tracking is now real).

### 6.2 S1 — "make it correct, and give the child a voice" — **shipped**, see Appendix D

1. **One geometry source of truth.** Introduce `Overlay::content_rect() -> Rect` (rect minus border, and minus any off-screen part) and a single `resize_child(cols, rows)` that sets the PTY **and** `parser.screen_mut().set_size()`. Delete both `− 4`s (`tenant.rs:71–72`, `tenant.rs:107–108`) and stop printing `size.rows/cols` as if it were the child's size. On host resize, recompute `rect` and propagate. → §3#1, #6.
2. **Region-based input routing.** Hit-test first, *then* decide policy:
   - pointer **inside** `content_rect` → encode and forward to the float's PTY using **the float's own** `mouse_protocol_encoding()`/`mouse_protocol_mode()` (SGR / X10 / UTF-8), with coordinates translated into the child's space, and with motion filtered exactly like a real terminal does (`1003` any-motion vs `1002` button-motion). Scroll wheel → `64/65` in X10 or `CSI < 64;x;y M` in SGR.
   - pointer **outside** → uncl's own chords (drag/resize/move), consumed locally.
   - reserve one modifier (or `Home`, which is currently stolen and never forwardable — `keyboard.rs:18`) as the "local" escape hatch. → §3#2.
3. **Mode-aware key encoding.** `application_cursor()` → `SS3` (`\x1bOD`) for arrows/Home/End, `CSI 1;2D`-style for modifiers, `application_keypad()` for keypad, and honour bracketed paste. Best-effort `CSI > u` (kitty keyboard) is *not* worth doing at this stage — instead, do **not** advertise it (§6.2.5). → ~100 LOC, replaces the hand-rolled table in `keyboard.rs`.
4. **Reply channel = capability proxy.** uncl sits between the child and the real terminal, but the *real terminal's* capabilities currently never reach the child. Give both parsers a `Callbacks` impl that writes responses back into the corresponding PTY, answering from a capability record probed **once at startup from the host**:
   ```
   CSI c        → CSI ?64;1;2;6;9;15;18;21;22 c   (xterm-ish DA1, conservative: list only what you honour)
   CSI > c      → CSI >0;0;0 c                    (DA2: deliberately empty -- do not claim to be a VT level)
   CSI 6 n      → CSI row;col R                   (from screen.cursor_position())
   CSI ? 6 n    → CSI ? row;col R
   DCS $ q ...  → DCS 1 $ r <value> ST            (DECRQSS: only claim what you honour)
   DCS + q ...  → terminal capabilities + "4" if the host has sixel (XTGETTCAP / XTSMGRAPHICS)
   OSC 10/11;?  → OSC 10;rgb:…   / OSC 11;rgb:…   (host's real fg/bg, probed once and cached)
   OSC 4 ; n ; ?→ OSC 4;n;rgb:…                   (palette, best-effort)
   ```
   **The sixel bit is not optional if you want images (decision 2).** Most image emitters (`chafa`, `img2sixel`, `viu`, `libsixel` apps) choose their protocol by querying the terminal; if the query goes unanswered the child assumes "no images" and never emits one — S3 would then be dead code. Same class of bug for `XTGETTCAP`/`XTGETTCAP Smulx` and `OSC 4` queries: answer conservatively but *answer*.
   Do not advertise what you don't implement (kitty keyboard `CSI > u`, `XTGETTCAP` for unsupported caps): a conservative DA1 is far better than a lying one — a wrong "yes" produces garbage the child cannot detect.
   Also forward `OSC 52` from child to host clipboard (`vt100 0.16+` surfaces it as `Callbacks::copy_to_clipboard`) and answer `OSC 52;c;?` from the host clipboard if you enable that.
5. **Cursor fidelity.** `if screen.hide_cursor() { hide } else { show, and use cursor_style() to emit DECSCUSR }`. Also stop `RAPID_BLINK`-ing the owner cursor. → §3#4.
6. **Focus/paste passthrough**: if the child enabled `?1004`, emit `CSI I`/`CSI O`. Paste: forward `\x1b[200~…\x1b[201~` when the child has `?2004h`, raw bytes otherwise — and **never** translate a paste into `KeyEvent`s (that is the current 100-events/s path in §2.2).
7. **One encoder table, one owner.** Today `keyboard.rs` hand-rolls ~250 lines of `vec![27, 91, …]` with no mode awareness. Replace it with `fn encode_key(key, modes, host) -> Option<Vec<u8>>` per pane (child's `application_cursor`/`application_keypad`/`bracketed_paste`, plus uncl's own local-chord table), and route *every* `crossterm::Event` through: `Event::Key` → encoder, `Event::Mouse` → hit-test then encoder or local handler, `Event::Paste` → bulk, `Event::Resize` → layout, `Event::FocusGained/Lost` → `CSI I`/`CSI O`. This single function is where decision 3 (mouse) and decision 4 (Windows) both land (§6.6).

**Exit criteria:** BTOP renders a stable full frame at the right size, is mouse-clickable inside the float (clicks, drags and wheel all land at the right cell *inside the child*), hides the cursor correctly, survives 20 consecutive float resizes with no smearing, and answers `CSI 6n` correctly enough that `CSI 6n`-using scripts do not stall. `nvim` + `lazygit` + `less` + `fzf` + `htop` all behave. *(This is the "everything except images" milestone: ~7 working days for S0+S1.)*

### 6.3 S2 — "own the emitter" — ~1 week, ~350 LOC, −4 dependencies — **shipped**, see Appendix E

Delete `ratatui`, `tui-term`, and the double-buffer. Write `src/app/render/mod.rs`:

```rust
// per frame, per pane (owner first, then float clipped to its content_rect)
fn draw_pane(out: &mut Vec<u8>, pane: &mut PaneFrame, screen: &vt100::Screen, origin: (u16, u16)) {
    let width = pane.rect.width;   // == the child's PTY width, enforced in S1
    // 1. rows: emit only changed rows, each preceded by CUP to (origin.1 + row, origin.0)
    for (row, seq) in screen.rows_diff(&pane.prev, 0, width).enumerate() {
        if !seq.is_empty() { out.extend_from_slice(format!("\x1b[{};{}H", origin.1+1+row, origin.0+1).as_bytes());
                              out.extend_from_slice(seq.as_bytes()); }
    }
    // 2. input modes (paste/mouse/CKM) must be *ours*, not the child's: uncl owns the host
    //    terminal, so never replay state_diff() blindly -- see the note below.
    // 3. cursor
    if !screen.hide_cursor() && focused { out.extend_from_slice(screen.cursor_state_formatted().as_bytes()); }
}
```

Notes that matter:

- `rows_diff(prev, start, width)` gives you a **free, tested, SGR-only** row encoder; keep `prev: vt100::Screen` (it's `Clone`, ~`rows*cols*32` B ≈ 60 KiB) so the diff is O(cells) memcmp, exactly like a real multiplexer.
- **Do not replay the child's `state_diff()`** (mouse mode, bracketed paste, DECCKM) onto the host terminal: those modes must remain uncl's own, or the host terminal starts sending SGR mouse reports that uncl then has to re-encode. Emit *our* modes only, and translate.
- Clipping: because the child's PTY is now sized to `content_rect`, every row is exactly `width` cells wide, so no horizontal clipping is needed. Vertical clipping is "iterate only visible rows". This is why §6.2.1 is a prerequisite.
- Paint order for a float: owner rows first (unclipped), then the float's rect (`CUP` + rows + optional background fill). No alpha, no partial overwrite — occlusion is total, which is the only case an ANSI encoder can handle anyway.
- Frame scheduler: coalesce notifications, cap at ~120 Hz, and force a full repaint on show/hide/resize/scrollback change. Keep the `Vec<u8>` per frame and do one `write_all` (plus one `flush`).

**Why this is the pivotal stage:** it removes the 100 Hz buffer diffing (§2.1), removes per-cell `Style` construction, drops the lock from the render path (clone-or-snapshot cells into your own `prev` buffer under a short lock), cuts dependencies, and — critically — gives you a place to put **out-of-band bytes after the grid flush**, which is the only way images can be drawn (§6.4).

### 6.4 S3 — "images, cheap tier" — ~3 days, ~300 LOC (decision 2) — **shipped**, see Appendix F

Goal: *an image the child emits shows up inside the float*. No decode, no scaling, no re-encoding, no alpha. Passthrough only.

1. **Tap DCS/APC.** Wrap the parse of each chunk:
   ```rust
   for seg in Splitter::new(chunk) {          // ~80 LOC: ESC P…ST, ESC _…ST, ESC \
       match seg {
           Seg::Bytes(b) => parser.process(b),                       // feed prefix first, so the cursor is current
           Seg::Dcs(p) | Seg::Apc(p) => graphics.on_payload(parser.screen_mut(), seg, p),
       }
   }
   ```
   For DCS, prefer patching `vt100` (~25 LOC: forward `hook/put/unhook` into `Callbacks`) — one parser, upstreamable. APC must be split by us regardless, because upstream `vte` discards it (§4.1).
2. **Placement store.** `struct Placement { rect: Rect, payload: Vec<u8>, proto: Proto }`. Record at the cursor cell; footprint from the payload header only — kitty: `a=T,f=…,s=<px w>,v=<px h>,c=<cols>,r=<rows>`; sixel: the DCS `"` raster attributes, else 1×1 cell per 6 pixels. Re-emit without understanding the pixels.
3. **Repaint after the grid flush** (S2 gives you the hook): re-emit every placement intersecting the frame unless a float covers it or the row scrolled away. Emit kitty placements **with the terminal's own clipping is not possible on plain xterm**, so instead use kitty's source-rect crop: `a=p,i=<id>,x=<px>,y=<px>,w=<px>,h=<px>` — the *terminal* crops, which is free and exact. Requires the original image to be transmitted once and then placed by id (`a=t` then `a=p`), which is the cheap path.
4. **Sixel degrades on purpose.** Sixel has no crop and no place-by-id: one image = one opaque blit at the cursor. Contract: **draw only if it fits inside the float's `content_rect`; otherwise skip it and log once.** That is what tmux effectively does, it costs ~0 LOC, and it satisfies "willing to compromise on quality". Do not implement sixel re-encoding in this stage.
5. **Hyperlinks (`OSC 8`) are a separate 1-day item, not part of S3.** `vt100` does not track them; the cheap version is state in your splitter (open/close + per-cell run id, ~60 LOC) and emitting `OSC 8` per run in the row encoder. Only worth doing if you actually want clickable links in the float — `alacritty_terminal` (S4) gives them for free, so **defer**.

**Explicit non-goals** (state them in the README so nobody files bugs): no image scaling, no sixel cropping, no animation (kitty `a=a`), no alpha/blending, images not clipped by the float border on Windows (see §6.6), no `iTerm2` inline images, no sixel on non-Unix.

### 6.5 What "cheap" buys, concretely

| Producer | Path through uncl | Fidelity after S3 |
|---|---|---|
| `chafa`, `viu`, `img2sixel` (sixel) | child bytes → DCS tap → re-emit | Exact **if it fits** the float; skipped if not. No scaling. |
| kitty-capable apps, `kitten icat` | child bytes → APC tap → re-emit + placement crop | Exact, croppable, survives occlusion by the float's own repaint order. |
| `feh`-style direct `/dev/tty` writes | bypasses the PTY | never supported (also unsupported by tmux) |
| anything requiring `TIOCGWINSZ` in pixels | we answer `TIOCGWINSZ` cells only (`portable-pty` limitation) | pixel-size queries return 0; most tools fall back to cell math |

The dependency order that actually matters here is S1 → S3, not S2 → S3: the tap can be added to today's `vt100` reader thread in an afternoon. But **without the capability probe in §6.2.4 nothing will ever emit an image**, so that probe is the real first step of "images".

### 6.6 Windows & Docker delta (decision 4)

**Docker:** a non-issue for rendering. `portable-pty` opens a normal pty; `TERM` inherits; `COLORTERM`/`TERM_PROGRAM` should be passed through explicitly (today only `TERM=xterm-256color` is set, `owner.rs:135`), and `SHELL` is already read from the env. Only caveat: `TERM` is hard-coded for the owner — if the host is `xterm-kitty`, the child believes it is not kitty and loses kitty features. Fix: copy the host's `TERM`/`COLORTERM`/`TERM_PROGRAM` and downgrade `TERM` only when the host is `screen`/`tmux` (where passthrough matters).

**Windows:** three separable issues, only one of which is real work.

1. **Output: fine, with one call.** S2 writes raw ANSI to stdout. Windows consoles need `ENABLE_VIRTUAL_TERMINAL_PROCESSING`; crossterm flips it *lazily* through its `ansi_support` module when its command API is used, and that module is public (`pub mod ansi_support`, `supports_ansi()`, present in the 0.27 line you pin) — call it once at startup and treat `false` as "uncl cannot run here" instead of emitting raw ESC into a legacy console.
2. **Input: the real work.** On Windows, crossterm's input backend produces `Event`s from console records rather than from VT sequences, so the child cannot simply be sent what arrives. Your `encode_key`/`encode_mouse` from §6.2.7 is already the right shape: it takes *semantic* events and produces VT sequences, so Windows needs no separate pipeline — but the mode-dependent branches (application cursor, X10 vs SGR mouse, kitty keyboard) must be exercised on Windows, because you cannot test them by accident. Put the encoder behind a small trait with a `#[cfg(test)]` fake, and keep the `cfg`-specific code to: raw-mode setup (crossterm does it), `tiocgwinsz` replacement (`crossterm::terminal::size()` already abstracts it — `owner.rs:355` uses it, `tenant.rs` uses `PtySize`), and mouse encoding tables (`1006` vs `1015` are not equally supported by ConPTY-era terminals).
3. **Images: off, and the reason is structural.** The child sits under uncl's *own* ConPTY, so it is one layer further removed than usual. **ConPTY v1** (the in-box implementation on Windows 10/11, and Windows Terminal ≤ 1.21) parses the child's VT into a text buffer and re-renders snapshots — it drops or reorders sequences it does not model, **swallows DCS payloads, and answers `DA1` itself, without the sixel attribute**. That means the child never even asks uncl for sixel: it asks ConPTY, is told no, and falls back to blocks. **ConPTY v2** (shipped with Windows Terminal 1.22+, also redistributable as `conpty.dll`) forwards the original VT bytes through, which is what made sixel work in WT 1.22 at all. Getting images on Windows therefore requires uncl to bundle and load the v2 `conpty.dll` — a packaging change well outside "cheap". Decision: **Windows = no images**, documented, and `uncl doctor` says why. (Consequence for §6.2.4: on Windows do *not* advertise sixel in `XTSMGRAPHICS`/`DA1`, or you will advertise something your PTY layer destroys — the capability record is per-platform, not global.)

Sources for the ConPTY v1/v2 distinction: Windows Terminal 1.22 release notes (`https://devblogs.microsoft.com/commandline/windows-terminal-preview-1-22-release/`), the conpty-v2 passthrough discussion (`https://github.com/microsoft/terminal/discussions/18500`), and a field report of in-box ConPTY answering DA1 without sixel and swallowing sixel DCS (`https://github.com/openwong2kim/wmux/issues/1932`).

**Also worth doing while in here:** a `--check` / `uncl doctor` that prints the probed capability record (host DA1/DA2, `TERM`, `COLORTERM`, sixel yes/no, kitty yes/no). It is 30 lines, and it is the difference between "btop looks wrong" and a bug report you can reproduce.

### 6.7 Sequencing summary (revised for the decisions)

| Stage | Effort | Net LOC | Unlocks | Risk |
|---|---|---|---|---|
| S0 speed | 2–3 d | ~±200 | Idle CPU → ~0; input 1 event/tick → unbounded; paste instant; real mode tracking; `vt100 0.18.1` via the vendored adapter | Low. `vt100` 0.18 API churn is contained in the 123-LOC adapter; benchmark it against 0.15.2 (see the criterion bench in §8) and keep the pin reversible. |
| S1 correctness + input + capability proxy | ~5 d | ~+700 | BTOP/htop/nvim/lazygit render at the right size **and are mouse-interactive**; paste; query replies; sixel capability advertised (prerequisite for any image ever appearing) | Medium — the encoder branches per mode and per platform; mitigated by the encoder trait and a golden-sequence table. |
| S2 own emitter — **shipped** | ~1 wk | ~+1,200/−350, −4 crates | Native-speed scan-out; a hook for out-of-band bytes; no per-cell `Style`; `ratatui`/`tui-term` gone | Medium — off-by-one/clipping bugs are visible; mitigate with the §8.1 golden-frame harness. **Load-bearing: do not defer behind S3.** |
| S3 cheap-tier images — **shipped** | ~3 d | ~+450 | kitty images (croppable), sixel (if it fits), passthrough | Medium-low, because the quality contract is explicit (§6.4 non-goals). |
| S3.5 hyperlinks (`OSC 8`) | 1 d | ~+60 | clickable links inside the float | Low; or free with S4. |
| S4 engine swap (`alacritty_terminal`) | 3–5 d | ~+600 | underline colour/styles, hyperlinks, kitty-keyboard input, sync output, `EventListener` (title/clipboard/bell) | Medium; **requires S2** (you must own the encoder before leaving `vt100`). Optional — skip unless users ask for the fidelity *inside* a float. |
| §6.6 Windows delta | 1–2 d | ~+120 | Windows build that runs a TUI float (no images) | Medium; needs a real Windows box or CI runner to validate input modes. |

**Critical path: S0 → S1 → S2 → S3** (≈16 working days, ≈2,300 net LOC, −4 crates net since `tui-term` and `ratatui` go away). S3.5/S4/Windows are independent tails that can be scheduled by demand. The single highest-risk item is **S2's clipping/repaint correctness**, not the parser or the graphics.


---

## 7. Things that look like short paths and are not

- **"Just write the child's bytes straight to stdout and overlay the float."** Cursor addressing, clipping, scroll regions and occlusion make this a full sequence *rewriter* (harder than a grid). Only works if the child owns the whole screen — which is Appendix B, and only there.
- **"Vendor a big emulator and be done."** You still have no grid → ANSI encoder; `alacritty_terminal` and `wezterm-term` both render to GPU surfaces. You'd swap ~50 lines of parser code for ~1,000 lines of encoder plus a large dependency tree.
- **"Keep `ratatui` and smuggle images/hyperlinks through".** You can put raw `OSC 8` in a cell's symbol and paint images after `flush()`, but `ratatui`'s diff engine will silently erase them on the next frame unless you poison the buffer and force full repaints. This fights the framework continuously and is *more* code than S2.
- **"Raise the tick rate."** Makes the CPU problem worse and does nothing for input batching (§2.2).
- **"Patch `tui-term`."** It's 1,200 LOC of `ratatui` widget plumbing for something uncl uses ~40 lines of; vendoring the 123-LOC adapter and deleting the dependency is strictly better.

---

## 8. How to know it works (test strategy)

1. **Golden-frame harness (do this first, it pays for itself in S1/S2).** A `tests/` binary that: feeds a recorded byte stream into the engine at a fixed size, renders through *uncl's* emitter into an `Vec<u8>`, then replays that output into a fresh `vt100::Parser` and compares `contents_formatted()` with a direct render of the source stream. Two engines, one assertion, no terminal needed. This is how you catch clipping/width/wide-char regressions without a human watching.
2. **Sequence corpus**, checked into `tests/corpus/`: `?1049h/l`, `?1000/1002/1003/1006/1015`, `?2004`, `?1004`, `?1h`, `?7l`, `CSI 6n`, `CSI c`, `DCS $q`, `REP`, `DECSCUSR`, wide/CJK/emoji/VS16, combining marks, `OSC 8`, a small sixel and a small kitty image, plus `tmux`'s passthrough wrapper.
3. **Per-app smoke list:** `btop`, `htop`, `nvim` (+ mouse drag/right-click, your issue #6), `lazygit`, `less`, `fzf`, `tig`, `ncdu`, `sixel-tools`/`chafa -f kitty`, `zsh` with a heavy prompt.
4. **Input fuzz:** random `KeyEvent`/`MouseEvent` streams against each mode combination, asserting the emitted bytes for a table of known-good sequences.
5. **Perf guard rails in CI:** `perf stat`-independent counters — frames drawn per second at idle (should be 0), bytes written per frame, and a criterion bench for `process(chunk) + emit_frame()` at 200×60.

---

## 9. Decisions taken, and what is still open

Recorded (see §0.1): floating is mandatory; images cheap-tier; mouse required; Windows/Docker nice-to-have; `vt100` upgrade via adapter. The plan in §6 is built on those.

Still genuinely open, in the order they will bite:

1. **Local chords vs. the child's input.** With mouse required and `Home` the only chord, the float will fight every full-screen app for events. Proposal: `Home` toggles visibility (keep), `Alt`+drag (or a dedicated 1-cell resize gutter drawn on the border) for float geometry, and everything inside `content_rect` goes to the child. Needs your sign-off because it is a UX rule, not a bug fix.
2. **When the float is hidden, where does input go?** Today it goes to the owner shell and the float freezes but keeps parsing. Consider *always* routing keys to the float and requiring `Home` to reach the owner shell — a monoplexer with one toggle. Cheap to change, changes the feel.
3. **Scrollback.** `vt100` has it, the float throws it away (`Parser::new(..., 0)`, `lease.rs:24`). Do you want wheel-scroll inside a *shell* float (mouse wheel is required by decision 3 anyway)? If yes, budget 1 day in S1 and note that `vt100 0.18` changed `set_size`/`set_scrollback` semantics.
4. **`TERM` policy.** Copy the host's `TERM`/`COLORTERM` through (needed for kitty/sixel capability probing, §6.6) or keep the hard-coded `xterm-256color` and let the capability proxy translate? The former is more faithful, the latter is more portable. Recommendation: copy through, downgrade only under `screen`/`tmux`.
5. **Windows validation hardware.** §6.6 needs a real Windows box (or a CI runner) for the input-mode matrix. Without one, ship Windows as "experimental, no images" and keep the encoder trait testable.

## 10. Implementation checklist (drop-in order)

Each line is a reviewable PR; the sequence is the shortest dependency-satisfying one.
S0 (items 1–5), S1 (items 6–10), S2 (items 11–13) and S3 (items 15–16) are implemented —
see Appendices C, D, E and F for the file maps and the verification status. Item 14 (the
golden-frame corpus in CI) is the one S2 item still open, and S3 deliberately skips item
17 (`OSC 8` hyperlinks, still an S3.5/S4 item).

```
S0  1. vendor adapter (src/app/ui/adapter.rs) + vt100 0.18.1 + drop tui-term        [½ d]
    2. delete owner.rs:203–260; read modes from Screen                            [½ d]
    3. drain-to-empty input loop (budget 512/tick) + Select/Dir wrapper           [½ d]
    4. coalescing PTY writer (unbounded) + remove send().unwrap()                 [½ d]
    5. dirty flag + select!(notify, 16ms) + forced repaint on show/hide/resize    [1 d]
S1  6. Overlay::content_rect() as the only geometry; resize_child() sets pty+grid [½ d]
    7. hit-test → route; mouse encoder per child mode (X10/UTF-8/SGR, motion)     [1 d]
    8. key encoder trait + mode-aware arrows/keypad/paste; local chord table      [1 d]
    9. Callbacks: DA1/DA2/DSR/DECRQSS/OSC 10-11/XTGETTCAP + startup probe         [1½ d]
   10. cursor visibility + shape (DECSCUSR), focus in/out, `uncl doctor`          [1 d]
S2 11. src/app/render/: rows_diff → CUP+row bytes; prev screen per pane          [1½ d]
   12. clip by content_rect, paint order owner→float, full-repaint triggers        [1½ d]
   13. delete ratatui/tui-term, drop the double buffer, one write_all per frame   [1 d]
   14. golden-frame harness + corpus in CI                                        [1 d]
S3 15. DCS/APC splitter + placement store + re-emit after grid flush             [1½ d]
   16. kitty place-by-id + source-rect crop; sixel fit-or-skip                    [1 d]
   17. (opt) OSC 8 hyperlink runs                                                 [1 d]
W   18. Windows: ansi_support call, encoder cfg matrix, close the images gap      [1½ d]
```

`~20 development days` end to end with buffer; `16` for S0–S3 alone.

## Appendix A — candidate-crate facts used above

| Crate | Version inspected | LOC | Key API | Note |
|---|---|---|---|---|
| `vt100` | 0.18.1 (2026-10-19) | 5,111 | `Screen::{rows_diff, rows_formatted, contents_diff, contents_formatted, state_diff, input_mode_formatted, cursor_state_formatted}`, `Callbacks::{unhandled_csi, unhandled_osc, unhandled_escape, copy_to_clipboard, paste_from_clipboard, set_window_title, resize, on_scroll}`, `Screen::{mouse_protocol_mode, mouse_protocol_encoding, application_cursor, bracketed_paste, hide_cursor, cursor_style}` | No `hook/put/unhook` ⇒ DCS payloads dropped. `Screen: Clone`. 0.18 requires `NonZeroU16` sizes. |
| `vte` | 0.15.0 | 4,159 | `Perform::{print, execute, hook, put, unhook, osc_dispatch, csi_dispatch, esc_dispatch}` | `State::SosPmApcString => anywhere()` discards APC bytes (`src/lib.rs:184`) ⇒ kitty graphics invisible to every `vte` consumer. |
| `tui-term` | 0.3.4 | 1,574 (adapter: 123) | `impl Screen for vt100::Screen`, `Cell::apply` | 0.3.4 requires `vt100 = 0.16.2`, `ratatui-core 0.1`/`ratatui-widgets 0.3` (ratatui 0.30.2, released 2026-06-19). |
| `alacritty_terminal` | 0.26.1-dev | 11,728 | `Term::{renderable_content, grid, mode}`, `TermMode` (incl. `KITTY_KEYBOARD_PROTOCOL`), `RenderableContent{display_iter, cursor, colors, mode}` | Apache-2.0. No sixel/graphics anywhere in the repo. Key *encoding* lives in the binary (`alacritty/src/input/keyboard.rs`), not the library. |
| `wezterm-term` | 0.1.0 | large | `Terminal` + `Image`/`ImageDataType`, `terminalstate/sixel.rs`, `image.rs` | MIT, git-only workspace crate with `termwiz`/`image`/`terminfo` in tow. |
| `zellij` panes | HEAD | ~16k | `grid::render → (CharacterChunk, raw_vte_output, SixelImageChunk, KittyImageChunk)`, `kitty_graphics/{store,replies,grid_state}`, `sixel::SixelImageStore`, `hyperlink_tracker` | MIT reference implementation of exactly the architecture recommended here. |

## Appendix B — passthrough takeover (deferred by decision 1)

v1 of this report proposed promoting the float to full screen whenever its child entered the alternate screen and relaying the child's bytes verbatim. It is the cheapest possible route to byte-exact fidelity (no parser, no encoder, no fidelity ceiling, images included), and it stays on the shelf for two narrower uses:

- **`uncl --maximized` / a user-invoked "maximize this pane"** for image-heavy or kitty-keyboard-heavy apps, where the user explicitly wants the float to step aside. Same code, opt-in.
- **A debugging switch** (`UNCL_PASSTHROUGH=1`) to bisect "is this uncl's compositor or the child?" in one run.

Mechanics, if it is ever picked up: detect `?1049h`/`?47h` on the child (`vt100` tracks alt-screen internally; `Screen` exposes it via the mode set), set `raw` relay for the affected pane, stop compositing it, and splice the child's bytes into the frame between a `CUP` to the origin and the exit sequence. `?1049l` restores. ~250 LOC, no emulation work — but it *removes* the float while it is active, so it cannot be the default (decision 1).

## Appendix C — S0 implementation log (2026-10-26)

### What landed

| S0 item | Change | Where |
|---|---|---|
| 1 | `vt100` pinned to 0.18.1; its cell→buffer mapping vendored (123 LOC, MIT); `PseudoTerminal` reimplemented against `ratatui::Buffer`; `tui-term` removed. **This forced `ratatui` 0.29 → 0.30.2 as well** — see "the pin that blocked S0" below | `Cargo.toml`, `src/app/ui/adapter.rs`, `src/app/ui/widget.rs` |
| 2 | The byte-sniffing block is **deleted**. Mouse gating, bracketed paste and cursor visibility are read from the parser (`mouse_protocol_mode()`, `bracketed_paste()`, `hide_cursor()`), so a sequence split across reads can no longer desynchronise anything | `owner.rs::forward_mouse`, `owner.rs::forward_paste`, `widget.rs` |
| 3 | Input moved to a dedicated blocking pump thread; the loop drains up to 512 queued events per iteration instead of one per 10 ms | `owner.rs::spawn_input_pump`, `owner.rs::handle_event` |
| 4 | One coalescing writer per pane: unbounded channel, up to 64 KiB joined into a single `write_all` + `flush`; every `send().await.unwrap()` is gone | `src/app/pty.rs` (new) |
| 5 | Frames are driven by a sticky `Signal` raised by the reader threads; the old `sleep(10)+draw` pair is gone; a 100 ms housekeeping tick exists only to notice child exit; show/hide/resize/keys request an explicit repaint | `src/app/signal.rs` (new), `owner.rs::run` |

Folded in because they were one-liners on the same code paths (and were on the bug list): the tenant no longer toggles raw mode or builds a throwaway `Terminal`/`Screen`; its grid is sized to the same number the PTY is told; `crossterm::terminal::size()` is no longer called per input event; the mouse resize path uses saturating arithmetic (it could panic on a tiny terminal); `KeyEventKind::Press` is enforced (Windows sent press+release, so every key fired twice); bracketed paste is enabled on the host and `Event::Paste` is forwarded as one write.

Also new: a shared `pty::spawn()` used by both panes (`src/app/pty.rs`), so owner and tenant can no longer drift apart in how they wire readers, writers, resizes and exit detection.

### The pin that blocked S0 (and corrects §5 of this report)

The first build failed to resolve:

```
error: failed to select a version for `unicode-width`
  required by package `vt100 v0.18.1`        (^0.2.2)
  previously selected `unicode-width v0.2.0` by `ratatui v0.29.0` (=0.2.0)
```

`ratatui 0.29` **exactly pins** `unicode-width = "=0.2.0"` (upstream does this deliberately, see ratatui issue #1271), while every `vt100` ≥ 0.16 requires `unicode-width = "0.2.1"` (0.16.2) or `"0.2.2"` (0.17+). The two ranges are disjoint, and because 0.2.x is semver-compatible cargo will not keep both copies — it errors instead.

Consequences, all of which were verified against the crates' own manifests:

- The vendored adapter alone (as §5 of this report originally recommended) does **not** make `vt100 0.18` reachable; the UI toolkit has to move too.
- No `vt100` from 0.16 to 0.18 works with `ratatui 0.29`. The only way to keep ratatui 0.29 is `vt100 0.15.2` — i.e. giving up callbacks and mode getters, which was the entire point of S0.
- `ratatui 0.30` relaxed the pin to `unicode-width = ">=0.2.0"`, so **0.30.x + vt100 0.18.x** resolves.

Fix applied: `ratatui = "0.30.2"` (and `crossterm = "0.29"` to match the version ratatui's own backend uses, avoiding two crossterm copies in the tree). The only source change 0.30 required was the block title: `title_position(Bottom)` + `title_alignment(Right)` became `title_bottom(Line::…right_aligned())`. Everything else we touch (`Buffer::{empty, cell, cell_mut, area}`, `Cell::{symbol, set_symbol, style, set_style}`, `Style::{new, reset, patch}`, `Frame::render_widget`, `Terminal::{new, clear, draw}`, `widgets::{Block, Borders, BorderType, Clear, Widget}`, `backend::CrosstermBackend`) is unchanged in path and signature.

Two side effects worth recording:

- **MSRV is now 1.88** (ratatui 0.30's `rust-version`); `vt100` needs 1.85 and the crate itself is edition 2024. `rustup update stable` covers it.
- This is a bump we plan to delete: S2 removes `ratatui`/`crossterm` from the tree entirely (§6.3), which also removes the whole `unicode-width` constraint class. It is plumbing, not architecture.

An alternative we did *not* take: keeping `ratatui 0.29` + `vt100 0.15.2` and running a second `vte` parser in parallel to recover mouse/paste/cursor modes (~120 LOC). It avoids the dependency bump but keeps every 0.17/0.18 parser fix out — including the xterm-compatible `set_size` that the float needs most, since it is resized constantly.

### Deliberately *not* in S0 (they are S1)

- One geometry source of truth (the float still gives its child `rect − 4` while painting `rect − 2`).
- Region-based input routing (a visible float still swallows every mouse event).
- Terminal-query replies / capability proxy.
- Scrollback for the float.

### Verification status — read this before trusting the diff

There is **no Rust toolchain in the environment this was written in** (crates.io, static.rust-lang.org and the Debian/Ubuntu archives are all outside the sandbox allowlist; `apt` has no working index). So:

- ✅ All 14 source files parse cleanly under tree-sitter-rust (no `ERROR`/missing nodes).
- ✅ Every external API call was checked against the actual sources of `vt100` 0.18.1, `crossterm` 0.29.0 and `ratatui` 0.30.2 (`Buffer::{cell, cell_mut, empty}`, `Style::patch`, `Cell::{symbol, style, set_symbol, set_style}`, `Frame::render_widget`, `Terminal::try_draw`'s autoresize, `KeyEventKind`, `Event::Paste`, `MouseEvent: Copy`, `Parser::new(NonZeroU16, …)`, `Screen::{mouse_protocol_mode, bracketed_paste, hide_cursor, cursor_position, cell}`, `Callbacks`).
- ❌ **Not compiled.** Expect a handful of borrow-checker or clippy nits on first build; the risky spots are the `select!` arm bodies (they call `&mut self` while the futures borrow only locals) and `pty::spawn`'s partial move of `PtyPair`.
- ⚠️ `Cargo.lock` was not regenerated (no cargo here): it still lists `tui-term`, `vt100 0.15.2` and `ratatui 0.29.0`. `cargo build` rewrites it; the graph change is `−tui-term −vt100 0.15 −vte 0.11 −ratatui 0.29 (−its ratatui-crossterm/crossterm 0.28 chain)`, `+vt100 0.18.1 +vte 0.15 +ratatui 0.30.2 (+ratatui-core/widgets/crossterm)`, and `unicode-width` moves 0.2.0 → 0.2.2.

First commands on a machine with a toolchain:

```sh
cargo build           # refreshes Cargo.lock, surfaces the compile nits
cargo test            # 13 new unit tests: widget/adapter, float geometry, Signal
cargo run             # then, in the shell: btop, nvim, lazgit, less, fzf
```

### What to measure once it runs (the §2 claims, now testable)

| Claim | Check | Expected |
|---|---|---|
| Idle cost was tick-driven (§2.1) | leave a prompt idle, `perf stat -e task-clock -p $(pgrep uncl) -- sleep 5` | ≈ 0, not the old 100 Hz redraw |
| Input was capped at 1 event/tick (§2.2) | paste 5 KB of text into the float | lands in one write, no character-by-character crawl |
| Paste/queue no longer stall the loop (§2.3) | hold a key down / drag while a child floods output | typing stays responsive, no freeze |
| Mode tracking is now real (§2.5) | `htop` in the float, then move the mouse in the *owner* area | no phantom reports when the child never enabled mouse mode |

### S0.1 — cursor and shutdown fixes (reported after first run)

Two regressions from S0, both fixed:

**1. No cursor at all.** S0 replaced `tui-term`'s cursor painting with
`buf_cell.set_style(buf_cell.style().patch(cursor.style))` where the style was
`Modifier::RAPID_BLINK` and nothing else. A blink modifier on a blank cell is
invisible, and ratatui hides the hardware cursor unless `Frame::set_cursor_position`
is called — so the result was no visible cursor in either pane.

Fix: stop painting a fake cursor entirely. The widget is now a pure grid painter
and the owner loop calls `Frame::set_cursor_position` at the *focused* pane's
cursor cell, so the real hardware cursor is used: it blinks with the user's own
blink rate, takes the shape their terminal is configured for, and disappears
when the child hides it (`hide_cursor()`, e.g. BTOP). One cursor, in the focused
pane; the unfocused pane has none, which is what a multiplexer should do.
`Frame::area()` guards against placing it off-screen.

This also removes a whole class of "cursor looks wrong" bugs that S1 would
otherwise inherit: shape (DECSCUSR) and visibility now come from the terminal and
the parser, not from our painting.

**2. Unclean exits / prompt not restored.** Three separate causes:

- The restore block ran only on the `Ok` path of `initialize_pty`, after
  `run()`; any `?` early return, or a panic, skipped it and left the terminal in
  raw mode on the alternate screen.
- Nothing ever killed the children. Their readers block in `read()` forever, so
  the blocking pool never drained and `#[tokio::main]`'s runtime shutdown could
  wait on them — the process hung *after* the UI was gone, with the terminal
  still owned by uncl.
- The blocking input pump had no stop condition: it polled every 50 ms forever.

Fix: a `TerminalGuard` whose `Drop` disables raw mode, mouse capture and
bracketed paste, leaves the alternate screen, shows the cursor and resets SGR —
declared right after the first state change so it runs on every exit path
including unwinding. `PtySession` now owns a `ChildKiller` and SIGHUPs its child
on drop (also when a float is replaced), which closes the pty and unblocks the
reader. The input pump checks an `AtomicBool`. And `main` builds its own runtime
so it can `shutdown_timeout(250ms)` instead of waiting indefinitely.

Also fixed while in there: the float's pty/parser/painted area now agree on one
number (`Overlay::content_rect()`, border-inset only). They previously disagreed
by two cells, and the cursor would have been placed at the wrong cell inside the
float. The widget also clips its area to the buffer, which is the same bug class
as the open "crash on exit fullscreen in wezterm (out of bounds)" issue.

### Known behaviours kept on purpose (so the diff stays reviewable)

- `Home` still toggles the float and is still not forwardable to the child (S1 gives it back and moves the toggle).
- Backspace still sends `0x08` (not `0x7f`) — unchanged from before, tracked in S1 with the mode-aware encoder.
- Mouse reports to the *owner* shell are still always SGR; the per-mode encoder is S1.
- **Clicks inside a visible float still go to drag/resize, not to the child.** This is S1's region routing and it needs the local-chord decision in §9.1; until then a float running `btop`/`htop`/`nvim` is keyboard-only.
- The float title shows the *child's* size (inside the border), which is now the size the pty was actually told.

## Appendix D — S1 implementation log (2026-10-26)

S1 is the "give the child a voice" stage of §6.2. The user-visible goal was narrow and
concrete: **a TUI running in the float must receive the mouse**, and everything else in
the float must behave like it is running in a real terminal rather than behind a wall.

### The bug that started it, and why it happened

Clicks and wheel events inside the float never reached the child. Two independent causes,
both in `mouse.rs`/`owner.rs`:

1. The float had no notion of *regions*. With the float visible, the old handler treated
   every mouse event as a candidate drag/resize, so presses inside the content area were
   consumed by uncl (`begin`/`drag`/`end`) and never encoded for the child.
2. Even if they had been encoded, uncl only knew one encoding — crossterm's host-side
   event — and forwarded nothing. The child's `mouse_protocol_mode()` /
   `mouse_protocol_encoding()` were read by nothing outside `keyboard.rs`'s dead code.

S1 replaces both with a single, testable decision function.

### What landed

| File | Change |
| --- | --- |
| `src/app/input/mouse.rs` | **rewritten.** `route(overlay, event) -> MouseRoute` is the whole policy: `Alt`+press → local, border → local resize, content → `Child{col,row}` (area-relative), outside → `HideFloat`. `begin/drag/end` own the drag state machine; `direction_for` picks the nearest corner/edge. 12 tests. |
| `src/app/input/encode.rs` | **new.** `ChildModes::from_screen`, `encode_key` (chars, Ctrl→control range, Alt prefix, DECCKM-aware arrows/Home/End, SS3 F1–F4, tilde keys, xterm modifier parameter), `encode_mouse` (SGR / legacy X10 / UTF-8, mode gating per `MouseProtocolMode`, 1-based coordinates, release handling). 16 tests. |
| `src/app/term/caps.rs` | **new.** Host capability record (`TERM`, `TERM_PROGRAM`, `COLORTERM`, kitty/sixel flags), `da1()`/`da2()`, xterm palette, `rgb_spec`, `apply_child_env`, `summary` for `uncl --doctor`. 4 tests. |
| `src/app/term/callbacks.rs` | **new.** `vt100::Callbacks` impl: DA1, DA2, DSR 5/6, `CSI ?6n`, `CSI 18t`, `OSC 10/11/4`; tracks `?1004` (vt100 does not model it); answers through a `ReplySink` installed on the pty writer. 8 tests. |
| `src/app/pty.rs` | `spawn` takes a `ReplySink` and installs it before the reader thread starts; new `ChildParser` alias. |
| `src/app/lease.rs` | Owns parser + sink + session; `modes()`, `sync_size()` (the single place that resizes pty **and** grid), `clamp_to` on host resize. |
| `src/app/ui/tenant.rs` | `content_rect` is the only geometry; `resize_to` clamps instead of bailing (a drag used to freeze when the requested size crossed `MIN_HEIGHT`); new `resize_to_corner` keeps the anchor corner fixed; `clamp_to`; `geometry_key` (Alt+arrows move, Alt+Shift+arrows resize). 14 tests. |
| `src/app/ui/owner.rs` | Chords → `encode_key`; mouse → `route` → `encode_mouse`; paste wrapped per `?2004`; focus reports per `?1004`; `HostMouse` policy (below); `Ctrl+Alt+q` quits. |
| `src/main.rs` | `--doctor`, `--version`, `--help`; `uncl` with no arguments still just runs. |
| `src/app/input/keyboard.rs` | **deleted** — its hand-rolled `vec![27, 91, …]` table is what `encode_key` replaces. |

66 tests by `cargo test`'s count (S0 had 18). No new dependencies.

### Two policy decisions worth remembering

**`HostMouse`: the host mode is not the child's mode.** crossterm's `EnableMouseCapture`
turns on `?1000`, `?1002` *and* `?1003` — any-motion — so every pixel of pointer movement
becomes an event, is drained, and is then thrown away by the mode filter. uncl now writes
the DEC private modes itself: SGR encoding always (`?1006`), press always while the float
is visible (needed for drag and click-away), button-motion/any-motion **only** when the
focused child asked for them, and everything off when nobody wants it. That also restores
native host text selection (with the terminal's usual Shift bypass) when the child is not
a mouse-mode application.

**`TERM` stays `xterm-256color`.** `TERM` is not inherited from the host: until the DCS/APC
tap exists (S3), claiming `xterm-kitty` or a sixel `TERM` makes a child emit image bytes
that uncl drops silently. `COLORTERM=truecolor` *is* inherited (24-bit colour survives our
re-encoding), `TERM_PROGRAM=uncl` is set, and the host's program is preserved as
`UNCL_HOST_TERM_PROGRAM`. `uncl --doctor` prints exactly what a child will see.

### Closing the gaps the first run exposed

Three things were still lying to the child after the first green build:

**A gesture that leaves the float.** Press inside the float, drag outside: the
`Drag`/`Up` used to fall outside `content_rect`, so uncl called it "local", the child never
received the release, and a text selection in an editor stayed stuck. `Overlay::child_grab`
now records that the child owns the pointer from press to release; while it is set, every
button event routes to the child, with the coordinates **clamped to the grid** (which is
what a real terminal reports for a pointer outside the window). `begin()` refuses to start a
local drag during a grab, so `Alt` cannot strand a drag either. Scroll is deliberately *not*
captured — it is an independent event, not part of the gesture.

**Clipboard (`OSC 52`).** A child copying to the clipboard was silently dropped: the request
is not for the child's terminal, it is for the *real* one. `Callbacks::copy_to_clipboard`
(vt100 validates the selector and the base64 payload) now queues `OSC 52 ; <ty> ; <data>`
on a host channel, and the owner thread — the only writer of stdout — emits it before the
next frame. Reading the host clipboard (`OSC 52 ; c ; ?`) is still unanswered on purpose:
almost no terminal allows it, and a wrong answer is worse than a child falling back.

**Cursor shape (`DECSCUSR`).** Hidden/shown and position were already honoured, but not the
shape, so nvim's insert-mode beam was a block in the float. The focused pane's
`screen.cursor_style()` is mapped to `CSI 0..6 SP q` and re-sent only when it changes; the
`TerminalGuard` resets it on exit, because the host's cursor shape outlives the alternate
screen.

**A crashing float no longer takes the owner with it.** The float's shell exiting (`exit` in
the float, a crash) respawns the lease; a *failing* `exec` is now backed off by
`TENANT_RESPAWN_BACKOFF` (2 s) instead of retried every 100 ms tick, and — the important
part — an error there no longer propagates out of the event loop, which used to tear down
the owner shell and exit uncl. The float also comes back **visible** if it was visible when
it died, so `exit` in the float gives a new prompt rather than a window that has to be
re-opened with `Home`.

### Deliberately *not* in S1

* **DECRQSS / XTGETTCAP / kitty keyboard (`CSI > u`) / application keypad.** Answering
  these badly is worse than not answering: a wrong "yes" makes a child emit sequences we
  cannot honour and the child cannot detect the failure. They belong with the S2 emitter,
  which is where the answers become true.
* **Cursor *shape* (`DECSCUSR`).** Hidden/shown and position are honoured; shape is not
  relayed. Visible cursor in the right cell was the bug; shape is cosmetic.
* **Sixel/kitty graphics.** Unchanged from S0: gated behind `UNCL_SIXEL` until the image
  tap exists (§6.4).
* **Owner-pane scrollback / alt-screen semantics.** Untouched.

### Verification status — read this before trusting the diff

There is **no Rust toolchain in this environment** (installs are blocked), so this code has
not been compiled or run. What was done instead:

* every file parses cleanly with `tree-sitter-rust`;
* every method name called in `src/` was cross-checked against a definition in `src/` or in
  the vendored sources of the exact dependency versions (`ratatui 0.30.2`, `crossterm 0.29`,
  `vt100 0.18.1`);
* the `vt100` callback signatures, `Parser<CB>` API, and *which* sequences reach
  `unhandled_csi`/`unhandled_osc` were read out of `vt100 0.18.1`'s own `perform.rs`
  (`?h`/`?l` with unknown modes, `c`, `>c`, `n`, and `18t` all land there; `8t` does not);
* geometry, encoders and routing are covered by the 66 tests above.

First thing to run on a machine with cargo: `cargo test`, then the manual script below.

### Manual acceptance script (S1 exit criteria, §6.2)

```sh
cargo build --release
./target/release/uncl --doctor          # TERM/COLORTERM a child will see
# in the float (Home):
echo $TERM; echo $COLORTERM             # xterm-256color / truecolor
printf '\e[6n'; timeout 1 cat -v        # CPR: expect ^[[<row>;<col>R, and no stall
btop                                    # click CPU/mem panes, wheel in the process list
htop; nvim; lazygit; less /etc/services; fzf
# select text in nvim/less and drag the pointer out of the float → selection keeps
#   following the pointer and releases cleanly (no stuck selection)
# nvim: `i` shows a beam cursor, `Esc` a block (DECSCUSR relayed to the host)
# `echo -n hello | base64 | xargs -I{} printf '\e]52;c;{}\e\\'` → pastes from the
#   host clipboard (terminal must allow OSC 52 writes)
# geometry: Alt+drag to move, drag the border to resize (child must reflow live)
# Alt+arrows move, Alt+Shift+arrows resize
# click outside the float → it hides; Home → it comes back
# `exit` inside the float → a fresh prompt, and the owner shell is untouched
# mouse outside the float (float hidden) → the owner shell sees nothing, selection is native
# drag the bottom edge up past MIN_HEIGHT → the float keeps tracking the pointer
# (`Ctrl+Alt+q` is the escape hatch if anything wedges)
```

---

## Appendix E — S2 implementation log (2026-10-26)

S2 is what §6.3 called "the pivotal stage". The UI toolkit is gone: a frame is now
`vt100::Screen` → bytes, encoded by uncl, written with a single `write_all`.

### What landed

| # | Change | File |
|---|---|---|
| 1 | The frame emitter: per-pane `prev` screens, owner row diffs, float rows painted into `content_rect`, borders, title, cursor last, one buffer per frame | `src/app/render/mod.rs` (new, ~1,020 LOC incl. 17 tests) |
| 2 | The only rectangle type in the tree (`new/right/bottom/is_empty/contains/inset`) | `src/geometry.rs` (new, ~100 LOC incl. 5 tests) |
| 3 | One definition of the float's inner area, shared by the mouse router, the pty sizing and the painter | `src/constants.rs` (`content_rect`) |
| 4 | `Terminal`/`Frame`/`CrosstermBackend` deleted from the paint path: `draw()` builds `Panes` and does one `write_all` + `flush`; the event loop got a frame deadline (`FRAME_MIN_INTERVAL`, 8 ms ≈ 120 Hz) instead of a 100 ms ticker; `Renderer::invalidate()` on show/hide, move, resize (keyboard and mouse), respawn, and `EnterAlternateScreen` | `src/app/ui/owner.rs` |
| 5 | `PseudoTerminal` widget and the `vt100→ratatui` adapter deleted (`ratatui` + `tui-term` + their 40-odd transitive crates) | `src/app/ui/widget.rs`, `src/app/ui/adapter.rs` deleted; `Cargo.toml` |

**83 tests** as `cargo test` counts them (S1's tree had 66: −5 with the deleted widget,
+5 geometry, +17 renderer).
**No new dependencies.** Net diff: **+1,205 / −346** across 13 files.

### What a frame does now

1. The loop wakes for a reader signal, an input event, or the frame deadline. Frames
   are coalesced: a child flooding output cannot make uncl repaint more than ~120×/s.
2. Both screens are cloned **under their locks** — one clone per pane, ~32 B/cell
   (~375 KiB at 200×60, ≈0.1 ms of `memcpy`). The lock is never held while a frame is
   encoded, which is the §2.4 fix S0 introduced and S2 keeps.
3. The snapshots are handed to `Renderer::render(Panes { .. })` **by value**, so the
   renderer keeps them as the next frame's `prev`. One clone per pane per frame, not two.
4. Nothing changed → `render` returns an **empty slice** and `draw` performs no syscall.
5. Otherwise: `?25l` → changed owner rows (`rows_diff`, or `rows_formatted` + `ECH(width)`
   on a full repaint) → the float's rows, each preceded by a `CUP` into its content rect
   → the float's top/bottom border and `s:rows:cols` title if the rect is new or was
   overpainted → one `CUP` + `?25h` for the focused child's cursor → one `write_all` + `flush`.

Input modes are still **never** replayed from the child's screen: `HostMouse` and
bracketed paste remain uncl's own, as decided in S1.

### Three laws that the §6.3 sketch did not have (and why it took 2× the LOC)

1. **`vt100`'s row encoders are pane-local.** `rows_formatted`/`rows_diff` emit
   `CSI r;cH` for backwards jumps within a row, addressed as if the pane were at the
   screen origin — and for that origin itself it emits a bare `CSI H`, whose parameters
   default to 1;1. `append_row` rewrites every absolute move (bare, empty-field or
   explicit, `H` or `f`) to screen coordinates and leaves every other CSI alone.
2. **`EL` inside a float is a weapon.** A styled-but-empty row tail (a coloured status
   bar — BTOP's entire chrome) ends in `CSI K`, which on a real terminal erases to the
   end of the *screen*: the float's right border, the owner's cells beside it, and the
   frame gutter. So the right border is painted **after** the row, and a row that
   emitted `EL` triggers `repair_tail`, which repaints the owner's cells from
   `rect.right()` to the screen edge for that row.
3. **`rows_formatted` never blanks.** It skips default cells and relies on what is
   already on screen, so a *full* repaint must `ECH(width)` first — bounded to the
   float's width, so it can never reach the border. That is exactly the difference
   between `FloatRow::Full` and `FloatRow::Diff`: `rows_diff` blanks by itself, so diff
   rows are painted without the erase and cost one `CUP` plus the changed cells.

The float's dirty-row plan is therefore three-way — **skip** (unchanged), **diff**
(changed), **full** (new, or the owner just repainted that row *under* the float).
Occlusion is what forces the third case: the owner's row diff cannot know the float was
sitting on top of it.

### Deviations from the §6.3 sketch (each deliberate)

- **`cursor_state_formatted()` is not used.** It carries no SGR state (so it corrupts
  attribute state mid-frame) and it expresses a *shape*, not a position. The real
  cursor is placed with `CUP` + `?25h` while painting stays cursor-hidden, and the
  shape is relayed out of band by S1's `decscusr()` — which is also what makes nvim's
  beam a beam instead of a painted rectangle.
- **`state_diff()` is still not replayed** (S0/S1 policy): uncl owns the host's modes.
- **The scheduler lives in the event loop**, not the renderer. `Renderer` is pure:
  bytes in, bytes out, no timers, no I/O — which is what makes the 17 tests possible.
- **The border and title are the renderer's job** now that `Block` is gone, and the
  title shows the child's *content* size (the number the pty was given), not the rect.
- **`Panes` owns its screens** instead of borrowing them, to keep the clone count at one.

### What this removes from the hot path

| Before (S1) | After (S2) |
|---|---|
| 100 Hz unconditional full-screen repaint | repaint on change only, coalesced to ≤ ~120 Hz, empty frame = 0 syscalls |
| `vt100` grid → `Buffer` of ratatui cells (a second `w*h` copy, per frame) | direct row encoding from the parser's grid |
| per-cell `Style` construction, then a full-buffer diff | per-**row** `memcmp` (`Cell: PartialEq` compares attributes too), then `vt100`'s tested SGR diff |
| one `write` per changed cell run (crossterm backend) | one `write_all` per frame |
| cursor emulated as a styled cell | real cursor, hidden during paint, placed once, child's hide honoured |

### Deliberately *not* in S2

- **The golden-frame corpus** (§10 item 14). The 17 renderer tests assert byte-level
  properties (CUP targets, `EL` repair ordering, diff-vs-full, cursor placement) against
  parsers fed real escape sequences, but there is no recorded corpus of child streams
  replayed in CI yet.
- **A rect-scoped invalidation.** A move/resize invalidates the whole screen: correct,
  but it repaints all rows instead of the union of the old and new rects.
- **Owner scrollback and alt-screen semantics** — untouched, as in S1.
- **`Cargo.lock`.** The committed lock is stale (it still says `crossterm 0.27`,
  `vt100 0.15.2`, `ratatui`, `tui-term` — it was not regenerated in S0/S1 either, and
  cannot be here without cargo). The first `cargo build`/`cargo test` rewrites it;
  commit that refresh on its own so the code change stays reviewable.

### Verification status — read this before trusting the diff

Still **no Rust toolchain in this environment**, so S2 has not been compiled or run:

* every file parses cleanly with `tree-sitter-rust` (checked after every edit);
* every method called on `vt100`, `crossterm` and `portable-pty` exists in the vendored
  source of the exact pinned version — in particular `Screen::cell`/`rows_diff`/
  `rows_formatted`/`cursor_position`/`hide_cursor`, and `Cell: PartialEq` (which compares
  attributes, not just text — the row diff depends on that);
* the pane-local `CUP` and `EL` behaviours above were read out of `vt100 0.18.1`'s
  `row.rs`/`screen.rs`, not assumed;
* 83 tests: geometry (5), renderer (17), routing/encoders (32), callbacks (10),
  capabilities (4), float geometry/keyboard (11), signals (3), constants (1) — the
  whole S0/S1 suite plus the emitter.

First thing to run on a machine with cargo: `cargo test`, then the script below.

### Manual acceptance script (S2 exit criteria)

```sh
cargo test                                  # 83 tests, no warnings expected
cargo build --release && ./target/release/uncl

# 1. idle cost: leave the float hidden and the owner idle → `top -p $(pgrep uncl)`
#    must show ~0% CPU with a still screen (this was the §2.1 bug)
# 2. btop in the float, Home to hide it → the owner must repaint with no float ghosts
#    (no border remnants, no half-drawn panes)
# 3. Home → the float appears with its `│ ┌ ┐ └ ┘` border and `s:rows:cols` title, and
#    the title must match what `stty size` says inside the float
# 4. drag the float by its border, and Alt+arrows / Alt+Shift+arrows → the vacated area
#    is repainted; no smearing, no leftover fragments at the old position
# 5. a *still* btop must cost nothing: `strace -e trace=write -c -p $(pgrep uncl)`
#    for 5 s → a handful of writes, not thousands (empty frames write nothing)
# 6. `seq 1 200000` in the float → no tearing, input stays responsive, bounded CPU
# 7. wheel-scroll a btop process list and a `less /etc/services`: rows scroll smoothly
#    and the float's border survives the scroll (the `EL` repair)
# 8. nvim inside the float: cursor blinks in the right cell; `i` = beam, `Esc` = block
# 9. resize the host window while btop runs → both panes reflow live, nothing smears
# 10. click outside the float → it hides; Home → it comes back; `exit` in the float →
#     a fresh prompt, owner shell untouched (`Ctrl+Alt+q` if anything wedges)
```

### Where S2 leaves the plan

§6.3's claim that this is the pivotal stage holds up: the encoder is now uncl's own, so
`state_diff`, the mode table and — crucially — **out-of-band bytes after the grid flush**
now have a place to live. That is the prerequisite for S3 (kitty/sixel passthrough) and
for S4 (an `alacritty_terminal` swap) without touching the event loop again.

---

## Appendix F — S3 implementation log (2026-10-26)

S3 is "images, cheap tier" (§6.4): **an image the child emits shows up inside the
float**, with no decode, no scale and no re-encode. It does now — for the two
protocols that matter on a terminal that is not a sixel terminal.

### What landed

| # | Change | File |
|---|---|---|
| 1 | The tap: a streaming splitter that removes `DCS`/`APC`/`OSC` strings from a child's output *before* `vt100` sees it, across chunk boundaries, and hands everything else back | `src/app/images.rs` (new, ~700 LOC incl. 19 tests) |
| 2 | The store: classification (kitty / sixel / iTerm2), kitty control-block parsing, chunk joining (`m=1`…`m=0`), a placement-only replay by image id, bounds on count and bytes | `src/app/images.rs` |
| 3 | The reader thread feeds the splitter, passes graphics to the store anchored at the child's cursor, and answers kitty's `a=q` capability query from the child's own pty | `src/app/pty.rs` |
| 4 | `Lease` owns the float's store (a restarted child gets a fresh one) | `src/app/lease.rs`, `src/app/ui/tenant.rs` |
| 5 | Emission **after the grid flush**: new images, re-placement on a move, re-blit after a repaint, and `a=d,d=A` when the float hides | `src/app/render/mod.rs` (6 new tests) |
| 6 | `TERM`-level advertising: `UNCL_GRAPHICS=kitty,iterm2` for children, `--doctor` says what the tap carries | `src/app/term/caps.rs` |
| 7 | Non-goals written down where users will find them | `readme.md` |

**117 tests** (S2 shipped 83: +23 tap/store, +6 renderer, +5 window reports and
cell size). No new dependencies.

### The five laws this stage is made of

1. **`vte` cannot tell us about pixels.** `vt100` does not implement
   `hook/put/unhook`, so a DCS payload is consumed and dropped *inside* the parser,
   and APC bytes go to `anywhere()`, which discards them. §6.4 offered patching
   `vt100` for DCS; splitting the stream ourselves instead handles DCS **and** APC
   **and** OSC 1337 in one place, with no fork of a dependency. The splitter is
   ~150 lines and has no version coupling.
2. **The splitter is protocol-agnostic, the store is not.** Out-of-band sequences
   are reported as `(kind, bytes)`; anything the store does not recognise comes
   straight back and is fed to the parser, so `OSC 52`, colour queries and `DECRQSS`
   behave exactly as they did in S1. That is what keeps this stage invisible to the
   rest of the terminal.
3. **A terminator can be split.** `ESC` at the end of one read and `\` at the start
   of the next is the normal case for a 4 KiB image on a pty, and treating the lone
   `ESC` as ordinary payload swallows the rest of the stream. `feed` checks the
   boundary explicitly (and a test covers it).
4. **A transmission is not an image.** kitty senders chunk payloads (`m=1`…`m=0`,
   4 KiB each), so a 100 KiB PNG arrives as ~25 sequences. Storing each as an image
   would both break the image and evict its first fragments, so chunks are joined
   into one entry anchored where the *first* one arrived.
5. **Two kinds of image, two kinds of lifetime.** kitty images live in the
   terminal's own image layer: painting text over them does not erase them, so a
   still frame costs nothing — but they have to be **deleted** (`a=d,d=A`) when the
   float hides, or they stay stuck over the owner's screen. sixel and iTerm2 draw
   into the same plane as text, so *any* repaint of their rows erases them and they
   have to be blitted again. The renderer's emission rule is exactly those three
   triggers: **new**, **moved**, **erased**.

### Emission rules (the whole policy)

| Trigger | What is sent |
|---|---|
| a new sequence | the payload, verbatim, at its translated anchor |
| the content rect changed (show, drag, resize) | `a=p,i=<id>…` for kitty (never the payload), the payload for the rest |
| a pixel-based image's rows were repainted | the payload again |
| the float hides | `ESC _ G a=d,d=A ESC \` — once |
| nothing changed | nothing at all |

### Deviations from §6.4

- **`vt100` is not patched** (law 1): one splitter for all three protocols instead
  of a DCS hook plus a separate APC splitter.
- **iTerm2 inline images are in scope**, though §6.4 listed them as a non-goal.
  Cost: ~40 lines on top of the same store. Reason: the terminals most likely to be
  hosting uncl (WezTerm, iTerm2, kitty) implement it, the review machine's terminal
  has **no sixel at all** (`uncl --doctor`: `sixel=false kitty_graphics=true`), and
  `wezterm imgcat` is the shortest path to a user seeing an image today.
- **No source-rect cropping.** §6.4's kitty crop (`x,y,w,h` in *pixels*) needs the
  cell size in pixels. We do have one (see `probe_cell_px`, below), but cropping
  would mean rewriting a sender's payload, and passthrough is the rule. An
  oversized image therefore overlaps the border instead of being cropped — a
  documented compromise, and the child's own crop parameters are still passed
  through verbatim.
- **The "does it fit" test is an estimate, not a measurement.** sixel only states
  its size in pixels, so the cell size has to be assumed: measured from the host
  tty at startup (`TIOCGWINSZ`), overridable with `UNCL_CELL_PX`, falling back to
  `10x20`. It only ever decides *skip*: the terminal does the drawing. Kitty's
  `c=`/`r=` are used when the sender provides them, and unknown footprints are
  drawn.
- **The capability probe is a reply, not a query.** Rather than interrogating the
  host at startup, the tap answers kitty's `a=q` from the child's own pty (that is
  the probe graphics-aware senders actually use) and advertises
  `UNCL_GRAPHICS=kitty,iterm2` in the child's environment. Sending sixel remains
  gated on `UNCL_SIXEL`, because a sixel the host cannot draw is garbage on screen,
  whereas an unknown APC/OSC is ignored by every terminal.

### Deliberately *not* in S3

- **Owner-pane images.** The background shell's graphics are stripped like every
  other child's, and never drawn. Claiming otherwise would be a lie: the owner pane
  is painted by the same row encoder as everything else.
- **Animation** (`a=a`), **frame edits** (`a=f`) beyond passthrough, **alpha**,
  **scaling**, **`OSC 8` hyperlinks** (that is S3.5, unchanged).
- **Scroll-aware placement.** An image is anchored to the cell it was emitted at
  and re-placed there; if the child scrolls its grid, the image stays put instead
  of travelling with the text. Doing it properly means tracking the grid's scroll
  deltas, which `vt100` does not expose — that is an S4 (`alacritty_terminal`)
  feature, not a cheap one.
- **Real cropping** (above). The cell size itself *is* probed now — see the S3.1
  section at the end of this appendix — but only for reporting and for the
  fit test.
- **A `--doctor` self-test** that emits a sample image; the acceptance script below
  does that in three lines instead.

### Verification status — read this before trusting the diff

Still **no Rust toolchain here**; S3 has not been compiled or run.

* every file parses with `tree-sitter-rust`;
* the `vt100`/`vte` behaviour the tap relies on was read from the vendored sources
  (`perform.rs`: `hook/put/unhook` unimplemented; `anywhere()` for APC);
* the kitty control-block grammar, `a=q` reply and `a=d,d=A` deletion were taken
  from kitty's own protocol description, and sixel's raster attributes from the
  `DECSDM`/sixel spec as implemented by `libsixel`/`chafa`;
* 117 tests, 34 of which are new here: splitter ordering, chunk boundaries, split
  terminators, C1 forms, classification per protocol, chunk joining, deletion,
  bounds, anchor translation, and the renderer's three emission triggers.

### Manual acceptance script (S3 exit criteria)

```sh
cargo test                       # 117 tests
cargo build --release && ./target/release/uncl

# 1. kitty graphics, no external tools needed: an 8x8 dark-red block, 8 cells wide.
#    Inside the float, paste this; a red rectangle must appear at the prompt cell.
python3 - <<'PY'
import base64, sys
w = h = 8
data = base64.b64encode(bytes([0x80, 0, 0]) * (w * h)).decode()
sys.stdout.write(f"\x1b_Gi=1,a=T,f=24,s={w},v={h},c=8,r=4;{data}\x1b\\")
PY
# 2. with real tools (whichever you have):
chafa --format=kitty -s 30x15 some.png      # kitty path
timg -pk some.png                           # kitty path
wezterm imgcat some.png                     # iTerm2 path
chafa --format=sixel -s 30x15 some.png      # sixel path (needs UNCL_SIXEL=1)

# 3. move the float (Alt+arrows, or drag the border) → the image follows the cell
#    it belongs to, and comes back unchanged. It must not be re-transmitted
#    (strace -e write -s 200 -p $(pgrep uncl) shows `a=p,i=…`, not the payload).
# 4. Home twice → the image disappears with the float and returns with it.
# 5. run the image command again in a scrolling TUI (e.g. `less some.png` via
#    chafa's sixel/kitty output) → no stale image left behind on the owner screen.
# 6. `uncl --doctor` → "image tap: kitty + iTerm2 always, sixel not advertised"
#    and a `cell size: WxH px (measured|assumed|UNCL_CELL_PX)` line.
# 7. `UNCL_SIXEL=1 uncl --doctor` → sixel advertised, and a full-screen sixel is
#    skipped once with "uncl: sixel image is larger than the float; skipping".
```

### S3.1 — `wezterm imgcat` panicked: the bug the first real image found

Reported from a WezTerm host, immediately after S3 shipped:

```
$ wezterm imgcat uncl.png
ERROR env_bootstrap > panic at wezterm/src/main.rs:383:10
attempt to divide by zero
  7: wezterm::ImgCatCommand::compute_image_cell_dimensions
```

S3 had made the *bytes* survive; it had not made the child believe it could draw
them. `imgcat` builds the iTerm2 sequence by measuring the terminal first
(`wezterm/src/main.rs` → `compute_image_cell_dimensions`):

1. `probe.screen_size()` writes `CSI 18 t` (text area in **cells**) and `CSI 16 t`
   (one cell in **pixels**), then `CSI c` (primary DA) to end the read;
2. it divides the image's pixel size by `term_size.xpixel` / `ypixel` to get a
   cell footprint — `(width / cell_pixel_width, height / cell_pixel_height)`;
3. it bails out with `no size information available` only if **rows and cols are
   both zero**.

We answered `18 t` (it is in S1's reply table) and nothing else. So termwiz saw a
usable size in cells, believed the probe had succeeded, and left the cell pixel
size at its initialised `0` — one division later, the child panicked. The panic
is in the child, but the missing reply is ours: **a terminal that answers half of
a compound query is worse than one that answers none**, because the half it
answers is what convinces the caller it has everything.

Fixes, in the order that matters:

| # | Change |
|---|---|
| 1 | `CSI 16 t` is answered — the cell size in pixels, `CSI 6 ; <height> ; <width> t`. This is the one that stops the panic. |
| 2 | `CSI 14 t` / `15 t` (text area / screen in pixels) and `CSI 19 t` (screen in cells) are answered too, all derived from the same pair of numbers, so a tool that cross-checks them gets a consistent story. |
| 3 | The cell size is **measured** at startup instead of assumed: `caps::probe_cell_px()` reads the host tty's `TIOCGWINSZ` (via `crossterm::terminal::window_size()`, which is what `wezterm imgcat`'s own producer reports) and divides pixels by cells. `UNCL_CELL_PX=WxH` still overrides, and `DEFAULT_CELL_PX` (10x20) is the last resort. One `ioctl`, no protocol round-trip, no blocking read. |
| 4 | `--doctor` says where the number came from: `cell size: 10x20 px (measured)`, so a wrong image size is one command away from being explained. |

Three invariants are now tests, because "some number" is not the requirement:

* no window report may contain a zero in any field (`no_window_report_is_ever_zero`);
* cells × cell size == the pixel report for the same area
  (`window_reports_are_internally_consistent`);
* a cell-size spec is either usable or ignored — `0x20` is *not* a cell size
  (`a_cell_size_spec_is_either_usable_or_ignored`).

**One more bug fell out of the first `cargo test` run** — and it is worth writing
down, because the test that caught it was written from the wrong idea of the
format. `a_sixel_too_large_for_the_float_is_skipped` failed: a 1000x1000 sixel
inside an 80x24 float was stored instead of skipped. The cause was in
`raster_attributes`: real sixel writes `"1;1;640;480#0;2;0;0;0`, where the
fourth number runs *straight into* the first `#` colour command with no `;`
between them. Splitting on `;` glued `480#0` together, `parse_u32` rejected it,
the height was lost, the footprint came back `Unknown`, and an unsized image was
always assumed to fit. So the "too large" test was passing for the wrong reason
on a fixture I had written in the same wrong shape. The parser now reads four
numbers and stops at the first non-digit, a `sixel_raster_attributes_end_at_the_first_command`
test pins the real header shape, and the fixture was corrected — which is the
only way a fix like this stays fixed.

The general lesson, worth carrying into S4: **every query we answer creates an
obligation to answer its siblings.** The reply table in `callbacks.rs` is a
contract with every terminal application in existence, and the failure mode of a
partial table is not "the app degrades", it is "the app trusts the half-answer and
divides by zero". §6.5's fidelity table should eventually be read the same way: a
capability we claim is a promise about the ones next to it.

### Where S3 leaves the plan

S0–S3 of §6 are now in the tree: fast, correct, self-owned emitter, images. What is
left is the tail the report already marked optional — `OSC 8` hyperlinks (S3.5),
the `alacritty_terminal` engine swap (S4), and the Windows delta (§6.6) — plus the
one open S2 item (the golden-frame corpus). Images are the last thing that was
*broken* rather than merely missing.
