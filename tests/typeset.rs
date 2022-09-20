use std::cell::RefCell;
use std::collections::{hash_map::Entry, HashMap};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::hash::Hash;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use elsa::FrozenVec;
use same_file::Handle;
use siphasher::sip128::{Hasher128, SipHasher};
use tiny_skia as sk;
use unscanny::Scanner;
use walkdir::WalkDir;

use typst::eval::{Smart, Value};
use typst::font::{Font, FontBook};
use typst::frame::{Element, Frame};
use typst::geom::{Length, RgbaColor, Sides};
use typst::library::layout::PageNode;
use typst::library::text::{TextNode, TextSize};
use typst::model::StyleMap;
use typst::source::{Source, SourceId};
use typst::syntax::SyntaxNode;
use typst::util::Buffer;
use typst::{bail, Config, World};

const TYP_DIR: &str = "./typ";
const REF_DIR: &str = "./ref";
const PNG_DIR: &str = "./png";
const PDF_DIR: &str = "./pdf";
const FONT_DIR: &str = "../fonts";

fn main() {
    env::set_current_dir(env::current_dir().unwrap().join("tests")).unwrap();

    let args = Args::new(env::args().skip(1));
    let mut filtered = Vec::new();

    // Since differents tests can affect each other through the memoization
    // cache, a deterministic order is important for reproducibility.
    for entry in WalkDir::new(".").sort_by_file_name() {
        let entry = entry.unwrap();
        if entry.depth() <= 1 {
            continue;
        }

        let src_path = entry.into_path();
        if src_path.extension() != Some(OsStr::new("typ")) {
            continue;
        }

        if args.matches(&src_path) {
            filtered.push(src_path);
        }
    }

    let len = filtered.len();
    if len == 1 {
        println!("Running test ...");
    } else if len > 1 {
        println!("Running {len} tests");
    }

    // Create loader and context.
    let mut world = TestWorld::new(args.print);

    // Run all the tests.
    let mut ok = 0;
    for src_path in filtered {
        let path = src_path.strip_prefix(TYP_DIR).unwrap();
        let png_path = Path::new(PNG_DIR).join(path).with_extension("png");
        let ref_path = Path::new(REF_DIR).join(path).with_extension("png");
        let pdf_path =
            args.pdf.then(|| Path::new(PDF_DIR).join(path).with_extension("pdf"));

        ok += test(
            &mut world,
            &src_path,
            &png_path,
            &ref_path,
            pdf_path.as_deref(),
        ) as usize;
    }

    if len > 1 {
        println!("{ok} / {len} tests passed.");
    }

    if ok < len {
        std::process::exit(1);
    }
}

/// Parsed command line arguments.
struct Args {
    filter: Vec<String>,
    exact: bool,
    pdf: bool,
    print: PrintConfig,
}

/// Which things to print out for debugging.
#[derive(Default, Copy, Clone, Eq, PartialEq)]
struct PrintConfig {
    syntax: bool,
    frames: bool,
}

impl Args {
    fn new(args: impl Iterator<Item = String>) -> Self {
        let mut filter = Vec::new();
        let mut exact = false;
        let mut pdf = false;
        let mut print = PrintConfig::default();

        for arg in args {
            match arg.as_str() {
                // Ignore this, its for cargo.
                "--nocapture" => {}
                // Match only the exact filename.
                "--exact" => exact = true,
                // Generate PDFs.
                "--pdf" => pdf = true,
                // Debug print the syntax trees.
                "--syntax" => print.syntax = true,
                // Debug print the frames.
                "--frames" => print.frames = true,
                // Everything else is a file filter.
                _ => filter.push(arg),
            }
        }

        Self { filter, exact, pdf, print }
    }

    fn matches(&self, path: &Path) -> bool {
        if self.exact {
            let name = path.file_name().unwrap().to_string_lossy();
            self.filter.iter().any(|v| v == &name)
        } else {
            let path = path.to_string_lossy();
            self.filter.is_empty() || self.filter.iter().any(|v| path.contains(v))
        }
    }
}

struct TestWorld {
    config: Config,
    print: PrintConfig,
    sources: FrozenVec<Box<Source>>,
    nav: RefCell<HashMap<PathHash, SourceId>>,
    book: FontBook,
    fonts: Vec<Font>,
    files: RefCell<HashMap<PathHash, Buffer>>,
}

