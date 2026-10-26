# uncl: rendering architecture review — why it is slow, why full‑screen apps break, and the shortest path to "renders everything natively"

**Scope:** `uncl` @ `ff09124` (1,286 LOC of Rust, MIT, `ratatui 0.29` + `tui-term 0.2` + `vt100 0.15.2` + `portable-pty 0.8`).
**Method:** static review of the whole tree (`src/**`), plus source-level inspection of the candidate engines and of `zellij` as the closest sibling design. The sandbox has no Rust toolchain, so **no numbers below are measured** — every performance claim is a structural argument with a cheap way to verify it, given in §2.8.

---

## 0. TL;DR

1. **The parser is not the bottleneck.** `vt100`/`vte` parse at hundreds of MB/s; uncl parses a few KB per frame. The slowness is (a) a 10 ms tick that renders the whole screen ~100×/s unconditionally, and (b) an event loop that processes **exactly one input event per tick**, which caps input and paste throughput at ~100 events/second. Neither has anything to do with `ratatui`'s drawing speed.
2. **`vt100 0.15.2` is the wrong pin, and it is the direct cause of the "poor sequence parsing" you are working around by hand.** `0.16`+ exposes the parser's *callbacks* (unhandled CSI/OSC/ESC) **and** first‑class terminal modes (`mouse_protocol_mode()`, `mouse_protocol_encoding()`, `application_cursor()`, `bracketed_paste()`, `hide_cursor()`, `cursor_style()`). That deletes the `String::from_utf8_lossy` + 20×`contains()` hack in `owner.rs:203–260`, which is both slow and **incorrect across read boundaries**.
3. **BTOP doesn't render because of three stacked geometry bugs plus a mouse-routing bug, not because of the parser.** The float's PTY is sized `rect − 4`, the parser grid is sized `25×80`/`rect`, and the widget paints into `rect − 2` — for a full-screen TUI, every one of those mismatches shows up as a shifted/garbled frame. And when the float is visible, **all mouse events are consumed by drag/resize** — the child never sees a single mouse event (`owner.rs:371`). BTOP is largely mouse-driven.
4. **"Renders everything natively" is not reachable by swapping the parser.** No Rust engine paints a *grid back to ANSI* with fidelity, and the features you are missing (`sixel`, kitty graphics, hyperlinks, `OSC 52`) are dropped **below** the grid layer: `vt100` does not implement `vte::Perform::hook/put/unhook` (DCS payloads vanish), and upstream `vte` routes `SOS/PM/APC` bytes into `anywhere()`, which discards them (kitty graphics APCs vanish for *every* `vte`-based engine).
5. **Shortest path (recommended):** three independent, individually shippable stages, ~**2,000 net LOC**, ~**2–3 focused weeks**:
   - **S0 — stop being slow** (~1 day): event-driven redraw, drain-to-empty input, `vt100 0.18`, delete the byte-sniffing.
   - **S1 — stop lying about geometry and input** (~3 days): one source of truth for the float's size, region-based input routing, mode-aware key/mouse encoding, paste, and a **reply channel** for terminal queries.
   - **S2 — own the emitter** (~1 week): delete `ratatui`+`tui-term`, write ~300 LOC that turns the grid into ANSI bytes using `vt100`'s own `rows_diff`/`state_diff`/`cursor_state_formatted`. This is the change that makes it fast *and* makes graphics possible.
   - **S3 — graphics passthrough** (~1 week, optional): DCS/APC tap + image re-emit, so sixel/kitty images survive compositing.
