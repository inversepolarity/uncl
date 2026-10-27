# uncl — rendering architecture

uncl is a terminal monoplexer: one floating terminal window over your shell,
toggled with `Home`. Everything a child program draws has to be captured, held in
a cell model, and repainted inside that floating rectangle, because the window can
move or disappear at any moment and the background shell has to be restored
underneath it.

This document describes the rendering architecture as built. It answers the four
questions that started the work, then documents each part of the system by name —
what it does, the rules it follows, and the compromises it accepts. The items that
are *not* finished are collected in one place at the end.

---

## The questions, and the answers

The original review asked: why is uncl slow, is `ratatui` the cause, can an ANSI
parser be vendored, why do BTOP-class TUIs mis-render, and what is the shortest
path to "renders everything natively". The answers shaped everything below.

### The parser was never the bottleneck

`vt100`/`vte` parse at hundreds of MB/s; uncl parses a few KB per frame. The
slowness was in the event loop:

- a 10 ms tick that rebuilt and diffed both full-screen buffers ~100×/s whether or
  not a single byte had arrived, and
- an input path that consumed **exactly one event per tick**, capping typing, mouse
  motion and paste at ~100 events/second, with up to 10 ms of added latency each.

Neither is `ratatui`'s drawing speed, and neither is fixed by a better parser.

### Why full-screen apps mis-rendered

Not the parser either. Three stacked geometry bugs and one routing bug:

1. **Three different sizes for the same window.** The float's pty was sized
   `rect − 4`, its `vt100` grid `25×80` at creation and `rect` afterwards, and the
   widget painted into `rect − 2`. A full-screen app asks `TIOCGWINSZ`, draws for
   the answer, and uncl displayed a different rectangle.
2. **The float never received mouse input.** While it was visible, every mouse
   event was consumed by drag/resize; none was encoded for the child. BTOP is
   largely mouse-driven.
3. **No terminal-query replies, ever.** Nothing answered `CSI c`, `CSI > c`,
   `CSI 6n`, `DCS $q`, `OSC 10/11;?`, so every capability probe blocked until its
   timeout and then guessed.
4. **The child's hidden cursor was drawn anyway**, and its hidden state was never
   consulted.

Plus a fidelity ceiling that is architectural rather than a bug: `ratatui`'s cell
holds one character and a style, so there is no way to represent an image, a
hyperlink, or an underline colour no matter how good the parser is.

### What "renders natively" actually requires

Any compositing multiplexer must hold a cell model of both children — the
background has to be repainted under and after the float, so its cells must exist
somewhere. "Native" therefore means *the grid and the wire encoder jointly preserve
everything the child emitted*, which takes three separable things:

1. **A complete state model** — text, SGR (including underline colour and styles),
   hyperlinks, modes (mouse, keypad, cursor, paste, focus, sync), cursor shape and
   visibility, scrollback, alternate screen.
2. **A state → ANSI encoder.** No off-the-shelf crate does this: `vt100` is the only
   public one (`rows_diff`, `rows_formatted`, `contents_formatted`), and it covers
   text, SGR, cursor and input modes. Alacritty and wezterm render to a GPU surface,
   not to bytes.
3. **Out-of-band passthrough** for what a grid cannot hold — sixel, kitty graphics,
   iTerm2 inline images, tmux passthrough. These are "blit bytes at a cell" events:
   keep a list of `(anchor, bytes)` and re-emit on repaint.

uncl does (2) itself, does (1) as far as `vt100` models it, and does (3) with a
stream tap. That is deliberately a smaller delta than swapping in a full emulator.

### Vendoring an ANSI parser is feasible but not useful

`vte` is ~4k LOC, two tiny dependencies, `no_std`-capable, and already in the tree
transitively — but it is a *state machine*, not a terminal: it dispatches
`print/execute/csi/esc/osc/hook/put/unhook` and holds no grid. Owning it changes
nothing about fidelity. The gaps that mattered live in `vt100`'s grid and in the
absence of a wire encoder, and the cheapest high-value copy turned out to be one
123-line adapter that let `vt100` be upgraded at all.

The decision that followed: do not vendor an emulator. Use `vt100` as the engine,
own the encoder, and tap the stream for graphics — the same architecture zellij
arrived at with ~16k LOC instead of ~2k.

---

## The architecture, end to end

