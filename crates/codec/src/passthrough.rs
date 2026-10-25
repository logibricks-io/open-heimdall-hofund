//! Same-protocol passthrough helpers.
//!
//! The gateway forwards raw bytes. It asks the codec to read token usage and,
//! when the client-requested model differs from the upstream model, to splice
//! that protocol's model field. Equal names are copied through with no JSON
//! parse. A differing name replaces only the bytes of the model string, so
//! every other byte, including large integers and escape sequences, stays as
//! the upstream sent it.
//!
//! An event is scanned once. More than 1 MiB, a leading BOM, or a value the
//! scanner cannot read, is forwarded after a literal replacement of the
//! quoted upstream model name.

use hofund_ir::{StreamEvent, Usage};

use crate::StreamDecoder;

/// Bytes of one event the tap will hold before falling back to a literal
/// replacement. A larger event is forwarded as it arrives.
pub(crate) const DEFAULT_EVENT_CAP: usize = 1 << 20;

/// Incremental read of a raw response body.
///
/// `push` returns the bytes the client should see. A chunk may end inside a
/// frame or a model string; the unfinished tail is held until a later `push`
/// or `finish`, up to 1 MiB.
pub trait RawTap: Send {
    /// Observes `chunk` and returns the bytes to forward.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8>;

    /// Flushes a trailing partial frame and any unfinished model field.
    fn finish(&mut self) -> Vec<u8>;

    /// Input and output tokens seen so far.
    fn tokens(&self) -> (Option<i64>, Option<i64>);
}

pub(crate) struct DecoderTap {
    decoder: Box<dyn StreamDecoder>,
    rewrite: bool,
    events: EventBuf<'static>,
    input: Option<i64>,
    output: Option<i64>,
}