6. **The genuinely shortest path to "native for everything", if you will trade one UX rule:** *passthrough takeover* — when the float's child enters the alternate screen (BTOP, nvim, lazygit, sixel viewers all do), stop compositing and hand the child the whole real screen, relaying its bytes verbatim. Zero parsing, **100% fidelity by construction**, including sixel/kitty graphics. It is ~1 week, needs no new emulation code, and is orthogonal to S0–S3. Hybrid ("float for shells, takeover for alt-screen apps") is the highest value-per-line option on the table — see §6.5.

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
| `tui-term`'s `vt100_imp.rs` (adapter) | 123 | Lets you pin `vt100 0.18` today. `tui-term 0.3.4` requires `vt100 = 0.16.2` and `ratatui-core 0.1` (i.e. ratatui 0.30.2), so you *cannot* upgrade `vt100` without either vendoring this file or patching the dependency. Copy it into `src/app/ui/adapter.rs` and delete `tui-term`. |
| `vt100` patch: forward `hook/put/unhook` to `Callbacks` | ~25 | Upstreamable; unlocks sixel/tmux passthrough with the engine you already have. Otherwise use the side-tap in §6.4. |
| alacritty's `input/keyboard.rs` (key → sequence, mode-aware) | ~600 | Only if you adopt `alacritty_terminal`. It is the best-tested implementation of application-cursor / keypad / kitty-keyboard encoding. |
| `alacritty_terminal` itself (dependency, not vendoring) | 11.7k | Apache-2.0 is fine for an MIT project with attribution/NOTICE, but note the licence asymmetry for a "vendored" copy. |
| `wezterm-term` (git dependency) | very large | Only if *images inside the grid* are a hard requirement. The build-time cost is real, and it is not on crates.io with these workspace deps. |

**Recommendation:** do not vendor an emulator. Use `vt100 0.18` as the engine, own ~300 LOC of encoder, and tap DCS/APC for graphics. That is a smaller, more debuggable delta than any emulator swap, and it is the same architecture `zellij` arrived at (own grid → emit ANSI) with 16k LOC instead of your 2k.

---

## 6. The shortest path

Stages are ordered by *dependency*, not by size, and each one is independently shippable. Effort assumes one developer familiar with the code.

### 6.1 S0 — "make it fast" — ~1 day, ~200 LOC changed

1. **Drain-to-empty input.** Replace `if poll(0) { read() }` with `while poll(0)? { handle(read()?) }`, bounded by a per-tick budget (e.g. 512 events) to stay fair to rendering. → §2.2, ~20 LOC.
2. **Dirty-driven redraw.** Add `Arc<AtomicBool>` (or a `tokio::sync::Notify`) set by the reader threads when they process bytes; render only when dirty, or at a 60 Hz floor while an animation/`REP` is in flight, or on input. Replace `sleep(10)` with `select!` on `Notify` + a 16 ms timer. → §2.1, ~40 LOC.
3. **Enable bracketed paste on the host** (`EnableBracketedPaste`) and forward `Event::Paste(s)` as one `\x1b[200~…\x1b[201~` write when the child has `?2004h`, or as a raw bulk write otherwise. → paste goes from ~100 chars/s to instant, ~40 LOC.
4. **Unbounded/batched PTY writer.** Replace the 32-slot channel + `await` with an unbounded channel plus a coalescing writer task (join queued writes, one `write_all` + `flush` per wake-up); remove every `.unwrap()` on send. → §2.3, ~50 LOC.
5. **`vt100 0.15.2 → 0.18.1`** (see §5: vendor the 123-LOC adapter, drop `tui-term`), then **delete `owner.rs:203–260`** and read the modes you need from the screen: `mouse_protocol_mode()`, `mouse_protocol_encoding()`, `application_cursor()`, `bracketed_paste()`, `hide_cursor()`, `cursor_style()`. → §2.5, §3#5, ~+40/−60 LOC, plus the 0.18 API churn (`Parser::new` takes `NonZeroU16`; `set_size` semantics changed; `set_scrollback` moved to `Screen`).

**Exit criteria:** idle CPU ≈ 0 with a still screen; ≤2 ms keystroke→PTY; 5 KB paste lands in one write; `htop` inside the float responds to the mouse *offsets* correctly even if not yet fully routed (mode tracking is now real).

### 6.2 S1 — "make it correct" — ~3 days, ~500 LOC