```
  stdin (raw mode)
        │
        ▼
  input pump thread ──► event queue ──┐
                                      │
  owner pty ──► reader ──► splitter ──┼──► vt100 parser (owner)  ─┐
                              │        │                           │  snapshots
  float pty ──► reader ──► splitter ──┼──► vt100 parser (float)  ─┤  (one clone
                              │        │                           │   per pane)
                              │        └──► one event loop ────────┘
                              │                 │
                              │                 ▼
                              │            Renderer::render  ──► one write_all
                              │                 ▲
                              └── images, DCS replies, links, clipboard
                                  (out of band, never given to the parser)
```

Four invariants hold this together:

- **One writer for stdout.** Frames, host effects and cursor-shape changes all go
  through the thread that owns stdout, so a child's clipboard copy can never land
  in the middle of a frame.
- **The parser lock is never held across a frame.** Each pane's screen is cloned
  under its lock, then the frame is encoded from the clones.
- **A frame is bytes, not a toolkit.** `Renderer` is pure: bytes in, bytes out, no
  timers, no I/O. That is what makes it testable.
- **One rectangle type** (`Rect`) and one definition of the float's inner area
  (`content_rect`), shared by the mouse router, the pty sizing and the painter.

---

## Event-driven frames

A frame is painted when something changed, not on a timer.

- Each pty reader raises a sticky `Signal` after feeding the parser. The event loop
  wakes on that signal, on input, or on the frame deadline — never on a fixed
  tick.
- Frames are coalesced to `FRAME_MIN_INTERVAL` (8 ms, ~120 Hz), so a child flooding
  output cannot make uncl repaint more often than a terminal can use.
- An unchanged screen produces an **empty** frame, and `draw` performs no syscall at
  all. A still BTOP costs nothing.
- A 100 ms housekeeping tick exists only to notice that a child exited.
- Show, hide, move, resize, respawn and alternate-screen entry request an explicit
  repaint (`Renderer::invalidate`).

Input is decoupled from frames: a dedicated blocking pump thread polls `crossterm`
and forwards events, and the loop drains up to 512 queued events per iteration
(`INPUT_DRAIN_BUDGET`) instead of one. Each pane has one coalescing writer: an
unbounded channel whose contents are joined into a single `write_all` of up to
64 KiB, so a 5 KB paste is one syscall pair rather than 5,000.

**The engine.** `vt100` is pinned at 0.18.1, with its cell→buffer mapping vendored
as a 123-line adapter so `tui-term` could be dropped. Getting there required
`ratatui` 0.30.2: `ratatui` 0.29 pins `unicode-width == 0.2.0` while every `vt100`
≥ 0.16 needs ≥ 0.2.1, and cargo will not keep two semver-compatible copies. The
toolkit bump was temporary plumbing — the toolkit is gone from the tree entirely.

**Why the upgrade mattered.** `vt100` 0.16 added parser callbacks and first-class
terminal modes; 0.17 added `REP`, `HPA`/`HVP`, `DECSCUSR`, `CSI 3J` and VS16 emoji
width; 0.18 made `set_size` xterm-compatible by pulling rows out of scrollback
instead of trimming the bottom — which is the resize bug a constantly-resized float
hits hardest. It also deleted the old hand-rolled byte sniffing (`from_utf8_lossy`
plus twenty `contains()` calls), which was both slow and wrong across read
boundaries: modes are now read from the parser, so a sequence split between two
reads cannot desynchronise anything.

---

## One geometry for the float

There is exactly one answer to "how big is the float's terminal", and three
consumers read it:

| Consumer | Uses it for |
|---|---|
| the pty | `TIOCSWINSZ`, so the child draws for the right size |
| the parser | the grid the child's output is parsed into |
| the renderer | the rectangle the rows are painted into |
| the mouse router | converting a screen position into a child-relative cell |

All four derive from `Overlay::content_rect()` = the float's rect inset by one cell
on each side. Resizing clamps instead of bailing (a drag that crossed `MIN_HEIGHT`
used to freeze), keeps the anchor corner fixed for a corner drag, and re-applies the
same one number to pty and grid together.

The float's border carries the state that is otherwise invisible: the child's **size**
and, while the view is scrolled back, the offset (`s:24:80 +12`) in the bottom
border; the child's **window title** (`OSC 0/2`) in the top one.