impl DecoderTap {
    pub(crate) fn new(
        decoder: Box<dyn StreamDecoder>,
        paths: &'static [&'static [&'static str]],
        client: &str,
        upstream: &str,
    ) -> Self {
        Self::with_cap(decoder, paths, client, upstream, DEFAULT_EVENT_CAP)
    }

    pub(crate) fn with_cap(
        decoder: Box<dyn StreamDecoder>,
        paths: &'static [&'static [&'static str]],
        client: &str,
        upstream: &str,
        cap: usize,
    ) -> Self {
        let rewrite = client != upstream && !paths.is_empty();
        Self {
            decoder,
            rewrite,
            events: EventBuf::new(paths, client, upstream, cap),
            input: None,
            output: None,
        }
    }

    fn note(&mut self, events: &[StreamEvent]) {
        for event in events {
            let usage = match event {
                StreamEvent::MessageStart(start) => start.usage.as_ref(),
                StreamEvent::MessageDelta(delta) => delta.usage.as_ref(),
                _ => None,
            };
            note_usage(usage, &mut self.input, &mut self.output);
        }
    }
}

impl RawTap for DecoderTap {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if let Ok(events) = self.decoder.push(chunk) {
            self.note(&events);
        }
        if !self.rewrite {
            return chunk.to_vec();
        }
        self.events.push(chunk)
    }

    fn finish(&mut self) -> Vec<u8> {
        if let Ok(events) = self.decoder.finish() {
            self.note(&events);
        }
        if !self.rewrite {
            return Vec::new();
        }
        self.events.finish()
    }

    fn tokens(&self) -> (Option<i64>, Option<i64>) {
        (self.input, self.output)
    }
}

pub(crate) fn rewrite_all(
    bytes: &[u8],
    paths: &[&[&str]],
    client: &str,
    upstream: &str,
) -> Vec<u8> {
    if paths.is_empty() || client == upstream || bytes.is_empty() {
        return bytes.to_vec();
    }
    let mut events = EventBuf::new(paths, client, upstream, usize::MAX);
    let mut out = events.push(bytes);
    out.extend(events.finish());
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Undecided,
    Sse,
    Json,
    BomSse,
    BomJson,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The byte belongs to the current event.
    Keep,
    /// The byte closed the event.
    End,
    /// The previous byte closed the event. This byte starts the next one.
    Replay,
}

struct EventBuf<'p> {
    paths: &'p [&'p [&'p str]],
    quoted_from: Vec<u8>,
    quoted_to: Vec<u8>,
    cap: usize,
    kind: Kind,
    overflowing: bool,
    pending: Vec<u8>,
    cursor: usize,
    at_line_start: bool,
    saw_content_line: bool,
    cr_held: bool,
    overlap: Vec<u8>,
}

impl<'p> EventBuf<'p> {
    fn new(paths: &'p [&'p [&'p str]], client: &str, upstream: &str, cap: usize) -> Self {
        Self {
            paths,
            quoted_from: json_quote(upstream),
            quoted_to: json_quote(client),
            cap,
            kind: Kind::Undecided,
            overflowing: false,
            pending: Vec::new(),
            cursor: 0,
            at_line_start: true,
            saw_content_line: false,
            cr_held: false,
            overlap: Vec::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if chunk.is_empty() {
            return Vec::new();
        }
        if self.overflowing {
            return self.push_overflow(chunk);
        }
        self.pending.extend_from_slice(chunk);
        self.classify_if_needed();
        match self.kind {
            Kind::Sse | Kind::BomSse => self.drain_events(),
            Kind::Json | Kind::BomJson | Kind::Undecided if self.pending.len() > self.cap => {
                self.overflow_pending()
            }
            Kind::Json | Kind::BomJson | Kind::Undecided => Vec::new(),
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.overflowing {
            self.cr_held = false;
            let mut tail = self.finish_literal_window();
            if !matches!(self.kind, Kind::Json | Kind::BomJson) {
                terminate_event(&mut tail);
            }
            self.overflowing = false;
            self.reset_line();
            return tail;
        }
        self.classify_if_needed();
        let mut out = Vec::new();
        if matches!(self.kind, Kind::Sse | Kind::BomSse) && self.finish_held_cr() {
            out.extend(self.take_ready_event());
        }
        if matches!(self.kind, Kind::Sse | Kind::BomSse) {
            out.extend(self.drain_events());
        }
        if self.pending.is_empty() {
            return out;
        }
        let rest = std::mem::take(&mut self.pending);
        self.reset_line();
        match self.kind {
            Kind::Json => out.extend(splice_document(
                &rest,
                self.paths,
                &self.quoted_to,
                &self.quoted_from,
            )),
            Kind::BomJson => out.extend(literal_replace(
                &rest,
                &self.quoted_from,
                &self.quoted_to,
                true,
            )),
            Kind::Sse | Kind::BomSse | Kind::Undecided => {
                let mut replaced = literal_replace(&rest, &self.quoted_from, &self.quoted_to, true);
                terminate_event(&mut replaced);
                out.extend(replaced);
            }
        }
        out
    }

    fn classify_if_needed(&mut self) {
        if self.kind != Kind::Undecided {
            return;
        }
        if let Some(kind) = classify(&self.pending) {
            self.kind = kind;
        }
    }

    fn drain_events(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let Some(end) = self.poll_event() else {
                if self.pending.len() > self.cap {
                    out.extend(self.overflow_pending());
                }
                return out;
            };
            out.extend(self.take_event(end));
        }
    }

    fn take_ready_event(&mut self) -> Vec<u8> {
        let end = self.cursor;
        self.take_event(end)
    }

    fn take_event(&mut self, end: usize) -> Vec<u8> {
        let event: Vec<u8> = self.pending.drain(..end).collect();
        self.reset_line();
        self.rewrite_event(&event)
    }

    fn rewrite_event(&self, event: &[u8]) -> Vec<u8> {
        if self.kind == Kind::BomSse {
            return literal_replace(event, &self.quoted_from, &self.quoted_to, true);
        }
        splice_sse_event(event, self.paths, &self.quoted_to)
            .unwrap_or_else(|| literal_replace(event, &self.quoted_from, &self.quoted_to, true))
    }

    fn overflow_pending(&mut self) -> Vec<u8> {
        self.overflowing = true;
        let bytes = std::mem::take(&mut self.pending);
        self.cursor = 0;
        self.stream_literal(&bytes)
    }

    fn push_overflow(&mut self, chunk: &[u8]) -> Vec<u8> {
        if !matches!(self.kind, Kind::Sse | Kind::BomSse) {
            return self.stream_literal(chunk);
        }
        let mut out = Vec::new();
        let start = 0;
        let mut index = 0;
        while index < chunk.len() {
            match self.feed_one(chunk[index]) {
                Step::Keep => index += 1,
                Step::End => {
                    let end = index + 1;
                    out.extend(self.stream_literal(&chunk[start..end]));
                    out.extend(self.finish_literal_window());
                    self.overflowing = false;
                    self.reset_line();
                    self.pending.extend_from_slice(&chunk[end..]);
                    out.extend(self.drain_events());
                    return out;
                }
                Step::Replay => {
                    out.extend(self.stream_literal(&chunk[start..index]));
                    out.extend(self.finish_literal_window());
                    self.overflowing = false;
                    self.reset_line();
                    self.pending.extend_from_slice(&chunk[index..]);
                    out.extend(self.drain_events());
                    return out;
                }
            }
        }
        out.extend(self.stream_literal(&chunk[start..]));
        out
    }

    fn poll_event(&mut self) -> Option<usize> {
        while self.cursor < self.pending.len() {
            let byte = self.pending[self.cursor];
            match self.feed_one(byte) {
                Step::Keep => self.cursor += 1,
                Step::End => {
                    self.cursor += 1;
                    return Some(self.cursor);
                }
                Step::Replay => return Some(self.cursor),
            }
        }
        None
    }

    fn finish_held_cr(&mut self) -> bool {
        if !self.cr_held {
            return false;
        }
        self.cr_held = false;
        self.close_line()
    }

    fn feed_one(&mut self, byte: u8) -> Step {
        if self.cr_held {
            self.cr_held = false;
            if byte != b'\n' {
                if self.close_line() {
                    return Step::Replay;
                }
                return self.feed_one(byte);
            }
            if self.close_line() {
                return Step::End;
            }
            return Step::Keep;
        }
        if byte == b'\r' {
            self.cr_held = true;
            return Step::Keep;
        }
        if byte == b'\n' {
            if self.close_line() {
                return Step::End;
            }
            return Step::Keep;
        }
        self.at_line_start = false;
        Step::Keep
    }

    fn close_line(&mut self) -> bool {
        let empty = self.at_line_start;
        self.at_line_start = true;
        if empty && self.saw_content_line {
            self.saw_content_line = false;
            true
        } else {
            self.saw_content_line = true;
            false
        }
    }

    fn reset_line(&mut self) {
        self.cursor = 0;
        self.at_line_start = true;
        self.saw_content_line = false;
        self.cr_held = false;
    }

    /// Forwards `bytes`, keeping a suffix long enough that a quoted model name
    /// split on this boundary is still replaced with the next chunk.
    fn stream_literal(&mut self, bytes: &[u8]) -> Vec<u8> {
        if bytes.is_empty() {
            return Vec::new();
        }
        let mut window = std::mem::take(&mut self.overlap);
        window.extend_from_slice(bytes);
        let needle = self.quoted_from.len();
        if needle < 3 {
            self.overlap.clear();
            return window;
        }
        let safe = window.len().saturating_sub(needle - 1);
        let mut out = Vec::with_capacity(window.len());
        let mut index = 0;
        while index < safe {
            if window[index..].starts_with(&self.quoted_from) {
                out.extend_from_slice(&self.quoted_to);
                index += needle;
            } else {
                out.push(window[index]);
                index += 1;
            }
        }
        self.overlap = window[index..].to_vec();
        out
    }

    fn finish_literal_window(&mut self) -> Vec<u8> {
        let window = std::mem::take(&mut self.overlap);
        literal_replace(&window, &self.quoted_from, &self.quoted_to, true)
    }
}

fn note_usage(usage: Option<&Usage>, input: &mut Option<i64>, output: &mut Option<i64>) {
    let Some(usage) = usage else {
        return;
    };
    if let Some(value) = as_i64(usage.input_tokens) {
        *input = Some(value);
    }
    if let Some(value) = as_i64(usage.output_tokens) {
        *output = Some(value);
    }
}

fn as_i64(value: Option<u64>) -> Option<i64> {
    value.and_then(|value| i64::try_from(value).ok())
}

enum Prefix {
    Yes,
    No,
    More,
}

enum Bom {
    NeedMore,
    Absent(usize),
    Present(usize),
}

fn classify(bytes: &[u8]) -> Option<Kind> {
    let (bom, index) = match bom_index(bytes) {
        Bom::NeedMore => return None,
        Bom::Absent(index) => (false, index),
        Bom::Present(index) => (true, index),
    };
    if index >= bytes.len() {
        return None;
    }
    match sse_prefix(&bytes[index..]) {
        Prefix::More => None,
        Prefix::Yes => Some(if bom { Kind::BomSse } else { Kind::Sse }),
        Prefix::No => Some(if bom { Kind::BomJson } else { Kind::Json }),
    }
}

fn bom_index(bytes: &[u8]) -> Bom {
    let mut index = 0;
    while index < bytes.len() && is_ws(bytes[index]) {
        index += 1;
    }
    if index >= bytes.len() {
        return Bom::NeedMore;
    }
    if bytes[index] != 0xEF {
        return Bom::Absent(index);
    }
    if bytes.len() - index < 3 {
        if [0xEF, 0xBB, 0xBF].starts_with(&bytes[index..]) {
            return Bom::NeedMore;
        }
        return Bom::Absent(index);
    }
    if bytes[index..index + 3] != [0xEF, 0xBB, 0xBF] {
        return Bom::Absent(index);
    }
    index += 3;
    while index < bytes.len() && is_ws(bytes[index]) {
        index += 1;
    }
    if index >= bytes.len() {
        Bom::NeedMore
    } else {
        Bom::Present(index)
    }
}

fn sse_prefix(rest: &[u8]) -> Prefix {
    const KEYS: &[&[u8]] = &[b"data:", b"event:", b"id:", b"retry:", b":"];
    if rest.is_empty() {
        return Prefix::More;
    }
    if KEYS.iter().any(|key| rest.starts_with(key)) {
        return Prefix::Yes;
    }
    if KEYS.iter().any(|key| key.starts_with(rest)) {
        return Prefix::More;
    }
    Prefix::No
}

fn splice_document(bytes: &[u8], paths: &[&[&str]], replacement: &[u8], needle: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset..].iter().all(|byte| is_ws(*byte)) {
            out.extend_from_slice(&bytes[offset..]);
            break;
        }
        match scan_json(&bytes[offset..], paths) {
            Ok((end, spans)) if spans.is_empty() => {
                out.extend_from_slice(&bytes[offset..offset + end]);
                offset += end;
            }
            Ok((end, spans)) => match apply_spans(&bytes[offset..offset + end], spans, replacement)
            {
                Some(piece) => {
                    out.extend(piece);
                    offset += end;
                }
                None => {
                    out.extend(literal_replace(&bytes[offset..], needle, replacement, true));
                    return out;
                }
            },
            Err(()) => {
                out.extend(literal_replace(&bytes[offset..], needle, replacement, true));
                return out;
            }
        }
    }
    out
}

fn splice_sse_event(event: &[u8], paths: &[&[&str]], replacement: &[u8]) -> Option<Vec<u8>> {
    let mut joined = Vec::new();
    let mut origin = Vec::new();
    let mut saw_data = false;
    for (start, end) in content_lines(event) {
        let Some(payload) = data_payload_index(&event[start..end]) else {
            continue;
        };
        if saw_data {
            joined.push(b'\n');
            origin.push(usize::MAX);
        }
        saw_data = true;
        let absolute = start + payload;
        for (offset, byte) in event[absolute..end].iter().copied().enumerate() {
            joined.push(byte);
            origin.push(absolute + offset);
        }
    }
    if !saw_data || joined.iter().all(|byte| is_ws(*byte)) {
        return Some(event.to_vec());
    }
    let (end, spans) = scan_json(&joined, paths).ok()?;
    if joined[end..].iter().any(|byte| !is_ws(*byte)) {
        return None;
    }
    if spans.is_empty() {
        return Some(event.to_vec());
    }
    let mut mapped = Vec::with_capacity(spans.len());
    for (start, stop) in spans {
        mapped.push(map_span(&origin, start, stop)?);
    }
    apply_spans(event, mapped, replacement)
}

fn content_lines(event: &[u8]) -> Vec<(usize, usize)> {
    let mut lines = Vec::new();
    let mut index = 0;
    let mut line_start = 0;
    while index < event.len() {
        if event[index] == b'\r' {
            let content_end = index;
            index += 1;
            if index < event.len() && event[index] == b'\n' {
                index += 1;
            }
            lines.push((line_start, content_end));
            line_start = index;
            continue;
        }
        if event[index] == b'\n' {
            lines.push((line_start, index));
            index += 1;
            line_start = index;
            continue;
        }
        index += 1;
    }
    if line_start < event.len() {
        lines.push((line_start, event.len()));
    }
    lines
}

fn data_payload_index(content: &[u8]) -> Option<usize> {
    let rest = content.strip_prefix(b"data:")?;
    let skip = usize::from(rest.first() == Some(&b' '));
    Some(content.len() - rest.len() + skip)
}

fn map_span(origin: &[usize], start: usize, end: usize) -> Option<(usize, usize)> {
    if start >= end || end > origin.len() {
        return None;
    }
    let first = *origin.get(start)?;
    if first == usize::MAX {
        return None;
    }
    let mut previous = first;
    for at in origin.iter().take(end).skip(start + 1).copied() {
        if at != previous + 1 {
            return None;
        }
        previous = at;
    }
    Some((first, previous + 1))
}

fn apply_spans(
    bytes: &[u8],
    mut spans: Vec<(usize, usize)>,
    replacement: &[u8],
) -> Option<Vec<u8>> {
    spans.sort_unstable_by_key(|span| span.0);
    let mut out = Vec::with_capacity(bytes.len() + replacement.len());
    let mut cursor = 0;
    for (start, end) in spans {
        if start < cursor || end < start || end > bytes.len() {
            return None;
        }
        out.extend_from_slice(&bytes[cursor..start]);
        out.extend_from_slice(replacement);
        cursor = end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    Some(out)
}

fn scan_json(bytes: &[u8], paths: &[&[&str]]) -> Result<(usize, Vec<(usize, usize)>), ()> {
    let mut scanner = Scanner { bytes, index: 0 };
    scanner.skip_ws();
    if scanner.index >= bytes.len() {
        return Err(());
    }
    let mut stack = Vec::new();
    let mut spans = Vec::new();
    scanner.value(paths, &mut stack, 0, &mut spans)?;
    Ok((scanner.index, spans))
}

struct Scanner<'a> {
    bytes: &'a [u8],
    index: usize,
}

enum StackKey {
    Raw(usize, usize),
    Owned(String),
}

impl<'a> Scanner<'a> {
    fn skip_ws(&mut self) {
        while self.index < self.bytes.len() && is_ws(self.bytes[self.index]) {
            self.index += 1;
        }
    }

