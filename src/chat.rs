//! KI-Chat im Startmenü: leichtes Markdown (Absätze, Listen, **fett**,
//! `Code`, Codeblöcke, Überschriften, Links) und das Layout der Unterhaltung.
//!
//! Hier wird nur gerechnet: Texte messen läuft über `Measure` (in start.rs
//! per GetTextExtentPoint32 auf dem Fenster-DC), gezeichnet wird dort auch.
//! So lassen sich Zerlegung und Umbruch ohne Fenster testen.

use windows_sys::Win32::Foundation::RECT;

use crate::util::px;

// ------------------------------------------------------------- Markdown

#[derive(Clone, Debug, PartialEq)]
pub enum Style {
    Plain,
    Bold,
    Code,
    /// Klickbarer Link (nur http/https).
    Link(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Para(Vec<Span>),
    Heading(Vec<Span>),
    /// Listenpunkt: Ebene (0 = oben), Marker („•“ oder „3.“), Inhalt.
    Item { level: usize, marker: String, spans: Vec<Span> },
    Code { lang: String, text: String },
    Quote(Vec<Span>),
    Rule,
}

fn span(text: &str, style: Style) -> Span {
    Span { text: text.to_string(), style }
}

/// Angehängtes Stück an die Spanliste; gleiche Stile werden zusammengelegt.
fn push_span(out: &mut Vec<Span>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(last) if last.style == style => last.text.push_str(text),
        _ => out.push(span(text, style)),
    }
}

/// Ende einer nackten URL: bis zum Leerraum, ohne Satzzeichen am Schluss.
fn url_len(s: &str) -> usize {
    let mut end = s.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`')).unwrap_or(s.len());
    while let Some(c) = s[..end].chars().last() {
        let strip = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' => true,
            // Klammer zu nur, wenn sie nicht zur URL gehört (Wikipedia-Links).
            ')' => s[..end].matches('(').count() < s[..end].matches(')').count(),
            _ => false,
        };
        if !strip {
            break;
        }
        end -= c.len_utf8();
    }
    end
}

fn is_url_start(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://")) && s.len() > 8
}

/// Zeileninhalt in Spans zerlegen: `**fett**`, `` `Code` ``, `[Text](url)`,
/// nackte http(s)-Links. `*kursiv*` wird ohne Sternchen normal gesetzt.
pub fn parse_inline(s: &str) -> Vec<Span> {
    let mut out = Vec::new();
    inline_into(s, &mut out, false);
    out
}

fn inline_into(s: &str, out: &mut Vec<Span>, bold: bool) {
    let plain = || if bold { Style::Bold } else { Style::Plain };
    let mut i = 0;
    let mut prev: Option<char> = None;
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().unwrap();
        // Maskiertes Zeichen: \* \` \_ …
        if c == '\\' {
            if let Some(n) = rest[1..].chars().next().filter(|n| n.is_ascii_punctuation()) {
                push_span(out, &n.to_string(), plain());
                i += 1 + n.len_utf8();
                prev = Some(n);
                continue;
            }
        }
        if c == '`' {
            let ticks = rest.len() - rest.trim_start_matches('`').len();
            let fence = &rest[..ticks];
            if let Some(end) = rest[ticks..].find(fence) {
                let inner = &rest[ticks..ticks + end];
                // `` ` `` – Leerzeichen am Rand gehören nicht zum Code.
                let inner = if inner.len() > 2 && inner.starts_with(' ') && inner.ends_with(' ') { &inner[1..inner.len() - 1] } else { inner };
                push_span(out, inner, Style::Code);
                i += ticks + end + ticks;
                prev = Some('`');
                continue;
            }
            push_span(out, fence, plain());
            i += ticks;
            continue;
        }
        if !bold && (rest.starts_with("**") || rest.starts_with("__")) {
            let mark = &rest[..2];
            if let Some(end) = rest[2..].find(mark).filter(|&e| e > 0) {
                let inner = &rest[2..2 + end];
                if !inner.starts_with(' ') && !inner.ends_with(' ') {
                    inline_into(inner, out, true);
                    i += 2 + end + 2;
                    prev = Some('*');
                    continue;
                }
            }
        }
        // *kursiv* / _kursiv_: Zeichen weglassen, Inhalt normal.
        if (c == '*' || c == '_') && !prev.is_some_and(|p| p.is_alphanumeric()) {
            let after = &rest[1..];
            if after.chars().next().is_some_and(|n| !n.is_whitespace() && n != c) {
                if let Some(end) = after.find(c).filter(|&e| e > 0) {
                    let inner = &after[..end];
                    let closes_word = !after[end + 1..].chars().next().is_some_and(|n| n.is_alphanumeric());
                    if !inner.ends_with(' ') && closes_word {
                        inline_into(inner, out, bold);
                        i += 1 + end + 1;
                        prev = Some(c);
                        continue;
                    }
                }
            }
        }
        if c == '[' {
            if let Some(close) = rest.find("](") {
                let label = &rest[1..close];
                let after = &rest[close + 2..];
                if let Some(paren) = after.find(')') {
                    let url = after[..paren].trim();
                    if is_url_start(url) && !label.contains('\n') {
                        let label = if label.trim().is_empty() { url } else { label };
                        let label: String = parse_inline(label).into_iter().map(|s| s.text).collect();
                        push_span(out, &label, Style::Link(url.to_string()));
                        i += close + 2 + paren + 1;
                        prev = Some(')');
                        continue;
                    }
                }
            }
        }
        if (c == 'h' || c == 'H') && !prev.is_some_and(|p| p.is_alphanumeric()) && is_url_start(rest) {
            let n = url_len(rest);
            if n > 8 {
                let url = &rest[..n];
                push_span(out, url, Style::Link(url.to_string()));
                i += n;
                prev = Some('/');
                continue;
            }
        }
        push_span(out, &rest[..c.len_utf8()], plain());
        i += c.len_utf8();
        prev = Some(c);
    }
}