1. **One geometry source of truth.** Introduce `Overlay::content_rect() -> Rect` (rect minus border, and minus any off-screen part) and a single `resize_child(cols, rows)` that sets the PTY **and** `parser.screen_mut().set_size()`. Delete both `− 4`s (`tenant.rs:71–72`, `tenant.rs:107–108`) and stop printing `size.rows/cols` as if it were the child's size. On host resize, recompute `rect` and propagate. → §3#1, #6.
2. **Region-based input routing.** Hit-test first, *then* decide policy:
   - pointer **inside** `content_rect` → encode and forward to the float's PTY using **the float's own** `mouse_protocol_encoding()`/`mouse_protocol_mode()` (SGR / X10 / UTF-8), with coordinates translated into the child's space, and with motion filtered exactly like a real terminal does (`1003` any-motion vs `1002` button-motion). Scroll wheel → `64/65` in X10 or `CSI < 64;x;y M` in SGR.
   - pointer **outside** → uncl's own chords (drag/resize/move), consumed locally.
   - reserve one modifier (or `Home`, which is currently stolen and never forwardable — `keyboard.rs:18`) as the "local" escape hatch. → §3#2.
3. **Mode-aware key encoding.** `application_cursor()` → `SS3` (`\x1bOD`) for arrows/Home/End, `CSI 1;2D`-style for modifiers, `application_keypad()` for keypad, and honour bracketed paste. Best-effort `CSI > u` (kitty keyboard) is *not* worth doing at this stage — instead, do **not** advertise it (§6.2.5). → ~100 LOC, replaces the hand-rolled table in `keyboard.rs`.
4. **Reply channel.** Give both parsers a `Callbacks` impl that writes responses back into the corresponding PTY:
   ```
   CSI c        → CSI ?64;1;2;6;9;15;18;21;22 c   (xterm-ish DA1, conservative: list only what you honour)
   CSI > c      → CSI >0;0;0 c                    (DA2: deliberately empty -- do not claim to be a VT level)
   CSI 6 n      → CSI row;col R                   (from screen.cursor_position())
   CSI ? 6 n    → CSI ? row;col R
   DCS $ q ...  → DCS 1 $ r <value> ST            (DECRQSS: only claim what you honour)
   OSC 10/11;?  → OSC 10;rgb:…  / OSC 11;rgb:…    (query the *host* once at startup, cache)
   ```
   Do not advertise what you don't implement (kitty keyboard, `XTGETTCAP` for unsupported caps): a conservative DA1 is far better than a lying one. Also forward `OSC 52` from the child to the host clipboard (vt100 0.16+ surfaces it) and answer `OSC 52;c;?` from the host's clipboard if you enable that.
5. **Cursor fidelity.** `if screen.hide_cursor() { hide } else { show, and use cursor_style() to emit DECSCUSR }`. Also stop `RAPID_BLINK`-ing the owner cursor. → §3#4.
6. **Focus/paste passthrough**: if the child enabled `?1004`, emit `CSI I`/`CSI O`. (Small; adds polish for nvim.)

**Exit criteria:** BTOP renders a stable full frame at the right size, is mouse-clickable inside the float, hides the cursor correctly, and survives 20 consecutive float resizes with no smearing. `nvim` + `lazygit` + `less` + `fzf` all behave. *(This is the "everything except images/hyperlinks" milestone, and it is about a week of work.)*

### 6.3 S2 — "own the emitter" — ~1 week, ~350 LOC, −4 dependencies

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

### 6.4 S3 — "images and links, passthrough" — ~1 week, ~450 LOC