    fn value(
        &mut self,
        paths: &[&[&str]],
        stack: &mut Vec<StackKey>,
        depth: usize,
        spans: &mut Vec<(usize, usize)>,
    ) -> Result<(), ()> {
        if depth > 64 {
            return Err(());
        }
        self.skip_ws();
        let Some(byte) = self.bytes.get(self.index).copied() else {
            return Err(());
        };
        match byte {
            b'{' => self.object(paths, stack, depth, spans),
            b'[' => self.array(paths, stack, depth, spans),
            b'"' => {
                let (start, end, _) = self.string_span()?;
                if path_matches(self.bytes, stack, paths) {
                    spans.push((start, end));
                }
                Ok(())
            }
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(()),
        }
    }

    fn object(
        &mut self,
        paths: &[&[&str]],
        stack: &mut Vec<StackKey>,
        depth: usize,
        spans: &mut Vec<(usize, usize)>,
    ) -> Result<(), ()> {
        self.index += 1;
        loop {
            self.skip_ws();
            if self.bytes.get(self.index) == Some(&b'}') {
                self.index += 1;
                return Ok(());
            }
            let (start, end, escaped) = self.string_span()?;
            let key = if escaped {
                StackKey::Owned(decode_string(&self.bytes[start + 1..end - 1])?)
            } else {
                StackKey::Raw(start + 1, end - 1)
            };
            self.skip_ws();
            if self.bytes.get(self.index) != Some(&b':') {
                return Err(());
            }
            self.index += 1;
            stack.push(key);
            self.value(paths, stack, depth + 1, spans)?;
            stack.pop();
            self.skip_ws();
            match self.bytes.get(self.index).copied() {
                Some(b',') => self.index += 1,
                Some(b'}') => {
                    self.index += 1;
                    return Ok(());
                }
                _ => return Err(()),
            }
        }
    }