/// Zeile ist ein Listenpunkt? → (Ebene, Marker, Rest).
fn list_item(line: &str) -> Option<(usize, String, &str)> {
    let indent = line.len() - line.trim_start().len();
    let t = line.trim_start();
    let level = (indent / 2).min(3);
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(m) {
            return Some((level, if level == 0 { "•" } else { "◦" }.to_string(), rest));
        }
    }
    let digits = t.len() - t.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 && digits <= 3 {
        let after = &t[digits..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return Some((level, format!("{}.", &t[..digits]), rest));
        }
    }
    None
}

fn is_rule(t: &str) -> bool {
    let t: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

/// Tabellenzeile `| a | b |` → Zellen; Trennzeilen (`|---|`) → leer.
fn table_cells(t: &str) -> Option<Vec<&str>> {
    if !t.starts_with('|') {
        return None;
    }
    let cells: Vec<&str> = t.trim_matches('|').split('|').map(str::trim).collect();
    if cells.iter().all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':'))) {
        return Some(Vec::new());
    }
    Some(cells)
}

/// Markdown-Text in Blöcke zerlegen. Ein nicht geschlossener Codeblock
/// reicht bis zum Ende (beim Streamen kommt der Abschluss erst später).
pub fn parse(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    // Offener Absatz bzw. Listenpunkt: Folgezeilen werden angehängt.
    let mut para: Option<String> = None;
    let mut item: Option<(usize, String, String)> = None;
    let flush = |blocks: &mut Vec<Block>, para: &mut Option<String>, item: &mut Option<(usize, String, String)>| {
        if let Some(p) = para.take() {
            blocks.push(Block::Para(parse_inline(&p)));
        }
        if let Some((level, marker, t)) = item.take() {
            blocks.push(Block::Item { level, marker, spans: parse_inline(&t) });
        }
    };
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            flush(&mut blocks, &mut para, &mut item);
            let fence = &t[..3];
            let indent = line.len() - line.trim_start().len();
            let lang = t.trim_start_matches(fence.chars().next().unwrap()).trim().to_string();
            let mut code = Vec::new();
            for l in lines.by_ref() {
                if l.trim().starts_with(fence) {
                    break;
                }
                // Einrückung des Zauns abziehen (Codeblock in einer Liste).
                let cut = l.len() - l.trim_start().len();
                code.push(&l[cut.min(indent)..]);
            }
            blocks.push(Block::Code { lang, text: code.join("\n") });
            continue;
        }
        if t.is_empty() {
            flush(&mut blocks, &mut para, &mut item);
            continue;
        }
        if let Some(h) = t.strip_prefix('#') {
            let h = h.trim_start_matches('#');
            if h.starts_with(' ') && t.len() - h.len() <= 6 {
                flush(&mut blocks, &mut para, &mut item);
                blocks.push(Block::Heading(parse_inline(h.trim().trim_end_matches('#').trim())));
                continue;
            }
        }
        if is_rule(t) {
            flush(&mut blocks, &mut para, &mut item);
            blocks.push(Block::Rule);
            continue;
        }
        if let Some((level, marker, rest)) = list_item(line) {
            flush(&mut blocks, &mut para, &mut item);
            item = Some((level, marker, rest.trim().to_string()));
            continue;
        }
        if let Some(q) = t.strip_prefix('>') {
            flush(&mut blocks, &mut para, &mut item);
            blocks.push(Block::Quote(parse_inline(q.trim())));
            continue;
        }
        if let Some(cells) = table_cells(t) {
            flush(&mut blocks, &mut para, &mut item);
            if !cells.is_empty() {
                blocks.push(Block::Para(parse_inline(&cells.join("  ·  "))));
            }
            continue;
        }
        // Folgezeile: an Listenpunkt oder Absatz anhängen (weicher Umbruch).
        if let Some((_, _, t2)) = item.as_mut() {
            t2.push(' ');
            t2.push_str(t);
        } else if let Some(p) = para.as_mut() {
            p.push(' ');
            p.push_str(t);
        } else {
            para = Some(t.to_string());
        }
    }
    flush(&mut blocks, &mut para, &mut item);
    blocks
}