---

## Input: routing, encoders, and who owns the mouse

Every mouse event is decided by one pure function, `mouse::route(overlay, event)`:

```
Alt+drag / border press  >  child (if a gesture is in progress)  >  child (content)  >  uncl
```

| Landing | Route | Why |
|---|---|---|
| `Alt` held, on the float | `Local` | move the float |
| the one-cell border | `Local` | resize from that edge/corner |
| inside the content rect | `Child { col, row }` | the child's coordinates, encoded for the child's protocol |
| outside the float | `HideFloat` | click away to hide |

A gesture that *starts* in the child's grid belongs to the child until the button
comes up, even if the pointer leaves the float — `Overlay::child_grab` records that,
and out-of-grid coordinates are clamped to the nearest cell, which is what a real
terminal reports. Losing that release is what used to leave a selection stuck in the
middle of a drag.

**Encoders are per pane and mode-aware.** `ChildModes::from_screen` reads the
parser; `encode_key` handles characters, Ctrl→control range, Alt prefix,
`DECCKM`-aware arrows and Home/End, SS3 for F1–F4, tilde keys, xterm modifier
parameters and application keypad. `encode_mouse` emits SGR, legacy X10 or UTF-8 by
the mode the *child* reported, gated on whether the child asked for mouse reporting
at all, with 1-based coordinates and proper release handling. Bracketed paste is
wrapped only when the child enabled it.

**The host's mouse mode is not the child's.** `HostMouse` writes the DEC private
modes itself instead of using crossterm's capture-all: SGR encoding always,
button-press always (uncl needs it to move the float, hide it on a click outside,
and resolve hyperlinks), motion only when the focused child asked for it, and
everything off when nobody wants it. The trade-off is explicit: the host's own
scrollback and click-drag selection are unavailable inside uncl, because uncl
provides both. `Shift`+drag still selects in most emulators, which take that key
back locally.

**Focus and cursor shape are relayed.** `?1004` is tracked (the parser does not model
it) and focus in/out is sent to whichever pane gains or loses the keyboard. The
focused pane's `DECSCUSR` shape is relayed to the host and reset on exit, so nvim's
insert-mode beam is a beam.

**The chords.** `Home` toggles the float. `Alt`+arrows move it, `Alt`+Shift+arrows
resize it. `Ctrl+Alt+q` quits. `Ctrl+Alt+Up/Down` and `Ctrl+Alt+PageUp/PageDown`
scroll the focused pane. Everything else is delivered to whichever pane owns the
keyboard.

**A crashing float cannot take the owner down.** The float's shell exiting respawns
it (visible again if it was visible); a failing `exec` is backed off rather than
retried every tick, and a failure there never propagates out of the event loop.

---

## The emitter: painting a frame ourselves

`ratatui` and `tui-term` are gone. A frame is a `vt100::Screen` turned into ANSI
bytes by `src/app/render/`, written once.

1. Both screens are cloned under their locks (one clone per pane, ~32 B/cell).
2. The clones are handed to `Renderer::render(Panes { .. })` **by value**, so the
   renderer keeps them as the next frame's `prev` — one clone per pane, not two.
3. Nothing changed → an empty slice, no syscall.
4. Otherwise: `?25l` → changed owner rows → the float's rows, each preceded by a
   `CUP` into its content rect → borders and titles if the rect is new or was
   overpainted → one `CUP` + `?25h` for the focused child's cursor → one `write_all`.

Three rules make that correct, and each was discovered the hard way:

**`vt100`'s row encoders are pane-local.** `rows_formatted`/`rows_diff` address the
row as if the pane sat at the screen origin, and emit a bare `CSI H` for that origin
— whose defaults are 1;1. `append_row` rewrites every absolute move (bare, partial or
explicit, `H` or `f`) into screen coordinates and leaves every other sequence alone.

**`EL` inside a float is a weapon.** A styled-but-empty row tail — BTOP's entire
chrome — ends in `CSI K`, which erases to the end of the *screen*: the float's right
border, the owner's cells beside it, and the frame gutter. So the right border is
painted *after* the row, and a row that emitted `EL` triggers `repair_tail`, which
repaints the owner's cells from the border to the screen edge.