    fn array(
        &mut self,
        paths: &[&[&str]],
        stack: &mut Vec<StackKey>,
        depth: usize,
        spans: &mut Vec<(usize, usize)>,
    ) -> Result<(), ()> {
        self.index += 1;
        loop {
            self.skip_ws();
            if self.bytes.get(self.index) == Some(&b']') {
                self.index += 1;
                return Ok(());
            }
            self.value(paths, stack, depth + 1, spans)?;
            self.skip_ws();
            match self.bytes.get(self.index).copied() {
                Some(b',') => self.index += 1,
                Some(b']') => {
                    self.index += 1;
                    return Ok(());
                }
                _ => return Err(()),
            }
        }
    }

    fn string_span(&mut self) -> Result<(usize, usize, bool), ()> {
        let start = self.index;
        if self.bytes.get(self.index) != Some(&b'"') {
            return Err(());
        }
        self.index += 1;
        let mut escaped = false;
        while self.index < self.bytes.len() {
            let byte = self.bytes[self.index];
            if byte == b'\\' {
                escaped = true;
                self.index += 1;
                let Some(mark) = self.bytes.get(self.index).copied() else {
                    return Err(());
                };
                if mark == b'u' {
                    if self.index + 4 >= self.bytes.len() {
                        return Err(());
                    }
                    if !self.bytes[self.index + 1..self.index + 5]
                        .iter()
                        .all(u8::is_ascii_hexdigit)
                    {
                        return Err(());
                    }
                    self.index += 5;
                    continue;
                }
                if !matches!(mark, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') {
                    return Err(());
                }
                self.index += 1;
                continue;
            }
            if byte == b'"' {
                self.index += 1;
                return Ok((start, self.index, escaped));
            }
            if byte < 0x20 {
                return Err(());
            }
            self.index += 1;
        }
        Err(())
    }