impl TestWorld {
    fn new(print: PrintConfig) -> Self {
        // Set page width to 120pt with 10pt margins, so that the inner page is
        // exactly 100pt wide. Page height is unbounded and font size is 10pt so
        // that it multiplies to nice round numbers.
        let mut styles = StyleMap::new();
        styles.set(PageNode::WIDTH, Smart::Custom(Length::pt(120.0).into()));
        styles.set(PageNode::HEIGHT, Smart::Auto);
        styles.set(
            PageNode::MARGINS,
            Sides::splat(Some(Smart::Custom(Length::pt(10.0).into()))),
        );
        styles.set(TextNode::SIZE, TextSize(Length::pt(10.0).into()));

        // Hook up helpers into the global scope.
        let mut std = typst::library::new();
        std.define("conifer", RgbaColor::new(0x9f, 0xEB, 0x52, 0xFF));
        std.define("forest", RgbaColor::new(0x43, 0xA1, 0x27, 0xFF));
        std.def_fn("test", move |_, args| {
            let lhs = args.expect::<Value>("left-hand side")?;
            let rhs = args.expect::<Value>("right-hand side")?;
            if lhs != rhs {
                bail!(args.span, "Assertion failed: {:?} != {:?}", lhs, rhs,);
            }
            Ok(Value::None)
        });
        std.def_fn("print", move |_, args| {
            print!("> ");
            for (i, value) in args.all::<Value>()?.into_iter().enumerate() {
                if i > 0 {
                    print!(", ")
                }
                print!("{value:?}");
            }
            println!();
            Ok(Value::None)
        });

        let mut fonts = vec![];
        for entry in WalkDir::new(FONT_DIR)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|entry| entry.file_type().is_file())
        {
            let buffer: Buffer = fs::read(entry.path()).unwrap().into();
            for index in 0 .. ttf_parser::fonts_in_collection(&buffer).unwrap_or(1) {
                fonts.push(Font::new(buffer.clone(), index).unwrap())
            }
        }

        Self {
            config: Config { root: PathBuf::new(), std, styles },
            print,
            sources: FrozenVec::new(),
            nav: RefCell::new(HashMap::new()),
            book: FontBook::from_fonts(&fonts),
            fonts,
            files: RefCell::new(HashMap::new()),
        }
    }

    fn provide(&mut self, path: &Path, text: String) -> SourceId {
        let hash = PathHash::new(path).unwrap();
        if let Some(&id) = self.nav.borrow().get(&hash) {
            self.sources.as_mut()[id.into_raw() as usize].replace(text);
            return id;
        }

        let id = SourceId::from_raw(self.sources.len() as u16);
        let source = Source::new(id, path, text);
        self.sources.push(Box::new(source));
        self.nav.borrow_mut().insert(hash, id);
        id
    }
}

impl World for TestWorld {
    fn config(&self) -> &Config {
        &self.config
    }

    fn resolve(&self, path: &Path) -> io::Result<SourceId> {
        let hash = PathHash::new(path)?;
        if let Some(&id) = self.nav.borrow().get(&hash) {
            return Ok(id);
        }

        let data = fs::read(path)?;
        let text = String::from_utf8(data).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "file is not valid utf-8")
        })?;

        let id = SourceId::from_raw(self.sources.len() as u16);
        let source = Source::new(id, path, text);
        self.sources.push(Box::new(source));
        self.nav.borrow_mut().insert(hash, id);

        Ok(id)
    }

    fn source(&self, id: SourceId) -> &Source {
        &self.sources[id.into_raw() as usize]
    }

    fn book(&self) -> &FontBook {
        &self.book
    }

    fn font(&self, id: usize) -> io::Result<Font> {
        Ok(self.fonts[id].clone())
    }

    fn file(&self, path: &Path) -> io::Result<Buffer> {
        let hash = PathHash::new(path)?;
        Ok(match self.files.borrow_mut().entry(hash) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => entry.insert(fs::read(path)?.into()).clone(),
        })
    }
}

/// A hash that is the same for all paths pointing to the same file.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
struct PathHash(u128);