// ---------------------------------------------------------------- Layout

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Font {
    Normal,
    /// Fett in normaler Größe (`**…**`).
    Bold,
    /// Überschrift.
    Heading,
    Mono,
    Small,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ink {
    Text,
    Sub,
    Error,
    Link(String),
}

/// Textmaße in Pixeln (in start.rs über GDI).
pub trait Measure {
    fn width(&self, font: Font, s: &str) -> i32;
    /// Zeilenhöhe der Schrift.
    fn height(&self, font: Font) -> i32;
    /// Oberlänge (Abstand Oberkante → Grundlinie).
    fn ascent(&self, font: Font) -> i32;
}

/// Ein Stück Text in einer Zeile.
#[derive(Clone)]
pub struct Run {
    pub rect: RECT,
    pub text: String,
    pub font: Font,
    pub ink: Ink,
    /// Inline-Code: bekommt eine eigene Hinterlegung.
    pub code: bool,
}

#[derive(Clone)]
pub enum Item {
    Run(Run),
    /// Hinterlegung einer eigenen Frage (rechtsbündig).
    Bubble(RECT),
    /// Hinterlegung eines Codeblocks.
    CodePanel(RECT),
    /// Kopier-Knopf; `label` = mit Beschriftung „Kopieren“.
    Copy { rect: RECT, text: String, label: bool },
    /// Listenmarker (• oder 3.).
    Marker { rect: RECT, text: String },
    Rule(RECT),
}

#[derive(Clone, Default)]
pub struct Layout {
    pub items: Vec<Item>,
    pub height: i32,
}

/// Eine Runde des Chats, wie sie gezeichnet wird.
pub struct Turn<'a> {
    pub question: &'a str,
    /// Zwischenschritte (z. B. Werkzeugaufrufe), klein über der Antwort.
    pub steps: &'a [String],
    pub text: &'a str,
    pub thinking: bool,
    pub done: bool,
    pub error: Option<&'a str>,
}

fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

fn shift(r: &mut RECT, dx: i32, dy: i32) {
    r.left += dx;
    r.right += dx;
    r.top += dy;
    r.bottom += dy;
}

/// Schrift eines Spans auf Basis der Blockschrift.
fn font_for(style: &Style, base: Font) -> Font {
    match (style, base) {
        (Style::Code, Font::Small) => Font::Small,
        (Style::Code, _) => Font::Mono,
        (Style::Bold, Font::Normal) => Font::Bold,
        _ => base,
    }
}

