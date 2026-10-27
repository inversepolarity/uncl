//! The golden-frame harness (report §8.1): one stream, two engines, one assertion.
//!
//! Every other test in this crate checks a *piece* — a row encoder, a border, a
//! diff. This one checks the whole claim those pieces exist to support: that a
//! frame uncl writes, replayed into a real terminal, leaves the terminal showing
//! what the child drew.
//!
//! The shape is deliberately dumb:
//!
//! 1. feed a byte stream to a `vt100` parser (the "source screen"),
//! 2. render it through uncl's emitter into a `Vec<u8>` (the "frame"),
//! 3. feed *the frame* to a fresh `vt100` parser (the "replayed screen"),
//! 4. require every cell of both grids — contents **and** attributes — to match.
//!
//! No terminal, no human, no golden PNGs to re-bless: `vt100` is both the oracle
//! and the terminal. That is what makes this cheap enough to run on every commit,
//! and it is the net the report asked for before S4 replaces the engine under it.
//!
//! Two properties make it a real test rather than a tautology:
//!
//! * the emitter is a *diffing* renderer, so the second phase feeds frames into a
//!   parser that has already seen the earlier ones — exactly what a terminal does,
//! * the corpus is byte streams a real program produces, including the ones that
//!   have historically broken this layer: wide characters, `REP`, autowrap off,
//!   alt-screen round trips, and an `OSC 8` span wrapped around ordinary cells.

use super::*;
use crate::constants::nz;

/// The grid every corpus entry is rendered at.
///
/// Wide enough that a wrapped row wraps, small enough that a full repaint is a
/// few hundred bytes: this harness runs on every `cargo test`.
const ROWS: u16 = 12;
const COLS: u16 = 40;

/// A recorded byte stream and the name it fails under.
struct Entry {
    name: &'static str,
    bytes: &'static [u8],
}

/// The corpus, in the order report §8.2 lists it.
///
/// Each entry is a stream a child can produce. None of them is checked against a
/// hand-written expected frame — the expectation is the *same screen*, which means
/// the corpus can grow without anyone blessing new output.
const CORPUS: &[Entry] = &[
    Entry {
        name: "plain text, CRLF",
        bytes: b"hello\r\nworld\r\nthird line",
    },
    Entry {
        name: "sgr: 16, 256 and truecolor",
        bytes: b"\x1b[1;31mred\x1b[0m \x1b[38;5;200mpalette\x1b[0m \x1b[48;2;1;2;3mrgb\x1b[0m",
    },
    Entry {
        name: "inverse bar, then reset",
        bytes: b"\x1b[7m\x1b[2Kstatus line\x1b[0m\r\n\x1b[0mnormal",
    },
    Entry {
        name: "wide characters",
        bytes: "日本語のテキスト\r\nテスト です".as_bytes(),
    },
    Entry {
        name: "combining marks and symbols",
        bytes: "cafe\u{301} na\u{ef}ve \u{2713} \u{2764}\u{fe0f}".as_bytes(),
    },
    Entry {
        name: "erase, delete and insert",
        bytes: b"abcdefgh\r\n\x1b[2Kgone\x1b[3D\x1b[2@XY\x1b[1P!",
    },
    Entry {
        name: "cursor moves within and across rows",
        bytes: b"\x1b[5;10Hmiddle\x1b[1;1Htop\x1b[12;40Hend",
    },
    Entry {
        name: "autowrap off",
        bytes: b"\x1b[?7lignoring-the-margin-this-line-is-far-too-long\x1b[?7h",
    },
    Entry {
        name: "carriage return overwrite (a progress bar)",
        bytes: b"  0%\r 50%\r100% done",
    },
    Entry {
        name: "tabs, backspace and a bell",
        bytes: b"a\tb\x08c\r\n\x07after the bell",
    },
    Entry {
        name: "REP repeats a cell",
        bytes: b"\x1b[1mx\x1b[9b\x1b[0m and \x1b[44m \x1b[4b\x1b[0m",
    },
    Entry {
        name: "alternate screen round trip",
        bytes: b"primary\r\n\x1b[?1049hfull screen app\x1b[2J\x1b[Halt screen\x1b[?1049lback",
    },
    Entry {
        name: "OSC 8 span around ordinary cells",
        bytes: b"\x1b]8;;https://example.com\x1b\\click me\x1b]8;;\x1b\\ after",
    },
    Entry {
        name: "hidden cursor and a DECSCUSR cursor style",
        bytes: b"\x1b[4 q\x1b[?25lno cursor here",
    },
    Entry {
        name: "scroll at the bottom of the grid",
        bytes: b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\neight\r\nnine\r\nten\r\neleven\r\ntwelve\r\nthirteen\r\nfourteen",
    },
];