impl PathHash {
    fn new(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        if file.metadata()?.is_file() {
            let handle = Handle::from_file(file)?;
            let mut state = SipHasher::new();
            handle.hash(&mut state);
            Ok(Self(state.finish128().as_u128()))
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }
}

fn test(
    world: &mut TestWorld,
    src_path: &Path,
    png_path: &Path,
    ref_path: &Path,
    pdf_path: Option<&Path>,
) -> bool {
    let name = src_path.strip_prefix(TYP_DIR).unwrap_or(src_path);
    println!("Testing {}", name.display());

    let text = fs::read_to_string(src_path).unwrap();

    let mut ok = true;
    let mut frames = vec![];
    let mut line = 0;
    let mut compare_ref = true;
    let mut compare_ever = false;
    let mut rng = LinearShift::new();

    let parts: Vec<_> = text.split("\n---").collect();
    for (i, &part) in parts.iter().enumerate() {
        let is_header = i == 0
            && parts.len() > 1
            && part
                .lines()
                .all(|s| s.starts_with("//") || s.chars().all(|c| c.is_whitespace()));

        if is_header {
            for line in part.lines() {
                if line.starts_with("// Ref: false") {
                    compare_ref = false;
                }
            }
        } else {
            let (part_ok, compare_here, part_frames) =
                test_part(world, src_path, part.into(), i, compare_ref, line, &mut rng);
            ok &= part_ok;
            compare_ever |= compare_here;
            frames.extend(part_frames);
        }

        line += part.lines().count() + 1;
    }

    if compare_ever {
        if let Some(pdf_path) = pdf_path {
            let pdf_data = typst::export::pdf(&frames);
            fs::create_dir_all(&pdf_path.parent().unwrap()).unwrap();
            fs::write(pdf_path, pdf_data).unwrap();
        }

        if world.print.frames {
            for frame in &frames {
                println!("Frame: {:#?}", frame);
            }
        }

        let canvas = render(&frames);
        fs::create_dir_all(&png_path.parent().unwrap()).unwrap();
        canvas.save_png(png_path).unwrap();

        if let Ok(ref_pixmap) = sk::Pixmap::load_png(ref_path) {
            if canvas.width() != ref_pixmap.width()
                || canvas.height() != ref_pixmap.height()
                || canvas
                    .data()
                    .iter()
                    .zip(ref_pixmap.data())
                    .any(|(&a, &b)| a.abs_diff(b) > 2)
            {
                println!("  Does not match reference image. ❌");
                ok = false;
            }
        } else if !frames.is_empty() {
            println!("  Failed to open reference image. ❌");
            ok = false;
        }
    }

    if ok {
        if world.print == PrintConfig::default() {
            print!("\x1b[1A");
        }
        println!("Testing {} ✔", name.display());
    }

    ok
}

fn test_part(
    world: &mut TestWorld,
    src_path: &Path,
    text: String,
    i: usize,
    compare_ref: bool,
    line: usize,
    rng: &mut LinearShift,
) -> (bool, bool, Vec<Frame>) {
    let mut ok = true;

    let id = world.provide(src_path, text);
    let source = world.source(id);
    if world.print.syntax {
        println!("Syntax Tree: {:#?}", source.root())
    }

    let (local_compare_ref, mut ref_errors) = parse_metadata(&source);
    let compare_ref = local_compare_ref.unwrap_or(compare_ref);

    ok &= test_spans(source.root());
    ok &= test_reparse(world.source(id).text(), i, rng);

    let (mut frames, errors) = match typst::typeset(world, id) {
        Ok(frames) => (frames, vec![]),
        Err(errors) => (vec![], *errors),
    };

    // Don't retain frames if we don't wanna compare with reference images.
    if !compare_ref {
        frames.clear();
    }

    // Map errors to range and message format, discard traces and errors from
    // other files.
    let mut errors: Vec<_> = errors
        .into_iter()
        .filter(|error| error.span.source() == id)
        .map(|error| {
            let range = world.source(error.span.source()).range(error.span);
            let msg = error.message.replace("\\", "/");
            (range, msg)
        })
        .collect();

    errors.sort_by_key(|error| error.0.start);
    ref_errors.sort_by_key(|error| error.0.start);

    if errors != ref_errors {
        println!("  Subtest {i} does not match expected errors. ❌");
        ok = false;

        let source = world.source(id);
        for error in errors.iter() {
            if !ref_errors.contains(error) {
                print!("    Not annotated | ");
                print_error(&source, line, error);
            }
        }

        for error in ref_errors.iter() {
            if !errors.contains(error) {
                print!("    Not emitted   | ");
                print_error(&source, line, error);
            }
        }
    }

    (ok, compare_ref, frames)
}

fn parse_metadata(source: &Source) -> (Option<bool>, Vec<(Range<usize>, String)>) {
    let mut compare_ref = None;
    let mut errors = vec![];

    let lines: Vec<_> = source.text().lines().map(str::trim).collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("// Ref: false") {
            compare_ref = Some(false);
        }

        if line.starts_with("// Ref: true") {
            compare_ref = Some(true);
        }

        let rest = if let Some(rest) = line.strip_prefix("// Error: ") {
            rest
        } else {
            continue;
        };

        fn num(s: &mut Scanner) -> usize {
            s.eat_while(char::is_numeric).parse().unwrap()
        }

        let comments =
            lines[i ..].iter().take_while(|line| line.starts_with("//")).count();

        let pos = |s: &mut Scanner| -> usize {
            let first = num(s) - 1;
            let (delta, column) =
                if s.eat_if(':') { (first, num(s) - 1) } else { (0, first) };
            let line = (i + comments) + delta;
            source.line_column_to_byte(line, column).unwrap()
        };

        let mut s = Scanner::new(rest);
        let start = pos(&mut s);
        let end = if s.eat_if('-') { pos(&mut s) } else { start };
        let range = start .. end;

        errors.push((range, s.after().trim().to_string()));
    }