**`rows_formatted` never blanks.** It skips default cells and relies on what is
already on screen, so a full repaint must `ECH(width)` first — bounded to the
float's width, so it cannot reach the border. `rows_diff` blanks by itself, so diff
rows cost one `CUP` plus the changed cells and no erase.

The float's dirty-row plan is therefore three-way: **skip** (unchanged), **diff**
(changed), **full** (new, or the owner repainted that row *under* the float — a cell
diff on the owner cannot know the float was sitting on top of it).

The cursor is not painted as a cell. Painting stays cursor-hidden, then one `CUP` +
`?25h` places the real cursor in the focused pane, honouring the child's own
`hide_cursor()`. A hidden cursor has no observable position, which is also why the
frame does not try to move one.

`state_diff` is deliberately never replayed: uncl owns the host's modes. Replaying a
child's input-mode state would make the host start reporting in a protocol the child
never asked for.

---

## Images

The tap sits between the pty reader and the parser: `DCS`, `APC` and `OSC` strings
are lifted out of the byte stream, across chunk boundaries, and everything else is
handed to `vt100` unchanged. This is necessary rather than convenient — `vt100` does
not implement `hook/put/unhook`, so a DCS payload is consumed and dropped inside the
parser, and `vte` routes `APC` bytes into `anywhere()`, which discards them.

The splitter is protocol-agnostic; the store is not. Anything the store does not
recognise goes straight back to the parser, so `OSC 52`, colour queries and `DCS`
queries behave exactly as they would without a tap.

The rules the store follows:

- **A terminator can be split.** `ESC` at the end of one read and `\` at the start
  of the next is the normal case for an image on a pty; a lone `ESC` is held back
  rather than treated as payload.
- **A transmission is not an image.** Kitty senders chunk payloads (`m=1`…`m=0`), so
  a 100 KiB PNG arrives as ~25 sequences. Chunks are joined into one entry anchored
  where the first arrived, or storing each fragment would both break the image and
  evict its own beginning.
- **Two image lifetimes.** Kitty images live in the terminal's own layer: painting
  text over them does not erase them, so a still frame costs nothing — but they must
  be deleted (`a=d,d=A`) when the float hides or they stay stuck over the owner's
  screen. sixel and iTerm2 draw into the text plane, so any repaint of their rows
  erases them and they must be blitted again. Emission is exactly those three
  triggers: **new**, **moved**, **erased**.

Emission happens *after* the grid, because any text painted over a pixel-based image
destroys it. Nothing is ever decoded, scaled or re-encoded: the child's bytes are
carried through and re-emitted, so what you get is whatever your terminal can draw.
An image too large for the float is scaled down by the terminal itself (the request
is edited, not the payload); sixel has no way to ask for a size, so an oversized
sixel is skipped once with a note. Kitty images are re-placed by id on a move, which
is one line rather than a payload.

---

## Hyperlinks

`OSC 8` is a property of a run of cells, not an event, and the emitter paints rows by
re-encoding cells. So a link has to be re-established around the run at paint time,
inside a row `vt100` handed over as one string.

`vt100` does not model hyperlinks at all, so the span is recorded where the cursor
still knows it: `Callbacks::unhandled_osc`, at open *and* at close. The store keeps
`(row, start, end, params, target, text)`; the text it covered is re-checked when the
row is painted, so a link whose cells have since changed is simply not drawn — a
clickable region over the wrong words is worse than none.

The row is emitted in **column-ranged segments** (`rows_formatted(start, len)`),
with `OSC 8` between them. Splicing into `vt100`'s encoded row at a byte offset is
not honestly computable: the string contains SGR runs, wide characters occupy two
cells, and combining marks attach to the preceding cell. Asking by column range
leaves wide-character handling, styles and wide-cell skipping to `vt100`.

Known limits, each a deliberate trade: **one row per link** (a wrapped link is
dropped, because `vt100` does not report where the grid wrapped); **a link does not
dirty its row** (links are not part of a cell, so rows carrying one are repainted in
full — more bytes on those rows, nothing on the rest); **64 spans per pane**, oldest
out.

Children see `FORCE_HYPERLINK=1`, which is the convention programs check before
emitting links at all. That follows the same rule as every capability uncl
advertises: *advertise only what you carry, but do carry what you advertise.*

---

## Clicks and scrollback

**uncl resolves the click itself.** While the float is up uncl holds the mouse
(`?1000h`), because it has to: that is how the float is draggable, how a click
outside hides it, and how the wheel can reach uncl's scrollback at all. The
consequence is that the *host* terminal never sees the click, so a hyperlink in the
float could never be opened by the emulator — it has no idea the click happened.

So uncl opens it: a press and a release on the same cell of a live span, in the
float or in the owner pane, only after the child declined the event. A press that
begins a drag or a resize clears the pending press, motion clears it, and a click
while the view is scrolled back resolves nothing (spans live in live-grid
coordinates). `OSC 8` is still painted, so cmd-click in the host keeps working where
the host does see it.

**Opening a URL is a security decision.** A child's output is untrusted, and every
word it prints becomes clickable. `allowed_url` accepts only `http`, `https`,
`mailto`, `file` and `ftp`; no control characters or whitespace (the classic way to
smuggle an argument past an opener), no non-UTF-8, nothing over 2,048 bytes. The
opener is the platform's own — `open`, `cmd /C start ""`, `xdg-open` — spawned and
reaped on a thread, so a click never leaves a zombie and never waits for a browser.

**Scrollback applies to the view, never to the parser.** `draw()` clones each pane's
screen and calls `set_scrollback(n)` on the *clone*:

- a scroll offset is a property of the view. Scrolling the parser would move the
  child's cursor out from under it and change what the next frame's diff means.
- the renderer diffs view against view, so a scrolled snapshot is just another screen
  to it.
- `set_scrollback` clamps, and the clamped value is read back, so an over-scrolled
  offset heals itself when the pane shrinks.

Everything else follows from that law: typing returns the view to live **and still
delivers the key**; the cursor is hidden while the focused pane is scrolled (its
cursor sits on a line that is not on screen); images are suppressed while scrolled,
because they anchor to live cells and live in a layer the text repaint cannot reach;
a link under the pointer resolves nothing; hiding the float resets its offset.

Capacity is inherited from the launching environment: `UNCL_SCROLLBACK` (default
2,000, `0` disables), read once per parser. One wheel notch is
`SCROLL_STEP` lines; `Ctrl+Alt` chords scroll without a mouse.

That capacity has a cost worth stating plainly: a `Screen` clone is a deep copy of
every row it holds, including scrollback (32 bytes per cell).

| Width | 2,000 lines of scrollback | one clone |
|---|---|---|
| 80 columns | 160,000 cells | 5.1 MB |
| 200 columns | 400,000 cells | 12.8 MB |

An idle prompt costs nothing because nothing is painted; a sustained flood at 200
columns pays ~1.5 GB/s of `memcpy` plus 2,000 allocations per frame, which is why
`UNCL_SCROLLBACK` exists in both directions. A stored-painted-rows renderer would
remove the copy rather than shrink it; that is one of the reasons to consider the
engine swap listed at the end.

---

## Answering the child's questions

A child cannot talk to the real terminal — it talks to uncl, which therefore has to
answer everything a terminal would. `src/app/term/caps.rs` builds one capability
record at startup and answers from it; `src/app/term/callbacks.rs` answers through
`vt100`'s callbacks; `src/app/term/dcs.rs` answers the family `vt100` has no hook
for at all.

| The child asks | Answered by | Notes |
|---|---|---|
| `CSI c` / `CSI > c` | `caps::da1`/`da2` | sixel attribute only when the tap will carry it |
| `CSI 6n`, `CSI ?6n` | `callbacks` | 1-based, from the live screen |
| `CSI 14/15/16/18/19 t` | `callbacks` | consistent pixel and cell numbers; never zero |
| `OSC 10/11/4 ; ?` | `callbacks` | xterm defaults, so callers stop hanging |
| `DCS $ q` (DECRQSS) | `dcs` | real pen (`SGR`), real cursor shape; everything else refused, not guessed |
| `DCS + q` (XTGETTCAP) | `dcs` | `TN`, `Co`, `RGB` when truecolor is real; `Smulx` refused |
| `?1004` focus reporting | `callbacks` | tracked, because the parser does not model it |
| kitty `a=q` | the image tap | answered from the child's own pty |

The general rule, learned twice: **`vt100`'s `Callbacks` trait is a list of things
the engine refuses to be responsible for**, and every unimplemented method on it is
a sequence a child sends and uncl silently drops. That is what `DECRQSS`,
`XTGETTCAP`, the bell and the window title all were.

Two of those answers are derived from state the engine already tracks rather than
from convention: `DECRQSS` reports the pen the child actually set, and a setting this
engine does not track is answered *invalid* (`DCS 0 $ r …`), which is legal and tells
the caller to stop asking. A wrong number would be believed.

**`TERM` is passed through, carefully.** The host's terminal family reaches children,
because the tap and `OSC 8` make that claim true. Three families are downgraded to
`xterm-256color`: `screen*`/`tmux*` (a multiplexer's own dialect, which uncl does not
speak), `xterm-kitty` (that `TERM` is what makes applications enable the kitty
*keyboard* protocol, which the encoder does not implement — its graphics half is
advertised separately), and consoles. `uncl --doctor` prints both the host's `TERM`
and the child's, with the reason when they differ.

**The bell** is forwarded to the host, but only from the pane that owns the keyboard:
a hidden float that is still parsing must not interrupt what you are doing in the
shell behind it.

**The title** (`OSC 0/2`) is recorded per pane, sanitised (control characters
stripped, bounded), and drawn in the float's top border. A rename forces a border
repaint, because a title that never appears is worse than none.

---

## The clipboard

`OSC 52` copies are relayed to the real terminal — that request is not about the
child's terminal, it is for the one the user is looking at. Reads are answered in two
tiers:

1. **what a child copied in this session** — `vim`'s `"+y` then `"+p` costs nothing
   and never leaves the process;
2. **the desktop's clipboard**, through `wl-paste`, `xclip`, `xsel`, `pbpaste` or
   PowerShell's `Get-Clipboard`.

The host terminal is deliberately not asked: its reply arrives as *input*, and
input belongs to crossterm's event parser, which would drop or misparse an `OSC 52`
reply. Reading the desktop directly gets the same clipboard without taking over
stdin.

The read runs on its own thread — it happens inside the callback that runs on the
reader feeding the parser, and a blocked reader is a frozen pane — one at a time,
bounded at 1 MiB (base64 makes that ~1.4 MiB of pty traffic, and anything bigger is
almost certainly a copied file). An empty clipboard is still an answer from the
program that owned the selection; with no helper installed the request stays
unanswered, which is what a child's timeout is for. Selectors `c`, `p` and `s` are
served; cut buffers and compound selectors are refused before anything is spawned.

---

## Windows and Docker

Docker needs nothing architectural: `portable-pty` opens a normal pty, `TERM` and
`COLORTERM` propagate, and the shell comes from `SHELL`.

Windows is guarded rather than ported:

- **Output needs one check.** uncl writes raw ANSI, so `crossterm::ansi_support::
  supports_ansi()` runs at startup — it enables
  `ENABLE_VIRTUAL_TERMINAL_PROCESSING` and reports whether the console can honour a
  frame at all. A legacy console gets one sentence instead of an alternate screen
  full of raw escapes.
- **Images are off, in the capability record rather than in a comment.** The child
  sits under uncl's own ConPTY, and ConPTY v1 parses the child's VT into a text
  buffer, swallows `DCS`, and answers `DA1` itself *without* the sixel attribute — so
  a child never asks, and anything forwarded would be destroyed one layer down.
  Advertising it would be a lie of exactly the kind the capability record exists to
  prevent. ConPTY v2 forwards bytes and would make this possible, but shipping it
  means bundling `conpty.dll`.
- **Input is the same encoder.** crossterm's Windows backend produces the same
  semantic events, and `encode_key`/`encode_mouse` already turn semantic events into
  VT sequences, so Windows needs no separate pipeline — only validation on real
  hardware.

---

## Knowing it works

Two layers of testing, and one of them is unusual.

**Unit tests** cover the pieces that can be asserted directly: geometry, routing,
encoders, the splitter, the image store, the link spans, the DCS table, the
clipboard's pure half, and the renderer's byte-level properties (CUP targets, `EL`
repair ordering, diff-versus-full, cursor placement). The suite is 178 tests.

**The golden-frame harness** (`src/app/render/golden.rs`) covers the claim the pieces
exist to support: that a frame uncl writes, replayed into a real terminal, leaves the
terminal showing what the child drew. One stream, two engines, one assertion:

1. feed a recorded byte stream to a `vt100` parser (the source screen);
2. render it through uncl's emitter into bytes (the frame);
3. feed *the frame* to a second, fresh `vt100` (the replayed screen);
4. require every cell — contents **and** attributes — plus cursor visibility to
   match.

There are no golden files to re-bless: the expectation is the source screen, so the
corpus can grow without anyone blessing new output. Fifteen recorded streams cover
plain text, 16/256/truecolor SGR, inverse bars, wide CJK, combining marks,
erase/delete/insert, cursor moves, autowrap off, carriage-return progress
overwrites, tabs and backspace, `REP`, an alternate-screen round trip, an `OSC 8`
span, and a hidden cursor with a `DECSCUSR` style. Three further tests cover what a
frame does *besides* paint cells:

- **a float over the owner's grid** — cells inside the content rect must be the
  float's, and every cell outside it must still be the owner's (this is the only
  end-to-end test of `repair_tail`);
- **a linked row** — `OSC 8` is emitted between column-ranged segments, and a segment
  computed wrongly shifts the row while every string-level assertion still passes;
- **an image** — a payload placed in the float must not disturb a single cell of
  either pane.

Plus the diff path (a corpus grown one frame at a time against a terminal that has
already seen the previous frames) and a resize.

The harness earned its keep on its first run, by finding two wrong *assertions*:

- **A hidden cursor has no position.** `place_cursor` refuses to move a cursor the
  child hid — there is nothing to see — and painting clears the remembered position,
  so the frame that shows it again is forced to `CUP` to the right cell. Comparing
  positions unconditionally was asserting something no user can perceive.
- **`invalidate()` is what makes a resize a repaint.** A resize whose surviving cells
  are all cells that were already painted produces an empty diff. That is correct and
  useless on its own: what actually moves is the float's **border**, which lives in
  cells whose contents may not change at all, and a cell diff cannot see a border.
  One test now pins the distinction — empty without the invalidation, a full repaint
  with it.

---

## Reference: candidate engines

| Engine | Licence | Size | Text+SGR | Underline colour/styles | `OSC 8` | Modes + input state | DCS (sixel) | APC (kitty) | `→ ANSI` encoder |
|---|---|---|---|---|---|---|---|---|---|
| `vt100 0.18.1` **in use** | MIT | 5.1k LOC, 2 deps | ✅ | ❌ | ❌ | ✅ mouse/CKM/keypad/paste/cursor, ❌ focus | ❌ | ❌ | **✅ `rows_diff`, `rows_formatted`** |
| `alacritty_terminal 0.26` | Apache-2.0 | 11.7k LOC, heavy deps | ✅ | ✅ | ✅ | ✅ incl. kitty keyboard + `EventListener` | ❌ | ❌ | ❌ (you write it) |
| `wezterm-term 0.1.0` | MIT | very large git subgraph | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ❌ |
| `vte 0.15` | Apache-2.0 OR MIT | 4.1k LOC | — | — | — | — | ✅ raw `hook/put/unhook` | ❌ (`SOS/PM/APC` discarded) | — |
| `zellij-server` panes | MIT | ~16k LOC | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `ansi-to-tui`, `ansi-parser` | MIT | small | parses rendered text, not a terminal | — | — | — | — | — | ❌ |

Two facts from that table drove the design: **nothing vendorable gives you a state →
ANSI encoder except `vt100`**, and **`APC` is unreachable through `vte`**, which is
why the graphics tap is a byte scanner of uncl's own rather than a parser hook.

---

## Not yet done

Two items, both optional by decision rather than by omission.

### The engine swap (`alacritty_terminal`)

`vt100` models text, SGR, cursor and input modes, and nothing else. Everything it
does not model, uncl works around — with a link store, a DCS answerer, a graphics
tap, and a scrollback-sized clone per frame. `alacritty_terminal` would replace those
workarounds with the engine's own state:

| Gap today | What would close it |
|---|---|
| Underline **styles and colours** (`SGR 4:x`, `58`/`59`) are dropped | `CellFlags` / `Color` |
| **Kitty keyboard** (`CSI > u`) is neither advertised nor encoded — `TERM=xterm-kitty` is therefore downgraded | `TermMode` + `KittyKeyboardFlags` |
| **Synchronised output** (`?2026`) is not honoured: uncl paints when it decides | `TermMode::SYNC_UPDATE` + `EventListener` |
| Hyperlink spans are **one row**, and die on repaint rather than living on the cell | the engine's own hyperlink storage |
| Pointer shape (`OSC 22`) and the rest of `EventListener` are hand-rolled or absent | `EventListener` |
| A **`Screen` clone carries the scrollback** every painted frame (~5–13 MB at 2,000 lines) | storing painted rows instead of diffing screens |

It is an engine swap, not a feature: it needs its own encoder (which uncl already
has), and it should not be attempted without the golden-frame corpus in place to
catch the cells it moves. The corpus is now in place, which is what makes the swap
safe to start whenever it is wanted.

### The Windows encoder matrix

The guards are in and `cfg`-gated, so they cannot affect a non-Windows build. What
has not been done is the part that needs hardware: validating the encoder matrix
(application cursor, SGR versus X10 mouse, ConPTY's own input quirks) on a real
console. Until then Windows is "builds and refuses gracefully", not "supported".

### Out of scope by design

Three things are deliberately *not* on the list above, and will not be done:

- **Passthrough takeover** (giving the child the real screen instead of compositing).
  Floating while a TUI runs is the product; takeover is the one thing that cannot be
  reconciled with it.
- **Re-encoding images** — decode, scale or crop in uncl. The bytes are carried
  through and the terminal does the scaling; that is the whole quality contract.
- **Images on Windows** — see above: they need a bundled ConPTY v2.

---

## Verifying by hand

The unit tests and the harness cover what can be asserted without a terminal. These
are the behaviours that need a person looking at a screen:

```sh
cargo test                        # 178 tests, including the golden frames
cargo build --release && ./target/release/uncl --doctor
```

**Frames and speed.** Idle at a prompt → `top -p $(pgrep uncl)` shows ~0% CPU. A
still `btop` in the float → `strace -e trace=write -c -p $(pgrep uncl)` over five
seconds shows a handful of writes, not thousands. `seq 1 200000` in the float → no
tearing, input stays responsive.

**Geometry and the float.** `btop`, `htop`, `nvim`, `lazygit`, `less`, `fzf` all draw
inside the border at the size `stty size` reports. Drag the border and the child
reflows live; move it with `Alt`+arrows or a border drag and the vacated area is
repainted with no smear. `Home` twice removes and restores it exactly. `exit` in the
float gives a fresh prompt and leaves the owner shell untouched.

**Input.** Click and wheel inside the float reach the child. Select text in `nvim`,
drag the pointer out of the float, release → the selection ends cleanly. `i` and
`Esc` in `nvim` switch the cursor to a beam and back. Paste 5 KB of text → it lands
in one write, not character by character.

**Links.** `printf '\e]8;;https://example.com\e\\example.com\e]8;;\e\\\n'`, then a
plain click in the float → the browser opens. The same link in the owner pane also
opens. `ls --hyperlink=always --color=always` → filenames are clickable. Dragging
from a link does not open anything.

**Images.** `wezterm imgcat`, `chafa --format=kitty`, `timg -pk` → the picture appears
inside the float at the prompt cell. Move the float → kitty re-places by id (one
line, not a re-send). `Home` twice → the image goes and comes back. Scrolling the
float back hides it and returning to live re-sends it, once.

**Scrollback.** `seq 1 500`, wheel up → the border reads `+3`, `+6`, …; typing
returns to live. `UNCL_SCROLLBACK=10000 uncl` scrolls far past 2,000 lines;
`UNCL_SCROLLBACK=0` makes the wheel do nothing.

**Queries and environment.** `printf '\eP$qm\e\\'` → `\eP1$r0m\e\\`, and after
`\e[1;31m` it reports that pen. `printf '\eP+q544e\e\\'` → the child's `TERM` in hex.
`echo $TERM` in the float is the host's own (under WezTerm, alacritty, foot) and
`xterm-256color` under kitty, screen or tmux, with `--doctor` explaining why.

**The clipboard.** Copy in the browser, then `printf '\e]52;c;?\e\\'` in the float →
the same text arrives; `vim`'s `"+p` works. Copy inside the float, paste inside the
float → no helper process starts.

**The bell and the title.** `printf '\a'` in the float is audible; in the owner shell
while the float is visible it is not. `printf '\e]2;hello\a'` puts the title in the
float's top border, and `nvim`/`ssh` set it themselves.