fn source_for(entry: &Entry) -> vt100::Parser {
    let mut parser = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    parser.process(entry.bytes);
    parser
}

fn owner_area() -> Rect {
    Rect::new(0, 0, COLS, ROWS)
}

/// Render one screen the way `Container::draw` would, with no float.
fn frame_for(renderer: &mut Renderer, screen: &vt100::Screen) -> Vec<u8> {
    renderer
        .render(Panes {
            owner: screen.clone(),
            owner_area: owner_area(),
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
            scroll: 0,
            focused_scrolled: false,
            float_title: None,
        })
        .to_vec()
}

/// Every cell and the cursor, compared cell by cell.
///
/// Cell-by-cell rather than a formatted-string diff on purpose: the pen a frame
/// leaves behind is not part of what the user sees, and a string comparison of
/// `contents_formatted` would fail on that while missing nothing real.
fn assert_same_screen(name: &str, want: &vt100::Screen, got: &vt100::Screen, frame: &[u8]) {
    assert_eq!(
        want.size(),
        got.size(),
        "{name}: the frame changed the grid size"
    );

    for row in 0..ROWS {
        for col in 0..COLS {
            let want_cell = want.cell(row, col);
            let got_cell = got.cell(row, col);
            assert_eq!(
                want_cell,
                got_cell,
                "{name}: cell ({row}, {col}) differs\n  want: {want_cell:?}\n  got:  {got_cell:?}\n  text: {:?}\n  frame: {:?}",
                want.contents(),
                String::from_utf8_lossy(frame),
            );
        }
    }

    assert_eq!(
        want.cursor_position(),
        got.cursor_position(),
        "{name}: the cursor landed in the wrong cell"
    );
}

#[test]
fn every_corpus_stream_replays_to_the_screen_it_drew() {
    for entry in CORPUS {
        let source = source_for(entry);

        let mut renderer = Renderer::new();
        let frame = frame_for(&mut renderer, source.screen());
        assert!(
            !frame.is_empty(),
            "{}: the first frame painted nothing",
            entry.name
        );

        let mut replayed = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
        replayed.process(&frame);
        assert_same_screen(entry.name, source.screen(), replayed.screen(), &frame);
    }
}

/// The same corpus, but grown one frame at a time: this is the *diff* path, and
/// it is where clipping and repaint bugs actually live.
#[test]
fn a_growing_corpus_replays_frame_by_frame() {
    // Each step is appended to the stream; every frame is applied to a terminal
    // that has already seen the previous ones, exactly like a real one.
    let steps: &[&[u8]] = &[
        b"$ cargo build",
        b"\r\n    Compiling uncl v0.1.0",
        b"\r\n\x1b[32m    Finished\x1b[0m dev profile",
        b"\r\n\x1b[2K\x1b[1;33mwarning\x1b[0m: unused variable `x`",
        b"\r\n",
    ];

    let mut source = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    let mut replayed = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    let mut renderer = Renderer::new();

    for (index, step) in steps.iter().enumerate() {
        source.process(step);
        let frame = frame_for(&mut renderer, source.screen());
        assert!(
            !frame.is_empty(),
            "step {index}: a changed screen produced an empty frame"
        );
        replayed.process(&frame);
        assert_same_screen(
            &format!("incremental step {index}"),
            source.screen(),
            replayed.screen(),
            &frame,
        );
    }

    // And an unchanged screen must cost nothing at all.
    let quiet = frame_for(&mut renderer, source.screen());
    assert!(quiet.is_empty(), "the steady state painted: {quiet:?}");
}