/// Wörter und Leerraum eines Spans.
fn tokens(s: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut space = None;
    for (i, c) in s.char_indices() {
        let ws = c.is_whitespace();
        if space.is_some_and(|sp| sp != ws) {
            out.push((&s[start..i], space.unwrap()));
            start = i;
        }
        space = Some(ws);
    }
    if let Some(sp) = space {
        out.push((&s[start..], sp));
    }
    out
}

/// Zu langes Wort in Stücke schneiden, die jeweils in `width` passen.
fn split_to_fit<'a>(m: &dyn Measure, font: Font, s: &'a str, first: i32, width: i32) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut room = first;
    let mut last_ok = 0;
    for (i, c) in s.char_indices() {
        let end = i + c.len_utf8();
        if m.width(font, &s[start..end]) > room && last_ok > start {
            out.push(&s[start..last_ok]);
            start = last_ok;
            room = width;
        }
        last_ok = end;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Spans in Zeilen der Breite `width` umbrechen, ab (x0, y).
/// Gibt die Runs und die Unterkante zurück.
pub fn wrap(m: &dyn Measure, spans: &[Span], base: Font, ink: &Ink, x0: i32, y: i32, width: i32, dpi: u32) -> (Vec<Run>, i32) {
    let pad = px(dpi, 4);
    let fonts: Vec<Font> = spans.iter().map(|s| font_for(&s.style, base)).collect();
    let line_h = fonts.iter().chain([&base]).map(|&f| m.height(f)).max().unwrap_or(0);
    let asc = fonts.iter().chain([&base]).map(|&f| m.ascent(f)).max().unwrap_or(0);
    let ink_of = |s: &Span| match &s.style {
        Style::Link(u) => Ink::Link(u.clone()),
        _ => ink.clone(),
    };

    let mut runs: Vec<Run> = Vec::new();
    let mut line: Vec<Run> = Vec::new();
    let mut y = y;
    let mut x = 0;
    // Code-Span am Zeilenende (für den Innenabstand der Hinterlegung).
    let mut prev_code: Option<usize> = None;
    let mut pending: Option<(usize, &str)> = None;

    let finish = |line: &mut Vec<Run>, runs: &mut Vec<Run>, y: i32| {
        if let Some(last) = line.last_mut() {
            let trimmed = last.text.trim_end().len();
            if trimmed < last.text.len() {
                last.text.truncate(trimmed);
                last.rect.right = last.rect.left + m.width(last.font, &last.text);
            }
        }
        for mut r in line.drain(..) {
            let top = y + asc - m.ascent(r.font);
            r.rect.top = top;
            r.rect.bottom = top + m.height(r.font);
            runs.push(r);
        }
    };
    let append = |line: &mut Vec<Run>, x: i32, text: &str, w: i32, font: Font, ink: Ink, code: bool| {
        if let Some(last) = line.last_mut() {
            if last.font == font && last.ink == ink && last.code == code && last.rect.right == x0 + x {
                last.text.push_str(text);
                last.rect.right += w;
                return;
            }
        }
        line.push(Run { rect: rect(x0 + x, 0, x0 + x + w, 0), text: text.to_string(), font, ink, code });
    };

    for (si, sp) in spans.iter().enumerate() {
        let font = fonts[si];
        let code = matches!(sp.style, Style::Code);
        let code_id = code.then_some(si);
        let ink = ink_of(sp);
        for (t, space) in tokens(&sp.text) {
            if space {
                if !line.is_empty() {
                    pending = Some((si, " "));
                }
                continue;
            }
            // Innenabstand beim Verlassen bzw. Betreten eines Code-Spans.
            let pads = |prev: Option<usize>| {
                let change = prev != code_id;
                (if change && prev.is_some() { pad } else { 0 }, if change && code_id.is_some() { pad } else { 0 })
            };
            let sp_w = pending.map_or(0, |(ps, s)| m.width(fonts[ps], s));
            let w = m.width(font, t);
            let (leave, enter) = pads(prev_code);
            if !line.is_empty() && x + leave + sp_w + enter + w > width {
                finish(&mut line, &mut runs, y);
                y += line_h;
                x = 0;
                prev_code = None;
                pending = None;
            }
            let (leave, enter) = pads(prev_code);
            x += leave;
            if let Some((ps, s)) = pending.take() {
                let pcode = matches!(spans[ps].style, Style::Code);
                let pw = m.width(fonts[ps], s);
                append(&mut line, x, s, pw, fonts[ps], ink_of(&spans[ps]), pcode);
                x += pw;
            }
            x += enter;
            if w <= width - x {
                append(&mut line, x, t, w, font, ink.clone(), code);
                x += w;
            } else {
                // Längeres Wort als die Zeile (URL, Pfad): zeichenweise brechen.
                let pieces = split_to_fit(m, font, t, width - x, width);
                for (n, piece) in pieces.iter().enumerate() {
                    if n > 0 {
                        finish(&mut line, &mut runs, y);
                        y += line_h;
                        x = 0;
                    }
                    let pw = m.width(font, piece);
                    append(&mut line, x, piece, pw, font, ink.clone(), code);
                    x += pw;
                }
            }
            prev_code = code_id;
        }
    }
    if !line.is_empty() {
        finish(&mut line, &mut runs, y);
        y += line_h;
    }
    (runs, y)
}

/// Code zeichenweise auf `width` umbrechen; Tabs werden zu vier Leerzeichen.
pub fn wrap_code(m: &dyn Measure, code: &str, width: i32) -> Vec<String> {
    let mut out = Vec::new();
    for line in code.split('\n') {
        let line = line.replace('\t', "    ");
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        for piece in split_to_fit(m, Font::Mono, line, width, width) {
            out.push(piece.to_string());
        }
    }
    out
}

/// Ganze Unterhaltung setzen. Koordinaten relativ zu (0, 0), Breite `width`.
pub fn layout(m: &dyn Measure, turns: &[Turn], width: i32, dpi: u32) -> Layout {
    let d = dpi;
    let mut items = Vec::new();
    let mut y = 0;
    for (n, turn) in turns.iter().enumerate() {
        if n > 0 {
            y += px(d, 20);
        }
        // Eigene Frage: rechtsbündige Blase, höchstens 80 % breit.
        let (bx, by) = (px(d, 12), px(d, 8));
        let max_w = width * 4 / 5 - 2 * bx;
        let q = turn.question.replace(['\r', '\n'], " ");
        let (mut runs, bottom) = wrap(m, &[span(q.trim(), Style::Plain)], Font::Normal, &Ink::Text, 0, y + by, max_w, d);
        let text_w = runs.iter().map(|r| r.rect.right).max().unwrap_or(0);
        let bubble = rect(width - text_w - 2 * bx, y, width, bottom + by);
        for r in &mut runs {
            shift(&mut r.rect, bubble.left + bx, 0);
        }
        items.push(Item::Bubble(bubble));
        items.extend(runs.into_iter().map(Item::Run));
        y = bubble.bottom + px(d, 12);

        for step in turn.steps {
            let spans = parse_inline(step);
            let (runs, bottom) = wrap(m, &spans, Font::Small, &Ink::Sub, px(d, 2), y, width - px(d, 2), d);
            items.extend(runs.into_iter().map(Item::Run));
            y = bottom + px(d, 2);
        }
        if !turn.steps.is_empty() {
            y += px(d, 6);
        }

        if turn.thinking && turn.text.trim().is_empty() {
            let (runs, bottom) = wrap(m, &[span("denkt nach …", Style::Plain)], Font::Normal, &Ink::Sub, 0, y, width, d);
            items.extend(runs.into_iter().map(Item::Run));
            y = bottom;
        } else {
            let text = if turn.done || turn.error.is_some() { turn.text.to_string() } else { format!("{} ▍", turn.text.trim_end()) };
            y = layout_blocks(m, &parse(&text), &mut items, y, width, d);
        }
        if let Some(e) = turn.error {
            if !turn.text.trim().is_empty() {
                y += px(d, 8);
            }
            let (runs, bottom) = wrap(m, &parse_inline(e), Font::Normal, &Ink::Error, 0, y, width, d);
            items.extend(runs.into_iter().map(Item::Run));
            y = bottom;
        }
        if turn.done && turn.error.is_none() && !turn.text.trim().is_empty() {
            y += px(d, 6);
            let w = m.width(Font::Small, "Kopieren") + px(d, 38);
            let r = rect(-px(d, 6), y, w - px(d, 6), y + px(d, 26));
            items.push(Item::Copy { rect: r, text: turn.text.trim().to_string(), label: true });
            y = r.bottom;
        }
    }
    Layout { items, height: y }
}

fn layout_blocks(m: &dyn Measure, blocks: &[Block], items: &mut Vec<Item>, mut y: i32, width: i32, d: u32) -> i32 {
    let mut prev_item = false;
    for (n, b) in blocks.iter().enumerate() {
        let is_item = matches!(b, Block::Item { .. });
        if n > 0 {
            y += if is_item && prev_item { px(d, 4) } else { px(d, 10) };
        }
        if matches!(b, Block::Heading(_)) && n > 0 {
            y += px(d, 4);
        }
        prev_item = is_item;
        match b {
            Block::Para(spans) => {
                let (runs, bottom) = wrap(m, spans, Font::Normal, &Ink::Text, 0, y, width, d);
                items.extend(runs.into_iter().map(Item::Run));
                y = bottom;
            }
            Block::Heading(spans) => {
                let (runs, bottom) = wrap(m, spans, Font::Heading, &Ink::Text, 0, y, width, d);
                items.extend(runs.into_iter().map(Item::Run));
                y = bottom;
            }
            Block::Quote(spans) => {
                let bar = px(d, 3);
                let (runs, bottom) = wrap(m, spans, Font::Normal, &Ink::Sub, px(d, 14), y, width - px(d, 14), d);
                items.push(Item::Rule(rect(px(d, 2), y, px(d, 2) + bar, bottom)));
                items.extend(runs.into_iter().map(Item::Run));
                y = bottom;
            }
            Block::Item { level, marker, spans } => {
                let indent = px(d, 6) + *level as i32 * px(d, 18);
                let mw = m.width(Font::Normal, marker).max(px(d, 10));
                let x = indent + mw + px(d, 8);
                let (runs, bottom) = wrap(m, spans, Font::Normal, &Ink::Text, x, y, width - x, d);
                // Marker auf der Grundlinie der ersten Zeile.
                let top = runs.first().map_or(y, |r| r.rect.top + m.ascent(r.font) - m.ascent(Font::Normal));
                items.push(Item::Marker { rect: rect(indent, top, indent + mw, top + m.height(Font::Normal)), text: marker.clone() });
                items.extend(runs.into_iter().map(Item::Run));
                y = bottom.max(top + m.height(Font::Normal));
            }
            Block::Code { text, .. } => {
                let (ix, iy) = (px(d, 12), px(d, 10));
                let copy = px(d, 28);
                let inner = width - 2 * ix - copy;
                let lines = wrap_code(m, text, inner);
                let lh = m.height(Font::Mono);
                let h = (lines.len() as i32 * lh).max(lh) + 2 * iy;
                let panel = rect(0, y, width, y + h);
                items.push(Item::CodePanel(panel));
                for (i, l) in lines.iter().enumerate() {
                    if l.is_empty() {
                        continue;
                    }
                    let top = y + iy + i as i32 * lh;
                    let r = rect(ix, top, ix + m.width(Font::Mono, l), top + lh);
                    items.push(Item::Run(Run { rect: r, text: l.clone(), font: Font::Mono, ink: Ink::Text, code: false }));
                }
                let b = px(d, 26);
                let cr = rect(width - b - px(d, 5), y + px(d, 5), width - px(d, 5), y + px(d, 5) + b);
                items.push(Item::Copy { rect: cr, text: text.clone(), label: false });
                y = panel.bottom;
            }
            Block::Rule => {
                let mid = y + px(d, 6);
                items.push(Item::Rule(rect(0, mid, width, mid + 1)));
                y = mid + px(d, 7);
            }
        }
    }
    y
}

// ----------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn p(t: &str) -> Span {
        span(t, Style::Plain)
    }

    #[test]
    fn inline_bold_and_code() {
        assert_eq!(parse_inline("**a** `b`"), vec![span("a", Style::Bold), p(" "), span("b", Style::Code)]);
    }

    #[test]
    fn blocks_from_example() {
        let b = parse("**a** `b`\n- c");
        assert_eq!(
            b,
            vec![
                Block::Para(vec![span("a", Style::Bold), p(" "), span("b", Style::Code)]),
                Block::Item { level: 0, marker: "•".into(), spans: vec![p("c")] },
            ]
        );
    }

    #[test]
    fn numbered_nested_and_continuation() {
        let b = parse("1. eins\n   weiter\n  - unter\n2) zwei");
        assert_eq!(
            b,
            vec![
                Block::Item { level: 0, marker: "1.".into(), spans: vec![p("eins weiter")] },
                Block::Item { level: 1, marker: "◦".into(), spans: vec![p("unter")] },
                Block::Item { level: 0, marker: "2.".into(), spans: vec![p("zwei")] },
            ]
        );
    }

    #[test]
    fn paragraphs_join_soft_breaks() {
        let b = parse("eins\nzwei\n\ndrei");
        assert_eq!(b, vec![Block::Para(vec![p("eins zwei")]), Block::Para(vec![p("drei")])]);
    }

    #[test]
    fn headings_rule_quote() {
        let b = parse("## Titel ##\n---\n> zitat\n#kein");
        assert_eq!(
            b,
            vec![Block::Heading(vec![p("Titel")]), Block::Rule, Block::Quote(vec![p("zitat")]), Block::Para(vec![p("#kein")])]
        );
    }

    #[test]
    fn fenced_code_closed_and_open() {
        let b = parse("vor\n```rust\nfn main() {\n    x\n}\n```\nnach");
        assert_eq!(
            b,
            vec![
                Block::Para(vec![p("vor")]),
                Block::Code { lang: "rust".into(), text: "fn main() {\n    x\n}".into() },
                Block::Para(vec![p("nach")]),
            ]
        );
        // Beim Streamen fehlt der Abschluss noch.
        assert_eq!(parse("```\nls -la"), vec![Block::Code { lang: String::new(), text: "ls -la".into() }]);
    }

    #[test]
    fn links_bare_and_markdown() {
        assert_eq!(
            parse_inline("Siehe https://example.com/a_(b). Oder [Doku](https://x.de/d)!"),
            vec![
                p("Siehe "),
                span("https://example.com/a_(b)", Style::Link("https://example.com/a_(b)".into())),
                p(". Oder "),
                span("Doku", Style::Link("https://x.de/d".into())),
                p("!"),
            ]
        );
        assert_eq!(parse_inline("(https://a.de/x)"), vec![p("("), span("https://a.de/x", Style::Link("https://a.de/x".into())), p(")")]);
        // Kein Link: anderes Schema, mitten im Wort.
        assert_eq!(parse_inline("ftp://a.de xhttps://b.de"), vec![p("ftp://a.de xhttps://b.de")]);
    }

    #[test]
    fn italic_and_unclosed_markers() {
        assert_eq!(parse_inline("ein *wort* hier"), vec![p("ein wort hier")]);
        assert_eq!(parse_inline("2 * 3 * 4"), vec![p("2 * 3 * 4")]);
        assert_eq!(parse_inline("**offen"), vec![p("**offen")]);
        assert_eq!(parse_inline("snake_case_name"), vec![p("snake_case_name")]);
        assert_eq!(parse_inline(r"\*nicht\*"), vec![p("*nicht*")]);
    }

    #[test]
    fn bold_with_code_inside() {
        assert_eq!(parse_inline("**x `y` z**"), vec![span("x ", Style::Bold), span("y", Style::Code), span(" z", Style::Bold)]);
    }

    #[test]
    fn table_rows() {
        assert_eq!(parse("| a | b |\n|---|:-:|\n| 1 | 2 |"), vec![Block::Para(vec![p("a  ·  b")]), Block::Para(vec![p("1  ·  2")])]);
    }

    /// Jedes Zeichen 10 px breit, Zeilen 20 px hoch (Mono 18 px).
    struct Fixed;
    impl Measure for Fixed {
        fn width(&self, _: Font, s: &str) -> i32 {
            s.chars().count() as i32 * 10
        }
        fn height(&self, f: Font) -> i32 {
            if f == Font::Mono { 18 } else { 20 }
        }
        fn ascent(&self, f: Font) -> i32 {
            if f == Font::Mono { 14 } else { 16 }
        }
    }

    fn texts(runs: &[Run]) -> Vec<(&str, i32, i32)> {
        runs.iter().map(|r| (r.text.as_str(), r.rect.left, r.rect.top)).collect()
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        let (runs, bottom) = wrap(&Fixed, &[p("aaa bbb ccc")], Font::Normal, &Ink::Text, 0, 0, 75, 96);
        assert_eq!(texts(&runs), vec![("aaa bbb", 0, 0), ("ccc", 0, 20)]);
        assert_eq!(bottom, 40);
        // Kein Leerzeichen am Zeilenende in der Breite.
        assert_eq!(runs[0].rect.right, 70);
    }

    #[test]
    fn wrap_splits_long_words() {
        let (runs, _) = wrap(&Fixed, &[p("ab cdefghij")], Font::Normal, &Ink::Text, 5, 0, 40, 96);
        assert_eq!(texts(&runs), vec![("ab", 5, 0), ("cdef", 5, 20), ("ghij", 5, 40)]);
    }

    #[test]
    fn wrap_keeps_styles_apart_and_pads_code() {
        let spans = parse_inline("**a** `b` c");
        let (runs, _) = wrap(&Fixed, &spans, Font::Normal, &Ink::Text, 0, 0, 500, 96);
        assert_eq!(runs.len(), 4);
        assert_eq!((runs[0].font, runs[0].text.as_str()), (Font::Bold, "a"));
        assert_eq!((runs[2].font, runs[2].code, runs[2].rect.left), (Font::Mono, true, 24));
        // Mono hat eine kleinere Oberlänge – gleiche Grundlinie.
        assert_eq!(runs[2].rect.top, 2);
        assert_eq!((runs[3].text.as_str(), runs[3].rect.left), (" c", 24 + 10 + 4));
    }

    #[test]
    fn wrap_links_get_link_ink() {
        let (runs, _) = wrap(&Fixed, &parse_inline("x https://a.de"), Font::Normal, &Ink::Text, 0, 0, 500, 96);
        assert_eq!(runs.last().unwrap().ink, Ink::Link("https://a.de".into()));
    }

    #[test]
    fn code_wrap_by_chars() {
        assert_eq!(wrap_code(&Fixed, "abcdef\n\n\tx", 40), vec!["abcd", "ef", "", "    ", "x"]);
    }

    #[test]
    fn layout_turns() {
        let turn = Turn { question: "hallo", steps: &[], text: "Hi\n\n```\nls\n```", thinking: false, done: true, error: None };
        let l = layout(&Fixed, &[turn], 400, 96);
        // Blase rechtsbündig, 5 Zeichen + 2 × 12 Innenabstand.
        assert!(matches!(l.items[0], Item::Bubble(r) if r.right == 400 && r.left == 400 - 50 - 24));
        let copies: Vec<(&str, bool)> = l.items.iter().filter_map(|i| if let Item::Copy { text, label, .. } = i { Some((text.as_str(), *label)) } else { None }).collect();
        assert_eq!(copies, vec![("ls", false), ("Hi\n\n```\nls\n```", true)]);
        assert!(l.items.iter().any(|i| matches!(i, Item::CodePanel(_))));
        let last = l.items.iter().filter_map(|i| if let Item::Copy { rect, .. } = i { Some(rect.bottom) } else { None }).max().unwrap();
        assert_eq!(l.height, last);
    }

    #[test]
    fn layout_streaming_and_steps() {
        let steps = vec!["Suche im Web".to_string()];
        let t = Turn { question: "q", steps: &steps, text: "", thinking: true, done: false, error: None };
        let l = layout(&Fixed, &[t], 300, 96);
        let runs: Vec<&str> = l.items.iter().filter_map(|i| if let Item::Run(r) = i { Some(r.text.as_str()) } else { None }).collect();
        assert_eq!(runs, vec!["q", "Suche im Web", "denkt nach …"]);
        let t = Turn { question: "q", steps: &[], text: "teil", thinking: false, done: false, error: None };
        let l = layout(&Fixed, &[t], 300, 96);
        assert!(l.items.iter().any(|i| matches!(i, Item::Run(r) if r.text == "teil ▍")));
        assert!(!l.items.iter().any(|i| matches!(i, Item::Copy { .. })));
    }
}