    fn literal(&mut self, text: &[u8]) -> Result<(), ()> {
        if self.bytes[self.index..].starts_with(text) {
            self.index += text.len();
            Ok(())
        } else {
            Err(())
        }
    }

    fn number(&mut self) -> Result<(), ()> {
        if self.bytes.get(self.index) == Some(&b'-') {
            self.index += 1;
        }
        if self.bytes.get(self.index) == Some(&b'0') {
            self.index += 1;
        } else if self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
            while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
        } else {
            return Err(());
        }
        if self.bytes.get(self.index) == Some(&b'.') {
            self.index += 1;
            if !self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                return Err(());
            }
            while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
        }
        if matches!(self.bytes.get(self.index), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.bytes.get(self.index), Some(b'+' | b'-')) {
                self.index += 1;
            }
            if !self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                return Err(());
            }
            while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
        }
        Ok(())
    }
}

fn path_matches(bytes: &[u8], stack: &[StackKey], paths: &[&[&str]]) -> bool {
    paths.iter().any(|path| {
        path.len() == stack.len()
            && path
                .iter()
                .zip(stack.iter())
                .all(|(segment, key)| match key {
                    StackKey::Raw(start, end) => &bytes[*start..*end] == segment.as_bytes(),
                    StackKey::Owned(text) => text == segment,
                })
    })
}

fn decode_string(content: &[u8]) -> Result<String, ()> {
    let mut out = String::new();
    let mut index = 0;
    while index < content.len() {
        if content[index] != b'\\' {
            let start = index;
            while index < content.len() && content[index] != b'\\' {
                index += 1;
            }
            let text = std::str::from_utf8(&content[start..index]).map_err(|_| ())?;
            out.push_str(text);
            continue;
        }
        index += 1;
        let Some(mark) = content.get(index).copied() else {
            return Err(());
        };
        match mark {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => push_unicode(content, &mut index, &mut out)?,
            _ => return Err(()),
        }
        if mark != b'u' {
            index += 1;
        }
    }
    Ok(out)
}

fn push_unicode(content: &[u8], index: &mut usize, out: &mut String) -> Result<(), ()> {
    *index += 1;
    let unit = hex4(&content[*index..])?;
    *index += 4;
    if (0xD800..=0xDBFF).contains(&unit) {
        if content.get(*index) != Some(&b'\\') || content.get(*index + 1) != Some(&b'u') {
            return Err(());
        }
        *index += 2;
        let low = hex4(&content[*index..])?;
        *index += 4;
        if !(0xDC00..=0xDFFF).contains(&low) {
            return Err(());
        }
        let code = 0x1_0000 + (((u32::from(unit) - 0xD800) << 10) | (u32::from(low) - 0xDC00));
        out.push(char::from_u32(code).ok_or(())?);
        return Ok(());
    }
    if (0xDC00..=0xDFFF).contains(&unit) {
        return Err(());
    }
    out.push(char::from_u32(u32::from(unit)).ok_or(())?);
    Ok(())
}

fn hex4(bytes: &[u8]) -> Result<u16, ()> {
    if bytes.len() < 4 {
        return Err(());
    }
    let mut value = 0u16;
    for byte in &bytes[..4] {
        value = (value << 4) | hex_value(*byte)?;
    }
    Ok(value)
}

fn hex_value(byte: u8) -> Result<u16, ()> {
    match byte {
        b'0'..=b'9' => Ok(u16::from(byte - b'0')),
        b'a'..=b'f' => Ok(u16::from(byte - b'a' + 10)),
        b'A'..=b'F' => Ok(u16::from(byte - b'A' + 10)),
        _ => Err(()),
    }
}

fn literal_replace(bytes: &[u8], from: &[u8], to: &[u8], partial: bool) -> Vec<u8> {
    let mut out = if from.len() < 3 {
        bytes.to_vec()
    } else {
        replace_exact(bytes, from, to)
    };
    if !partial || from.len() < 2 {
        return out;
    }
    let max = (from.len() - 1).min(out.len());
    let mut len = max;
    while len >= 2 {
        if out.ends_with(&from[..len]) {
            out.truncate(out.len() - len);
            out.extend_from_slice(to);
            break;
        }
        len -= 1;
    }
    out
}

fn replace_exact(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut index = 0;
    while index < haystack.len() {
        if haystack[index..].starts_with(needle) {
            out.extend_from_slice(replacement);
            index += needle.len();
        } else {
            out.push(haystack[index]);
            index += 1;
        }
    }
    out
}

fn json_quote(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 2);
    out.push(b'"');
    for ch in text.chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{0008}' => out.extend_from_slice(b"\\b"),
            '\u{000c}' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            other if (other as u32) < 0x20 => {
                let code = other as u32;
                out.extend_from_slice(b"\\u00");
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push(HEX[((code >> 4) & 0xf) as usize]);
                out.push(HEX[(code & 0xf) as usize]);
            }
            other => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
    out
}