    (compare_ref, errors)
}

fn print_error(source: &Source, line: usize, (range, message): &(Range<usize>, String)) {
    let start_line = 1 + line + source.byte_to_line(range.start).unwrap();
    let start_col = 1 + source.byte_to_column(range.start).unwrap();
    let end_line = 1 + line + source.byte_to_line(range.end).unwrap();
    let end_col = 1 + source.byte_to_column(range.end).unwrap();
    println!("Error: {start_line}:{start_col}-{end_line}:{end_col}: {message}");
}

/// Pseudorandomly edit the source file and test whether a reparse produces the
/// same result as a clean parse.
///
/// The method will first inject 10 strings once every 400 source characters
/// and then select 5 leaf node boundries to inject an additional, randomly
/// chosen string from the injection list.
fn test_reparse(text: &str, i: usize, rng: &mut LinearShift) -> bool {
    let supplements = [
        "[",
        "]",
        "{",
        "}",
        "(",
        ")",
        "#rect()",
        "a word",
        ", a: 1",
        "10.0",
        ":",
        "if i == 0 {true}",
        "for",
        "* hello *",
        "//",
        "/*",
        "\\u{12e4}",
        "```typst",
        " ",
        "trees",
        "\\",
        "$ a $",
        "2.",
        "-",
        "5",
    ];

    let mut ok = true;

    let apply = |replace: std::ops::Range<usize>, with| {
        let mut incr_source = Source::detached(text);
        if incr_source.root().len() != text.len() {
            println!(
                "    Subtest {i} tree length {} does not match string length {} ❌",
                incr_source.root().len(),
                text.len(),
            );
            return false;
        }

        incr_source.edit(replace.clone(), with);

        let edited_src = incr_source.text();
        let incr_root = incr_source.root();
        let ref_source = Source::detached(edited_src);
        let ref_root = ref_source.root();
        let mut ok = incr_root == ref_root;
        if !ok {
            println!(
                "    Subtest {i} reparse differs from clean parse when inserting '{with}' at {}-{} ❌\n",
                replace.start, replace.end,
            );
            println!("    Expected reference tree:\n{ref_root:#?}\n");
            println!("    Found incremental tree:\n{incr_root:#?}");
            println!(
                "    Full source ({}):\n\"{edited_src:?}\"",
                edited_src.len()
            );
        }

        ok &= test_spans(ref_root);
        ok &= test_spans(incr_root);
        ok
    };

    let mut pick = |range: Range<usize>| {
        let ratio = rng.next();
        (range.start as f64 + ratio * (range.end - range.start) as f64).floor() as usize
    };

    let insertions = (text.len() as f64 / 400.0).ceil() as usize;
    for _ in 0 .. insertions {
        let supplement = supplements[pick(0 .. supplements.len())];
        let start = pick(0 .. text.len());
        let end = pick(start .. text.len());

        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }

        ok &= apply(start .. end, supplement);
    }

    let source = Source::detached(text);
    let leafs = source.root().leafs();
    let start = source.range(leafs[pick(0 .. leafs.len())].span()).start;
    let supplement = supplements[pick(0 .. supplements.len())];
    ok &= apply(start .. start, supplement);

    ok
}