1. **Tap DCS/APC.** Wrap the parse of each chunk:
   ```rust
   // split at ESC P / ESC _ ; feed prefix to vt100 first so the cursor is current
   for seg in Splitter::new(chunk) {
       match seg {
           Seg::Bytes(b)  => parser.process(b),
           Seg::Dcs{..} | Seg::Apc{..} => graphics.on_payload(&parser.screen(), seg),
       }
   }
   ```
   `Splitter` is ~80 LOC (`ESC P`…`ST`, `ESC _`…`ST`, `ESC \`, `\x07`-terminated OSC passthrough handled by vt100 anyway). For DCS, prefer patching `vt100` (25 LOC: forward `hook/put/unhook` into `Callbacks`) — cleaner, upstreamable, and keeps one parser.
2. **Image list.** `struct Placement { rect: Rect, bytes: Vec<u8>, proto: Sixel|Kitty }`. On payload: decode just enough header to know the pixel footprint (sixel: parse the `"` raster attributes, or the DCS `q` params; kitty: `a=T,f=…,s=…,v=…`), convert to cells, store at the current cursor cell.
3. **Repaint.** After the grid flush, re-emit every placement that intersects the frame. On occlusion by a float or a scroll, drop it (that's what tmux effectively does for panes it doesn't own).
4. **Clipping.** Kitty graphics support source-rect cropping (`a=p,x=…,y=…,w=…,h=…`), so you can crop to the float. **Sixel has no crop mechanism**, so an oversized sixel image either overflows the float or must be re-encoded (decode → RGB → sixel). Pick "degrade: draw only if it fits", document it, and you have parity with tmux.
5. **Hyperlinks.** `vt100` doesn't track `OSC 8`. Two options: (a) track it yourself in the same tap (open/close state + per-cell id, ~60 LOC), or (b) move the engine to `alacritty_terminal` and read `cell.hyperlink()` / `cell.underline_color()` / `cell.zerowidth()`. (a) is cheaper if you stay on `vt100`.

**Alternative to all of S3, if you want to stop at ~90% fidelity:** note that images can also be handled by *not compositing them* — see §6.5.

### 6.5 The shortcut worth seriously considering: passthrough takeover

The float is opaque, so compositing is only needed while a *background* exists to paint over. Full-screen apps take the alternate screen, at which point the background is irrelevant — there is nothing to composite against. So:

> **When the float's child enters the alternate screen (`?1049h`/`?47h`), and the float is visible, promote the float to the full screen and relay the child's bytes verbatim.** Intercept `?1049l` to demote back.

What you get for ~1 week and **zero** emulation work:

- BTOP, nvim, lazygit, htop, `less`, sixel/kitty image viewers render **byte-exactly**, because their bytes reach the real terminal unmodified. No parser in the path means no fidelity ceiling at all.
- Every input mode works for free (you're just a pipe): mouse at native rates, kitty keyboard, focus, paste, `OSC 52`, DCS passthrough, images.
- S0/S1 still needed (speed, geometry, mode tracking, reply channel), but S2/S3 become optional.

What you give up: the app is no longer floating while it runs. Drag/resize of a running TUI becomes a full-screen resize (which is what those apps actually want anyway). For the monoplexer's stated purpose — "one floating term as an accomplice to your terminal misdeeds" — a **hybrid** is the best value on the board:

| Float content | Behaviour |
|---|---|
| Shell / line-oriented tools (no alt screen) | stays a floating window, grid-composited (S0–S2 quality) |
| Alt-screen apps (BTOP &co.) | auto-promoted to full screen, byte-exact passthrough, demoted on exit |

That hybrid reaches "renders everything natively" in about **2 weeks** with no emulator work, and S2/S3 remain as the "float a TUI properly" upgrade path afterwards.

### 6.6 Sequencing summary

| Stage | Effort | Net LOC | Unlocks | Risk |
|---|---|---|---|---|
| S0 speed | 1 d | ~±200 | Idle CPU → ~0; input 1 event/tick → unbounded; paste instant; real mode tracking | Low. `vt100` 0.18 API churn; vendoring the 123-LOC adapter. |
| S1 correctness | 3 d | ~+500 | BTOP/htop/nvim/lazygit render at the right size, mouse works, cursor correct, no more resize smearing | Medium — input encoding branches per mode; needs a test matrix. |
| S2 own emitter | 1 wk | ~+350, −4 crates | Native-speed scan-out; no per-cell `Style`; a hook for out-of-band bytes | Medium — off-by-one/clipping bugs are visible; mitigate with golden frames. |
| S3 graphics | 1 wk | ~+450 | sixel, kitty graphics, `OSC 8` links, `OSC 52` | Medium-high; sixel clipping is genuinely unsolvable — degrade on purpose. |
| S4 engine swap (`alacritty_terminal`) | 3–5 d | ~+600 | Underline colour/styles, hyperlinks, kitty-keyboard input, sync output, `EventListener` for title/clipboard/bell | Medium; requires S2 (you must own the encoder before leaving `vt100`). |
| S5 (alt to S3) takeover | 1 wk | ~+250 | Byte-exact everything for alt-screen apps | Low code cost, product/UX decision. |

**Recommended critical path: S0 → S1 → (S5 and/or S2 → S3).** S4 only if you need hyperlinks/underline styles *inside a floating window*.

---

## 7. Things that look like short paths and are not

- **"Just write the child's bytes straight to stdout and overlay the float."** Cursor addressing, clipping, scroll regions and occlusion make this a full sequence *rewriter* (harder than a grid). Only works if the child owns the whole screen — which is exactly §6.5, and only there.
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

## 9. Open questions (product decisions that change the plan)

1. **Is "floating while a TUI runs" a hard requirement?** If not, §6.5 alone gets you "native for everything" in ~1 week and S3 can be dropped forever.
2. **Are inline images (sixel/kitty) in scope?** If they are, note that sixel cannot be clipped to an arbitrary rect without re-encoding; if they're not, S3 shrinks to hyperlinks + `OSC 52` (~150 LOC).
3. **Do you need mouse interaction *inside* the float?** If the float is keyboard-only by design, S1 shrinks by ~150 LOC but BTOP/htop stay awkward — I'd assume yes.
4. **Windows/Docker targets?** `portable-pty` handles it, but SGR-only mouse encoding and DECSCUSR differ; the reply channel needs a `TERM`-aware capability table.
5. **`vt100` 0.18 upgrade appetite** — it changes `Parser::new`/`set_size` signatures and is a prerequisite for deleting the sniffing hack. Patching `tui-term` or vendoring its 123-LOC adapter is the only way to have both.

---

## Appendix A — candidate-crate facts used above

| Crate | Version inspected | LOC | Key API | Note |
|---|---|---|---|---|
| `vt100` | 0.18.1 (2026-10-19) | 5,111 | `Screen::{rows_diff, rows_formatted, contents_diff, contents_formatted, state_diff, input_mode_formatted, cursor_state_formatted}`, `Callbacks::{unhandled_csi, unhandled_osc, unhandled_escape, copy_to_clipboard, paste_from_clipboard, set_window_title, resize, on_scroll}`, `Screen::{mouse_protocol_mode, mouse_protocol_encoding, application_cursor, bracketed_paste, hide_cursor, cursor_style}` | No `hook/put/unhook` ⇒ DCS payloads dropped. `Screen: Clone`. 0.18 requires `NonZeroU16` sizes. |
| `vte` | 0.15.0 | 4,159 | `Perform::{print, execute, hook, put, unhook, osc_dispatch, csi_dispatch, esc_dispatch}` | `State::SosPmApcString => anywhere()` discards APC bytes (`src/lib.rs:184`) ⇒ kitty graphics invisible to every `vte` consumer. |
| `tui-term` | 0.3.4 | 1,574 (adapter: 123) | `impl Screen for vt100::Screen`, `Cell::apply` | 0.3.4 requires `vt100 = 0.16.2`, `ratatui-core 0.1`/`ratatui-widgets 0.3` (ratatui 0.30.2, released 2026-06-19). |
| `alacritty_terminal` | 0.26.1-dev | 11,728 | `Term::{renderable_content, grid, mode}`, `TermMode` (incl. `KITTY_KEYBOARD_PROTOCOL`), `RenderableContent{display_iter, cursor, colors, mode}` | Apache-2.0. No sixel/graphics anywhere in the repo. Key *encoding* lives in the binary (`alacritty/src/input/keyboard.rs`), not the library. |
| `wezterm-term` | 0.1.0 | large | `Terminal` + `Image`/`ImageDataType`, `terminalstate/sixel.rs`, `image.rs` | MIT, git-only workspace crate with `termwiz`/`image`/`terminfo` in tow. |
| `zellij` panes | HEAD | ~16k | `grid::render → (CharacterChunk, raw_vte_output, SixelImageChunk, KittyImageChunk)`, `kitty_graphics/{store,replies,grid_state}`, `sixel::SixelImageStore`, `hyperlink_tracker` | MIT reference implementation of exactly the architecture recommended here. |