fn is_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// Ends a partial event so the next frame is not glued to it.
fn terminate_event(bytes: &mut Vec<u8>) {
    if bytes.is_empty() || frame_closed(bytes) {
        return;
    }
    let crlf = bytes.windows(2).any(|pair| pair == b"\r\n");
    if crlf {
        if bytes.ends_with(b"\r\n") {
            bytes.extend_from_slice(b"\r\n");
        } else if bytes.ends_with(b"\r") {
            bytes.extend_from_slice(b"\n\r\n");
        } else {
            bytes.extend_from_slice(b"\r\n\r\n");
        }
        return;
    }
    if bytes.contains(&b'\r') {
        if bytes.ends_with(b"\r") {
            bytes.push(b'\r');
        } else {
            bytes.extend_from_slice(b"\r\r");
        }
        return;
    }
    if bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    } else {
        bytes.extend_from_slice(b"\n\n");
    }
}

pub(crate) fn frame_closed(bytes: &[u8]) -> bool {
    bytes.ends_with(b"\n\n") || bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\r\r")
}

#[cfg(test)]
mod tests {
    use super::{DecoderTap, RawTap, rewrite_all};
    use crate::{
        AnthropicMessages, Codec, Converse, GenerativeLanguage, OpenAiChat, OpenAiResponses,
    };