/// Ensure that all spans are properly ordered (and therefore unique).
#[track_caller]
fn test_spans(root: &SyntaxNode) -> bool {
    test_spans_impl(root, 0 .. u64::MAX)
}

#[track_caller]
fn test_spans_impl(node: &SyntaxNode, within: Range<u64>) -> bool {
    if !within.contains(&node.span().number()) {
        eprintln!("    Node: {node:#?}");
        eprintln!(
            "    Wrong span order: {} not in {within:?} ❌",
            node.span().number(),
        );
    }

    let start = node.span().number() + 1;
    let mut children = node.children().peekable();
    while let Some(child) = children.next() {
        let end = children.peek().map_or(within.end, |next| next.span().number());
        if !test_spans_impl(child, start .. end) {
            return false;
        }
    }

    true
}

/// Draw all frames into one image with padding in between.
fn render(frames: &[Frame]) -> sk::Pixmap {
    let pixel_per_pt = 2.0;
    let pixmaps: Vec<_> = frames
        .iter()
        .map(|frame| {
            let limit = Length::cm(100.0);
            if frame.width() > limit || frame.height() > limit {
                panic!("overlarge frame: {:?}", frame.size());
            }
            typst::export::render(frame, pixel_per_pt)
        })
        .collect();

    let pad = (5.0 * pixel_per_pt).round() as u32;
    let pxw = 2 * pad + pixmaps.iter().map(sk::Pixmap::width).max().unwrap_or_default();
    let pxh = pad + pixmaps.iter().map(|pixmap| pixmap.height() + pad).sum::<u32>();

    let mut canvas = sk::Pixmap::new(pxw, pxh).unwrap();
    canvas.fill(sk::Color::BLACK);

    let [x, mut y] = [pad; 2];
    for (frame, mut pixmap) in frames.iter().zip(pixmaps) {
        let ts = sk::Transform::from_scale(pixel_per_pt, pixel_per_pt);
        render_links(&mut pixmap, ts, frame);

        canvas.draw_pixmap(
            x as i32,
            y as i32,
            pixmap.as_ref(),
            &sk::PixmapPaint::default(),
            sk::Transform::identity(),
            None,
        );

        y += pixmap.height() + pad;
    }

    canvas
}

/// Draw extra boxes for links so we can see whether they are there.
fn render_links(canvas: &mut sk::Pixmap, ts: sk::Transform, frame: &Frame) {
    for (pos, element) in frame.elements() {
        let ts = ts.pre_translate(pos.x.to_pt() as f32, pos.y.to_pt() as f32);
        match *element {
            Element::Group(ref group) => {
                let ts = ts.pre_concat(group.transform.into());
                render_links(canvas, ts, &group.frame);
            }
            Element::Link(_, size) => {
                let w = size.x.to_pt() as f32;
                let h = size.y.to_pt() as f32;
                let rect = sk::Rect::from_xywh(0.0, 0.0, w, h).unwrap();
                let mut paint = sk::Paint::default();
                paint.set_color_rgba8(40, 54, 99, 40);
                canvas.fill_rect(rect, &paint, ts, None);
            }
            _ => {}
        }
    }
}

/// A Linear-feedback shift register using XOR as its shifting function.
/// Can be used as PRNG.
struct LinearShift(u64);

impl LinearShift {
    /// Initialize the shift register with a pre-set seed.
    pub fn new() -> Self {
        Self(0xACE5)
    }

    /// Return a pseudo-random number between `0.0` and `1.0`.
    pub fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 3;
        self.0 ^= self.0 << 14;
        self.0 ^= self.0 >> 28;
        self.0 ^= self.0 << 36;
        self.0 ^= self.0 >> 52;
        self.0 as f64 / u64::MAX as f64
    }
}