/// A screen that is painted, resized, and painted again.
///
/// Resizing the float is a `set_size` on the parser, which pulls rows out of
/// scrollback; the frame after it has no `prev` of the same shape, so it has to be
/// a full repaint. Nothing else in the suite covers that transition end to end.
#[test]
fn a_resize_repaints_and_the_result_replays() {
    let mut source = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    for line in 1..=ROWS {
        source.process(format!("row {line}\r\n").as_bytes());
    }

    let mut renderer = Renderer::new();
    let mut replayed = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    let first = frame_for(&mut renderer, source.screen());
    replayed.process(&first);
    assert_same_screen("before the resize", source.screen(), replayed.screen(), &first);

    // The float grew: same parser, larger grid.
    source.set_size(nz(ROWS), nz(COLS - 10));
    let area = Rect::new(0, 0, COLS - 10, ROWS);
    let frame = renderer
        .render(Panes {
            owner: source.screen().clone(),
            owner_area: area,
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
            scroll: 0,
            focused_scrolled: false,
            float_title: None,
        })
        .to_vec();
    assert!(!frame.is_empty(), "a resize must repaint");

    let mut after = vt100::Parser::new(nz(ROWS), nz(COLS - 10), 0);
    after.process(&frame);
    // Only the columns that exist in both are compared: the narrower grid is the
    // whole point of the resize.
    for row in 0..ROWS {
        for col in 0..COLS - 10 {
            assert_eq!(
                source.screen().cell(row, col),
                after.screen().cell(row, col),
                "resized cell ({row}, {col}) differs"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The float, links and images — the three things the owner-only corpus above
// cannot see.
//
// The point of each of these is the same: whatever *else* the frame does (draw a
// border, wrap a row in `OSC 8`, place an image), the grid a terminal ends up
// holding must still be the grid the child drew. A link that desyncs a row, or an
// image that lands on a cell, would pass every string-level test in `mod.rs` and
// fail here.
// ---------------------------------------------------------------------------

/// The float's rectangle, inside the corpus grid.
fn float_rect() -> Rect {
    Rect::new(4, 2, 20, 7)
}

#[test]
fn a_float_replays_over_the_owner_grid() {
    let mut owner = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    owner.process(b"owner line one\r\nowner line two\r\n$ ");
    let mut float = vt100::Parser::new(nz(5), nz(18), 0);
    float.process(b"floating:\r\n\x1b[1;33myellow\x1b[0m\r\n日本語");

    let rect = float_rect();
    let content = content_rect(rect);

    let mut renderer = Renderer::new();
    let frame = renderer
        .render(Panes {
            owner: owner.screen().clone(),
            owner_area: owner_area(),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: None,
            links: FrameLinks::default(),
            scroll: 0,
            focused_scrolled: false,
            float_title: None,
        })
        .to_vec();
    assert!(!frame.is_empty());

    let mut replayed = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    replayed.process(&frame);

    // Inside the float: the child's cells, exactly.
    for row in 0..content.height {
        for col in 0..content.width {
            assert_eq!(
                float.screen().cell(row, col),
                replayed.screen().cell(content.y + row, content.x + col),
                "float cell ({row}, {col}) differs\n  frame: {:?}",
                String::from_utf8_lossy(&frame)
            );
        }
    }

    // Outside it: the owner's cells, except where the float's border covers them.
    for row in 0..ROWS {
        for col in 0..COLS {
            if rect.contains(col, row) {
                continue;
            }
            assert_eq!(
                owner.screen().cell(row, col),
                replayed.screen().cell(row, col),
                "owner cell ({row}, {col}) was not preserved\n  frame: {:?}",
                String::from_utf8_lossy(&frame)
            );
        }
    }

    // And the frame is a frame: the corners are there.
    let text = String::from_utf8_lossy(&frame);
    assert!(text.contains('┌') && text.contains('┘'), "{text:?}");
}

#[test]
fn a_linked_row_replays_to_the_same_cells() {
    // `OSC 8` is emitted *inside* a row, between column-ranged segments. If those
    // segments were computed wrongly, the row would shift — the cells would still
    // be painted, in the wrong place. Only a replay can see that.
    let mut parser = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    parser.process(b"before ");
    let mut links = Links::new();
    links.osc(parser.screen(), b"", b"https://example.com/a");
    parser.process(b"click me");
    links.osc(parser.screen(), b"", b"");
    parser.process(b" after\r\nsecond row");
    links.osc(parser.screen(), b"", b"file:///tmp/log");
    parser.process(b" tail");

    let mut renderer = Renderer::new();
    let frame = renderer
        .render(Panes {
            owner: parser.screen().clone(),
            owner_area: owner_area(),
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks {
                owner: links,
                float: Links::new(),
            },
            scroll: 0,
            focused_scrolled: false,
            float_title: None,
        })
        .to_vec();

    let text = String::from_utf8_lossy(&frame);
    assert!(text.contains("\x1b]8;;https://example.com/a\x1b\\"), "{text:?}");

    let mut replayed = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    replayed.process(&frame);
    for row in 0..ROWS {
        for col in 0..COLS {
            assert_eq!(
                parser.screen().cell(row, col),
                replayed.screen().cell(row, col),
                "linked cell ({row}, {col}) differs — the segments shifted the row"
            );
        }
    }
}

#[test]
fn an_image_does_not_disturb_the_grid() {
    use crate::app::images::{Ingest, Kind, Store};

    // An image can only ever be placed inside the float's content rect (that is
    // the whole point of S3's geometry law), so this has to be a float frame — and
    // that makes the assertion stronger: the payload must not disturb the float's
    // cells *or* the owner's.
    let mut owner = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    owner.process(b"owner text\r\nbehind the float");
    let rect = float_rect();
    let content = content_rect(rect);

    let mut float = vt100::Parser::new(nz(content.height), nz(content.width), 0);
    float.process(b"\x1b[1mcaption\x1b[0m\r\nunder the picture");

    let mut store = Store::new();
    assert!(matches!(
        store.ingest(
            Kind::Apc,
            b"\x1b_Gf=24,a=T,i=7;QUJD\x1b\\",
            (1, 2),
            (content.width, content.height)
        ),
        Ingest::Stored
    ));

    let mut renderer = Renderer::new();
    let frame = renderer
        .render(Panes {
            owner: owner.screen().clone(),
            owner_area: owner_area(),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
            scroll: 0,
            focused_scrolled: false,
            float_title: None,
        })
        .to_vec();

    // The payload is in the frame…
    let text = String::from_utf8_lossy(&frame);
    assert!(text.contains("\x1b_G"), "{text:?}");

    // …and a terminal that ignores it entirely (which is what `vt100` does, and
    // what any terminal without kitty graphics does) still shows the child's cells.
    let mut replayed = vt100::Parser::new(nz(ROWS), nz(COLS), 0);
    replayed.process(&frame);

    for row in 0..content.height {
        for col in 0..content.width {
            assert_eq!(
                float.screen().cell(row, col),
                replayed.screen().cell(content.y + row, content.x + col),
                "float cell ({row}, {col}) was disturbed by the image"
            );
        }
    }
    for row in 0..ROWS {
        for col in 0..COLS {
            if rect.contains(col, row) {
                continue;
            }
            assert_eq!(
                owner.screen().cell(row, col),
                replayed.screen().cell(row, col),
                "owner cell ({row}, {col}) was disturbed by the image"
            );
        }
    }
}