    #[test]
    fn a_split_json_object_is_rewritten_when_it_completes() {
        let mut tap = OpenAiChat.raw_tap("demo", "upstream");
        assert!(tap.push(br#"{"model":"up"#).is_empty());
        assert!(tap.push(br#"stream","n":1}"#).is_empty());
        let text = String::from_utf8(tap.finish()).expect("utf-8");
        assert_eq!(text, r#"{"model":"demo","n":1}"#);
    }

    #[test]
    fn numbers_and_escapes_round_trip_byte_for_byte() {
        let raw = br#"{"model":"upstream","big":123456789012345678901234567890,"f":10.10,"e":1e2,"s":"\u00e9\/"}"#;
        let want = br#"{"model":"demo","big":123456789012345678901234567890,"f":10.10,"e":1e2,"s":"\u00e9\/"}"#;
        let paths: &[&[&str]] = &[&["model"]];
        assert_eq!(rewrite_all(raw, paths, "demo", "upstream"), want);
        assert_eq!(rewrite_all(raw, paths, "upstream", "upstream"), raw);
        let untouched = br#"{"id":"c","created":1.50,"big":123456789012345678901234567890,"e":1e2,"s":"\u00e9\/"}"#;
        assert_eq!(rewrite_all(untouched, paths, "demo", "upstream"), untouched);
        let tool = br#"{"model":"upstream","content":[{"type":"tool_use","input":{"account":123456789012345678901234567890,"amount":10.10,"exp":1e2,"model":"tesla-3"}}]}"#;
        let tool_out = rewrite_all(tool, paths, "demo", "upstream");
        assert_eq!(&tool_out[..tool_out.len()], &{
            let mut expected = tool.to_vec();
            let from = br#""model":"upstream""#;
            let to = br#""model":"demo""#;
            let at = expected
                .windows(from.len())
                .position(|window| window == from)
                .unwrap();
            expected.splice(at..at + from.len(), to.iter().copied());
            expected
        });
        assert!(
            tool_out
                .windows(14)
                .any(|window| window == br#""model":"demo""#)
        );
        assert!(
            tool_out
                .windows(17)
                .any(|window| window == br#""model":"tesla-3""#)
        );
        assert!(tool_out.windows(5).any(|window| window == b"10.10"));
        assert!(tool_out.windows(3).any(|window| window == b"1e2"));
    }

    #[test]
    fn equal_names_are_not_parsed() {
        let frame = b"data: {\"model\":\"demo\",\"big\":123456789012345678901234567890,\"f\":10.10,\"e\":1e2,\"s\":\"\\u00e9\\/\"}\n\n";
        let mut tap = OpenAiChat.raw_tap("demo", "demo");
        let mid = frame.len() / 2;
        let first = tap.push(&frame[..mid]);
        assert_eq!(first, &frame[..mid]);
        let mut out = first;
        out.extend(tap.push(&frame[mid..]));
        out.extend(tap.finish());
        assert_eq!(out, frame);
        assert_eq!(tap.tokens(), (None, None));
    }

    #[test]
    fn a_multiline_data_field_and_a_cr_event_are_spliced() {
        let paths: &[&[&str]] = &[&["model"]];
        let split = b"data: {\"model\":\ndata: \"upstream\",\"n\":1}\n\n";
        assert_eq!(
            rewrite_all(split, paths, "demo", "upstream"),
            b"data: {\"model\":\ndata: \"demo\",\"n\":1}\n\n"
        );
        let cr = b"data: {\"model\":\"upstream\",\"n\":1}\r\r";
        assert_eq!(
            rewrite_all(cr, paths, "demo", "upstream"),
            b"data: {\"model\":\"demo\",\"n\":1}\r\r"
        );
        let done = b"data: [DONE]\n\n";
        assert_eq!(rewrite_all(done, paths, "demo", "upstream"), done);
    }

    #[test]
    fn unreadable_events_do_not_leak_the_upstream_name() {
        let paths: &[&[&str]] = &[&["model"]];
        let mut odd = b"data: {\"model\":\"upstream\",\"x\":\"".to_vec();
        odd.push(0xff);
        odd.extend_from_slice(b"\"}\n\n");
        let rewritten = rewrite_all(&odd, paths, "demo", "upstream");
        assert!(
            rewritten
                .windows(14)
                .any(|window| window == b"\"model\":\"demo\"")
        );
        assert!(!rewritten.windows(8).any(|window| window == b"upstream"));
        assert!(rewritten.contains(&0xff));

        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(b"data: {\"model\":\"upstream\"}\n\n");
        let rewritten = rewrite_all(&bom, paths, "demo", "upstream");
        assert!(rewritten.starts_with(&[0xEF, 0xBB, 0xBF]));
        assert!(
            rewritten
                .windows(14)
                .any(|window| window == b"\"model\":\"demo\"")
        );
        assert!(!rewritten.windows(8).any(|window| window == b"upstream"));
    }

    #[test]
    fn an_event_over_the_cap_is_forwarded_before_finish() {
        let mut body = br#"data: {"model":"upstream","pad":""#.to_vec();
        body.resize((1 << 20) + 64, b'q');
        let mut tap = DecoderTap::with_cap(
            OpenAiChat.stream_decoder(),
            OpenAiChat.model_paths(),
            "demo",
            "upstream",
            1 << 20,
        );
        let forwarded = tap.push(&body);
        assert!(forwarded.len() > (1 << 20) - 64, "held {}", forwarded.len());
        assert!(!forwarded.windows(8).any(|window| window == b"upstream"));
        assert!(forwarded.windows(4).any(|window| window == b"demo"));
        let tail = tap.finish();
        let mut all = forwarded;
        all.extend(tail);
        assert!(!all.windows(8).any(|window| window == b"upstream"));
    }

    #[test]
    fn one_byte_chunks_of_a_long_event_stay_exact() {
        let mut event = br#"data: {"model":"upstream","big":123456789012345678901234567890,"f":10.10,"e":1e2,"s":"\u00e9\/","pad":""#.to_vec();
        event.extend(std::iter::repeat_n(b'a', 8 * 1024));
        event.extend_from_slice(br#""}"#);
        event.extend_from_slice(b"\n\n");
        let mut tap = OpenAiChat.raw_tap("demo", "upstream");
        let mut out = Vec::new();
        for byte in &event {
            out.extend(tap.push(std::slice::from_ref(byte)));
        }
        out.extend(tap.finish());
        assert!(
            out.windows(14)
                .any(|window| window == b"\"model\":\"demo\"")
        );
        assert!(!out.windows(8).any(|window| window == b"upstream"));
        assert!(out.windows(5).any(|window| window == b"10.10"));
        assert!(out.windows(3).any(|window| window == b"1e2"));
        assert!(
            out.windows(30)
                .any(|window| window == b"123456789012345678901234567890")
        );
        let same = OpenAiChat.raw_tap("upstream", "upstream");
        let mut copied = Vec::new();
        let mut same = same;
        for byte in &event {
            copied.extend(same.push(std::slice::from_ref(byte)));
        }
        copied.extend(same.finish());
        assert_eq!(copied, event);
    }

    #[test]
    fn a_stray_quote_on_a_comment_does_not_leak_the_upstream_model() {
        let paths: &[&[&str]] = &[&["model"]];
        for prefix in [": keep-alive \"\n\n", "data: upstream said \"hello\n\n"] {
            let body = format!(
                "{prefix}data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream-x\",\"choices\":[]}}\n\n"
            );
            let text = rewrite_all(body.as_bytes(), paths, "demo", "upstream");
            let text = String::from_utf8(text).expect("utf-8");
            assert!(text.contains("\"model\":\"demo\""), "{prefix}: {text}");
            assert!(!text.contains("upstream-x"), "{prefix}: {text}");
            assert!(text.contains(prefix.trim_end()), "{prefix}");
        }
    }

    #[test]
    fn tool_arguments_keep_their_own_model_field() {
        let anthropic = br#"{"id":"m","type":"message","role":"assistant","model":"upstream-x","content":[{"type":"tool_use","id":"t","name":"car","input":{"model":"tesla-3"}}],"stop_reason":"end_turn"}"#;
        let anthropic_text =
            String::from_utf8(AnthropicMessages.rewrite_model(anthropic, "demo", "upstream"))
                .expect("utf-8");
        assert!(
            anthropic_text.contains("\"model\":\"demo\""),
            "{anthropic_text}"
        );
        assert!(
            anthropic_text.contains("\"model\":\"tesla-3\""),
            "{anthropic_text}"
        );
        assert!(!anthropic_text.contains("upstream-x"), "{anthropic_text}");

        let nested = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"upstream-x\",",
            "\"content\":[{\"type\":\"tool_use\",\"input\":{\"model\":\"tesla-3\"}}]}}\n\n",
        );
        let mut tap = AnthropicMessages.raw_tap("demo", "upstream");
        let mut forwarded = tap.push(nested.as_bytes());
        forwarded.extend(tap.finish());
        let text = String::from_utf8(forwarded).expect("utf-8");
        assert!(text.contains("\"model\":\"demo\""), "{text}");
        assert!(text.contains("\"model\":\"tesla-3\""), "{text}");
        assert!(!text.contains("upstream-x"), "{text}");

        let language = br#"{"responseId":"g","modelVersion":"upstream-x","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"car","args":{"model":"tesla-3"}}}]}}]}"#;
        let language_text =
            String::from_utf8(GenerativeLanguage.rewrite_model(language, "demo", "upstream"))
                .expect("utf-8");
        assert!(
            language_text.contains("\"modelVersion\":\"demo\""),
            "{language_text}"
        );
        assert!(
            language_text.contains("\"model\":\"tesla-3\""),
            "{language_text}"
        );
        assert!(!language_text.contains("upstream-x"), "{language_text}");
    }

    #[test]
    fn a_partial_event_is_terminated_when_the_tap_finishes() {
        let mut tap = OpenAiChat.raw_tap("demo", "upstream");
        assert!(tap.push(br#"data: {"model":"upstream"#).is_empty());
        let tail = String::from_utf8(tap.finish()).expect("utf-8");
        assert_eq!(tail, "data: {\"model\":\"demo\"\n\n");
        assert!(!tail.contains("upstream"));
    }

    #[test]
    fn each_protocol_reports_stream_and_unary_usage_across_a_split_frame() {
        let chat = concat!(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream\",",
            "\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
        );
        assert_tokens(&OpenAiChat, chat.as_bytes(), chat_unary());
        let anthropic = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"model\":\"upstream\",\"content\":[],",
            "\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"output_tokens\":7}}\n\n",
        );
        assert_tokens(
            &AnthropicMessages,
            anthropic.as_bytes(),
            br#"{"id":"m","type":"message","role":"assistant","model":"upstream","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":7}}"#,
        );
        let responses = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"model\":\"upstream\",",
            "\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}}\n\n",
        );
        assert_tokens(
            &OpenAiResponses,
            responses.as_bytes(),
            br#"{"id":"r","model":"upstream","status":"completed","output":[],"usage":{"input_tokens":11,"output_tokens":7}}"#,
        );
        let language = concat!(
            "data: {\"responseId\":\"g\",\"modelVersion\":\"upstream\",",
            "\"usageMetadata\":{\"promptTokenCount\":11,\"candidatesTokenCount\":7}}\n\n",
        );
        assert_tokens(
            &GenerativeLanguage,
            language.as_bytes(),
            br#"{"responseId":"g","modelVersion":"upstream","candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7}}"#,
        );
    }

    fn assert_tokens(codec: &dyn Codec, stream: &[u8], unary: &[u8]) {
        let mut tap = codec.raw_tap("demo", "upstream");
        let mut forwarded = Vec::new();
        for byte in stream {
            forwarded.extend(tap.push(std::slice::from_ref(byte)));
        }
        forwarded.extend(tap.finish());
        assert_eq!(tap.tokens(), (Some(11), Some(7)), "{}", codec.id());
        let text = String::from_utf8_lossy(&forwarded);
        assert!(
            text.contains("demo") && !text.contains("upstream"),
            "{} model rewrite: {text}",
            codec.id()
        );
        assert_eq!(
            codec.response_tokens(unary),
            (Some(11), Some(7)),
            "{}",
            codec.id()
        );
        let rewritten = codec.rewrite_model(unary, "demo", "upstream");
        let unary_text = String::from_utf8(rewritten).expect("utf-8");
        assert!(
            unary_text.contains("demo") && !unary_text.contains("upstream"),
            "{} unary model: {unary_text}",
            codec.id()
        );
    }

    #[test]
    fn a_usage_frame_split_across_two_chunks_is_counted_once_it_completes() {
        let frame = concat!(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream\",",
            "\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
        );
        let mut tap = OpenAiChat.raw_tap("demo", "upstream");
        let split = frame.find("prompt_tokens").expect("field");
        let bytes = frame.as_bytes();
        let _ = tap.push(&bytes[..split]);
        assert_eq!(tap.tokens(), (None, None));
        let _ = tap.push(&bytes[split..]);
        let _ = tap.finish();
        assert_eq!(tap.tokens(), (Some(11), Some(7)));
    }

    fn chat_unary() -> &'static [u8] {
        br#"{"id":"c","object":"chat.completion","model":"upstream","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#
    }

    #[test]
    fn stream_error_frames_match_the_protocol() {
        let responses = String::from_utf8(OpenAiResponses.encode_stream_error(502, "overloaded"))
            .expect("utf-8");
        assert!(responses.starts_with("event: error\ndata: "), "{responses}");
        assert!(responses.contains("\"type\":\"error\""), "{responses}");
        assert!(responses.contains("\"code\":502"), "{responses}");
        assert!(
            responses.contains("\"message\":\"overloaded\""),
            "{responses}"
        );
        assert!(responses.ends_with("\n\n"), "{responses}");

        let chat = String::from_utf8(OpenAiChat.encode_stream_error(500, "nope")).expect("utf-8");
        assert!(chat.starts_with("data: {\"error\":"), "{chat}");
        assert!(!chat.contains("event:"), "{chat}");

        let language =
            String::from_utf8(GenerativeLanguage.encode_stream_error(503, "later")).expect("utf-8");
        assert!(language.starts_with("data: {\"error\":"), "{language}");
        assert!(!language.contains("event:"), "{language}");

        let anthropic =
            String::from_utf8(AnthropicMessages.encode_stream_error(429, "slow")).expect("utf-8");
        assert!(anthropic.starts_with("event: error\ndata: "), "{anthropic}");
    }

    #[test]
    fn stream_content_type_follows_the_protocol() {
        assert_eq!(OpenAiChat.stream_content_type(), "text/event-stream");
        assert_eq!(
            GenerativeLanguage.stream_content_type(),
            "text/event-stream"
        );
        assert_eq!(OpenAiResponses.stream_content_type(), "text/event-stream");
        assert_eq!(AnthropicMessages.stream_content_type(), "text/event-stream");
        assert_eq!(
            Converse.stream_content_type(),
            "application/vnd.amazon.eventstream"
        );
        assert_eq!(Converse.response_content_type(false), "application/json");
    }
}
