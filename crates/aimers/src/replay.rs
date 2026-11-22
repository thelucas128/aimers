//! Turns the JSONL env log into a readable replay: the script's statements in
//! the order they ran, with the instances it made bound to names, grouped by
//! the source line that produced them.
//!
//! The JSONL is the environment log proper (every read, write and call);
//! this file is the same events with the reads folded away and runs of the
//! same statement (a `WaitForChild` polled every frame) collapsed, so a
//! person can read what the script did.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Default)]
pub struct Stats {
    pub calls: u64,
    pub sets: u64,
    pub gets: u64,
    pub instances: u64,
    pub globals: u64,
    pub connects: u64,
    pub waits: u64,
}

fn var_name(n: u64) -> String {
    format!("v{n}")
}

/// `script:line` -> the script and the line number.
fn split_src(src: &str) -> Option<(&str, usize)> {
    let (file, n) = src.rsplit_once(':')?;
    Some((file, n.parse().ok()?))
}

/// The script's own source: a result is named after the variable its line
/// assigns (`local part = Instance.new("Part")` -> `part`), when the line
/// assigns exactly one. Chunks without a file (`loadstring`) keep `vN`.
pub struct Source {
    name: String,
    lines: Vec<String>,
}

impl Source {
    pub fn new(name: &str, text: &str) -> Self {
        Self {
            name: name.to_string(),
            lines: text.lines().map(|l| l.trim_start_matches('\u{feff}').to_string()).collect(),
        }
    }

    /// The variable the line at `src` (`file:line`) assigns, if it is this
    /// script and the line is a single assignment.
    fn assigned(&self, src: &str) -> Option<String> {
        assigned_name(self.line(src)?)
    }

    /// The parameter list of the first `function(...)` on the line at `src`
    /// (`Part.Touched:Connect(function(hit)` -> `hit`).
    fn params(&self, src: &str) -> Option<String> {
        let line = self.line(src)?;
        // On a minified one-line script the first `function(` belongs to the
        // script's own machinery, not this connection's handler; naming its
        // parameters would be a guess, so the replay says `...`.
        if line.len() > 300 {
            return None;
        }
        let start = line.find("function(")? + "function(".len();
        let rest = &line[start..];
        let end = rest.find(')')?;
        Some(rest[..end].trim().to_string())
    }

    /// The text of the line at `src`, when it belongs to this script.
    fn line(&self, src: &str) -> Option<&str> {
        let (script, line) = src.rsplit_once(':')?;
        if script.rsplit('.').next() != Some(self.name.as_str()) {
            return None;
        }
        let line: usize = line.parse().ok()?;
        self.lines.get(line.checked_sub(1)?).map(|l| l.as_str())
    }

    /// The keyword a condition this line starts with (`if`, `elseif` or
    /// `while`): only a call there can have opened a branch by returning
    /// true, or closed one by returning false.
    fn condition(&self, src: &str) -> Option<&'static str> {
        let line = self.line(src)?.trim_start();
        if line.starts_with("if ") || line.starts_with("if(") {
            Some("if")
        } else if line.starts_with("elseif ") || line.starts_with("elseif(") {
            Some("elseif")
        } else if line.starts_with("while ") || line.starts_with("while(") {
            Some("while")
        } else {
            None
        }
    }

    /// The line where the block opened at `src` closes, counting Lua's block
    /// words forward (`if`/`function`/`while`/`for`/`repeat` open, `end`/
    /// `until` close). `None` when the source cannot say: a minified line is
    /// the whole program, and a block left open has no end.
    fn block_end(&self, src: &str) -> Option<usize> {
        let (_, number) = src.rsplit_once(':')?;
        let start: usize = number.parse().ok()?;
        if self.line(src)?.len() > 300 {
            return None;
        }
        // An `elseif` opens nothing of its own: start a level in, so the one
        // `end` that closes the whole chain closes it here too.
        let mut depth = if self.condition(src) == Some("elseif") { 1 } else { 0 };
        for (i, text) in self.lines.iter().enumerate().skip(start - 1) {
            for word in words(text) {
                match word {
                    "if" | "function" | "while" | "for" | "repeat" => depth += 1,
                    "end" | "until" => depth -= 1,
                    _ => {}
                }
            }
            if depth <= 0 {
                return Some(i + 1);
            }
        }
        None
    }
}

/// Lua's words on a line, with strings and comments dropped: enough to count
/// the block keys a line opens and closes.
fn words(line: &str) -> Vec<&str> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => break,
            b'"' | b'\'' => {
                let quote = b[i];
                i += 1;
                while i < b.len() && b[i] != quote {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                // `[[ ... ]]` / `[==[ ... ]==]` holds no words.
                let mut j = i + 1;
                let mut eq = 0;
                while b.get(j) == Some(&b'=') {
                    eq += 1;
                    j += 1;
                }
                if b.get(j) != Some(&b'[') {
                    i += 1;
                    continue;
                }
                let close = format!("]{}]", "=".repeat(eq));
                match line[j + 1..].find(&close) {
                    Some(k) => i = j + 1 + k + close.len(),
                    None => break,
                }
            }
            c if (c as char).is_ascii_alphanumeric() || c == b'_' => {
                let start = i;
                while i < b.len() && ((b[i] as char).is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(&line[start..i]);
            }
            _ => i += 1,
        }
    }
    out
}

/// `local part: Part = ...` / `part = ...` -> `part`; anything else
/// (`local a, b = ...`, `x += 1`, `if x == y then`) -> None.
fn assigned_name(line: &str) -> Option<String> {
    let t = line.trim();
    let t = t.strip_prefix("local ").unwrap_or(t);
    let (lhs, _) = t.split_once('=')?;
    let lhs = lhs.trim();
    if lhs.contains(',') {
        return None;
    }
    let name = lhs.split(':').next()?.trim();
    is_ident(name).then(|| name.to_string())
}

fn lua_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Where an instance shows up as an expression when there's no binding: the
/// path from `game` (`game:GetService("Players").LocalPlayer`,
/// `workspace:FindFirstChild("Model")`), using the service local when one
/// has been declared.
fn path_expr(path: &str, services: &HashMap<String, String>) -> String {
    if path.is_empty() || path == "Instance" {
        return "game".to_string();
    }
    let mut parts = path.split('.');
    let first = parts.next().unwrap_or("");
    let mut expr = match services.get(first) {
        Some(name) => name.clone(),
        None if first == "Workspace" => "workspace".to_string(),
        None => format!("game:GetService({})", lua_string(first)),
    };
    let mut rest = parts.peekable();
    // The one player in the DataModel is the local one.
    if first == "Players" && rest.peek() == Some(&"Player") {
        rest.next();
        expr = format!("{expr}.LocalPlayer");
    }
    for part in rest {
        if is_ident(part) {
            expr.push('.');
            expr.push_str(part);
        } else {
            expr.push_str(&format!("[{}]", lua_string(part)));
        }
    }
    expr
}

/// Binds instances to names the replay can reuse: the `v0`, `v1`, ...
/// variables for `Instance.new` results, and a `local Players = ...` for
/// each service (or `LocalPlayer`) the first time the script touches it.
struct Resolver {
    /// Instance id -> the expression that produced it (`v0`).
    vars: HashMap<String, String>,
    /// Instance id -> a named local (`Players`, `LocalPlayer`, ...).
    locals: HashMap<String, String>,
    /// Service name -> its local, once declared.
    services: HashMap<String, String>,
    declared: HashSet<String>,
    pending: Vec<String>,
    next_var: u64,
}

impl Resolver {
    fn new() -> Self {
        Self {
            vars: HashMap::new(),
            locals: HashMap::new(),
            services: HashMap::new(),
            declared: HashSet::new(),
            pending: Vec::new(),
            next_var: 0,
        }
    }

    fn new_var(&mut self, id: &str) -> String {
        let name = var_name(self.next_var);
        self.next_var += 1;
        self.vars.insert(id.to_string(), name.clone());
        name
    }

    /// Binds an instance to a name from the source, unless the name is
    /// already another instance's.
    fn bind(&mut self, id: &str, name: String) -> bool {
        if self.declared.contains(&name) {
            return false;
        }
        self.declared.insert(name.clone());
        self.vars.insert(id.to_string(), name);
        true
    }

    /// `local Players = game:GetService("Players")`, once per service. A
    /// service the script gets by name is declared where the `GetService`
    /// ran, so later members read as `Players.LocalPlayer`.
    fn service(&mut self, name: &str) -> String {
        self.service_named(name, None)
    }

    /// `service`, preferring the variable the script's own line assigns
    /// (`local Http = game:GetService("HttpService")`).
    fn service_named(&mut self, name: &str, hint: Option<String>) -> String {
        if name == "Workspace" {
            return "workspace".to_string();
        }
        if let Some(local) = self.services.get(name) {
            return local.clone();
        }
        let local = match hint {
            Some(h) if !self.declared.contains(&h) => h,
            _ => name.to_string(),
        };
        if self.declared.insert(local.clone()) {
            self.pending.push(format!(
                "local {local} = game:GetService({})",
                lua_string(name)
            ));
        }
        self.services.insert(name.to_string(), local.clone());
        local
    }

    fn subject(&mut self, id: &str, path: &str) -> String {
        if let Some(v) = self.vars.get(id) {
            return v.clone();
        }
        if let Some(v) = self.locals.get(id) {
            return v.clone();
        }
        let segs: Vec<&str> = path.split('.').collect();
        if segs.len() == 1 && !path.is_empty() && path != "Instance" {
            // A service: the top level of the DataModel.
            let name = self.service(path);
            if !id.is_empty() {
                self.locals.insert(id.to_string(), name.clone());
            }
            return name;
        }
        if segs.len() == 2 && segs[0] == "Players" {
            let players = self.service("Players");
            if self.declared.insert("LocalPlayer".to_string()) {
                self.pending
                    .push(format!("local LocalPlayer = {players}.LocalPlayer"));
            }
            if !id.is_empty() {
                self.locals.insert(id.to_string(), "LocalPlayer".to_string());
            }
            return "LocalPlayer".to_string();
        }
        path_expr(path, &self.services)
    }

    fn take_decls(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending)
    }
}

/// One `if <test> then` (or `while <test> do`) opened by a result that came
/// back true: it closes when the source passes the line holding the block's
/// `end`.
struct Guard {
    base: usize,
    end_line: usize,
    /// The source line that holds the test: a loop tests it again each
    /// iteration, and only the first test opened the block.
    src: String,
}

/// The replay's lines. A statement is written once, however many times it
/// ran (`×N` on its line); a source line's heading is held back until
/// something prints under it, so lines whose reads were all already noted
/// don't leave empty `-- line N` blocks.
struct Out {
    lines: Vec<String>,
    /// The dedupe key of each line, so a statement that ran again bumps the
    /// count on the line already written.
    keys: Vec<Option<String>>,
    /// Statements written so far, by callback context, indent, source line
    /// and text.
    seen: HashMap<String, u32>,
    /// The last statement written, still open to a `ret` naming its result.
    pending: Option<(String, usize, String, Option<String>)>,
    heading: Option<(String, String)>,
    /// The file and line heading last printed: a statement that follows a
    /// note on the same line shares that heading instead of repeating it.
    shown_heading: Option<(String, String)>,
    last_file: String,
    /// How deep inside a `:Connect(function ...)` the statements are.
    indent: usize,
    /// The source line the next statement runs on: a note holds onto it so
    /// it can be written under the statement it describes, not the next one.
    src: String,
    /// The connection whose handler is being written (0 outside one): two
    /// handlers can run the same statement, and each body keeps its own.
    context: u64,
    /// Comments that belong after the statement they describe (the instance
    /// `--satisfy` made for a call), held so that call can still be rewritten
    /// into the result's binding (`local x = ...`).
    notes: Vec<(String, usize, String)>,
    guards: Vec<Guard>,
}

impl Out {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            keys: Vec::new(),
            seen: HashMap::new(),
            pending: None,
            heading: None,
            shown_heading: None,
            last_file: String::new(),
            indent: 0,
            src: String::new(),
            context: 0,
            notes: Vec::new(),
            guards: Vec::new(),
        }
    }

    fn pad(&self, indent: usize) -> String {
        "    ".repeat(indent)
    }

    fn emit(&mut self, indent: usize, text: String, key: Option<String>) {
        self.lines.push(format!("{}{text}", self.pad(indent)));
        self.keys.push(key);
    }

    /// What makes two statements the same one: the handler they ran in, how
    /// deep, the source line, and the text (with its arguments and values).
    fn key(&self, text: &str) -> String {
        format!("{}\u{1}{}\u{1}{}\u{1}{}", self.context, self.indent, self.src, text)
    }

    fn line_no(&self) -> usize {
        self.src
            .rsplit_once(':')
            .and_then(|(_, n)| n.parse().ok())
            .unwrap_or(0)
    }

    fn flush_pending(&mut self) -> std::io::Result<()> {
        let Some((text, indent, src, key)) = self.pending.take() else { return Ok(()) };
        self.emit(indent, text, key);
        let (mine, rest) = std::mem::take(&mut self.notes)
            .into_iter()
            .partition(|(_, _, s)| *s == src);
        self.notes = rest;
        for (text, indent, _) in mine {
            self.emit(indent, text, None);
        }
        Ok(())
    }

    /// A note the log ended on (or one with no statement around it) still
    /// goes out in order.
    fn flush_notes(&mut self) -> std::io::Result<()> {
        for (text, indent, _) in std::mem::take(&mut self.notes) {
            self.emit(indent, text, None);
        }
        Ok(())
    }

    fn flush_heading(&mut self) -> std::io::Result<()> {
        let Some((file, line_no)) = self.heading.take() else { return Ok(()) };
        let indent = self.indent;
        // A note already printed under this line's heading: the statement
        // that follows it belongs to the same line.
        if self.shown_heading.as_ref() == Some(&(file.clone(), line_no.clone())) {
            return Ok(());
        }
        if file != self.last_file {
            if !self.last_file.is_empty() {
                self.lines.push(String::new());
                self.keys.push(None);
            }
            self.emit(indent, format!("-- {file}"), None);
            self.last_file = file.clone();
        }
        self.emit(indent, format!("-- line {line_no}"), None);
        self.shown_heading = Some((file, line_no));
        Ok(())
    }

    /// A heading for the source line the next statement runs on. Its indent
    /// is the statement's: a block can close between the two.
    fn at(&mut self, file: &str, line_no: &str) {
        self.heading = Some((file.to_string(), line_no.to_string()));
    }

    /// A source line that printed nothing printed no heading either.
    fn discard_heading(&mut self) {
        self.heading = None;
    }

    /// A statement. A statement that ran before is not written again: the
    /// source has it once, and its line says how many times it ran.
    fn line(&mut self, text: impl Into<String>) -> std::io::Result<()> {
        let text = text.into();
        let key = self.key(&text);
        if let Some(n) = self.seen.get_mut(&key) {
            *n += 1;
            // The statement printed nothing, so its heading is not printed
            // either (it was left for this statement to write).
            self.discard_heading();
            return Ok(());
        }
        // The previous line goes out before the heading for this one.
        self.flush_pending()?;
        // ... and before the `end` of a block this statement sits outside of.
        self.close_guards(self.line_no())?;
        let key = self.key(&text);
        if let Some(n) = self.seen.get_mut(&key) {
            *n += 1;
            self.discard_heading();
            return Ok(());
        }
        self.seen.insert(key.clone(), 1);
        let indent = self.indent;
        self.flush_heading()?;
        self.pending = Some((text, indent, self.src.clone(), Some(key)));
        Ok(())
    }

    /// A statement that always prints, even if it says the same thing as one
    /// already written: a `:Connect(function` and its `end)` are structure,
    /// and the body between them is this connection's own.
    fn line_always(&mut self, text: impl Into<String>) -> std::io::Result<()> {
        let text = text.into();
        self.flush_pending()?;
        self.close_guards(self.line_no())?;
        let indent = self.indent;
        self.flush_heading()?;
        self.pending = Some((text, indent, self.src.clone(), None));
        Ok(())
    }

    /// The statement written last is still open: a `ret` that names its
    /// result can still turn it into `local x = ...`.
    fn pending_is(&self, text: &str) -> bool {
        matches!(&self.pending, Some((t, _, _, _)) if t == text)
    }

    /// A test that came back true opened a block: its statements go one
    /// level in, and `end` comes back when the source has closed it.
    fn open_guard(&mut self, end_line: usize, src: &str) {
        self.guards.push(Guard { base: self.indent, end_line, src: src.to_string() });
        self.indent += 1;
    }

    /// The test on this source line already opened its block: a loop tests
    /// its condition again on every pass, including the one that ends it.
    fn guard_is_open(&self, end_line: usize, src: &str) -> bool {
        self.guards
            .iter()
            .any(|g| g.end_line == end_line && g.src == src)
    }

    /// The last statement has not been written yet: drop it, because the
    /// event it came from was a repeated test, not a statement of its own.
    fn drop_pending(&mut self, text: &str) {
        let Some((t, _, _, key)) = &self.pending else { return };
        if t != text {
            return;
        }
        if let Some(key) = key {
            self.seen.remove(key);
        }
        self.pending = None;
        self.discard_heading();
    }

    /// A statement dropped after its heading printed (a test that opened
    /// nothing) leaves its heading standing over nothing. Take it back when
    /// the next statement writes a heading of its own.
    fn retract_heading(&mut self, file: &str, line_no: &str) {
        if self.pending.is_some() {
            return;
        }
        let heading = format!("-- line {line_no}");
        if self.lines.last().is_some_and(|l| l.trim_start() == heading) {
            self.lines.pop();
            self.keys.pop();
            if self.shown_heading.as_ref() == Some(&(file.to_string(), line_no.to_string())) {
                self.shown_heading = None;
            }
        }
    }

    /// Close the `if` blocks the current source line is past (or that the
    /// handler they ran in has ended), writing their `end`.
    fn close_guards(&mut self, line: usize) -> std::io::Result<()> {
        while let Some(g) = self.guards.last() {
            if line <= g.end_line && self.indent > g.base {
                break;
            }
            let base = g.base;
            self.guards.pop();
            self.indent = base;
            self.flush_pending()?;
            self.emit(base, "end".to_string(), None);
        }
        Ok(())
    }

    /// A comment that goes after the statement it describes (the pending
    /// statement stays rewriteable: a `ret` still binds it to its variable).
    fn note(&mut self, text: impl Into<String>) {
        let indent = self.indent;
        self.notes.push((text.into(), indent, self.src.clone()));
    }

    /// The last statement is still pending: if it is exactly `expected` (a
    /// call whose result a `ret` event just named), swap in its binding.
    fn rewrite_last(&mut self, expected: &str, replacement: String) -> bool {
        match &mut self.pending {
            Some((text, _, _, _)) if text == expected => {
                *text = replacement;
                true
            }
            _ => false,
        }
    }

    /// A comment: flushes any run and heading first.
    fn raw(&mut self, text: impl AsRef<str>) -> std::io::Result<()> {
        self.flush_pending()?;
        self.flush_notes()?;
        self.flush_heading()?;
        let indent = self.indent;
        self.emit(indent, text.as_ref().to_string(), None);
        Ok(())
    }

    /// Code the replay adds itself (a named local), never folded.
    fn code(&mut self, text: impl AsRef<str>) -> std::io::Result<()> {
        self.raw(text)
    }

    /// The file's text: every line, and `-- ×N` on the ones that ran more
    /// than once.
    fn finish(&mut self) -> String {
        self.flush_pending().ok();
        for (i, key) in self.keys.iter().enumerate() {
            let Some(key) = key else { continue };
            let n = self.seen.get(key).copied().unwrap_or(1);
            if n > 1 {
                self.lines[i].push_str(&format!("  -- ×{n}"));
            }
        }
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }
}

/// One run of a connection's handler: the events between its `cb` start and
/// end. `clean` means no other thread's events sat in between, so the run
/// can print inside the `:Connect(...)` that made it.
#[derive(Clone)]
struct Span {
    start: usize,
    end: usize,
    clean: bool,
    /// The handler had not returned when the log ended.
    open: bool,
}

/// The handler runs of every connection, by connection id.
fn callback_spans(events: &[serde_json::Value]) -> HashMap<u64, Vec<Span>> {
    struct Open {
        cid: u64,
        tid: String,
        start: usize,
        clean: bool,
    }
    let mut out: HashMap<u64, Vec<Span>> = HashMap::new();
    let mut open: Vec<Open> = Vec::new();
    for (i, v) in events.iter().enumerate() {
        let tid = v.get("tid").and_then(|t| t.as_str()).unwrap_or("");
        let ev = v.get("ev").and_then(|e| e.as_str()).unwrap_or("");
        if ev == "cb" {
            let cid = v.get("cid").and_then(|c| c.as_u64()).unwrap_or(0);
            let phase = v.get("phase").and_then(|p| p.as_str()).unwrap_or("");
            if phase == "start" {
                open.push(Open { cid, tid: tid.to_string(), start: i, clean: true });
            } else if let Some(k) = open.iter().rposition(|o| o.tid == tid) {
                let o = open.remove(k);
                out.entry(o.cid)
                    .or_default()
                    .push(Span { start: o.start, end: i, clean: o.clean, open: false });
            }
            continue;
        }
        // A plain event interrupts every open handler that is not its own
        // thread: those two were suspended and resumed around each other.
        match open.iter().rposition(|o| o.tid == tid) {
            Some(k) => {
                for o in &mut open[k + 1..] {
                    o.clean = false;
                }
            }
            None => {
                for o in open.iter_mut() {
                    o.clean = false;
                }
            }
        }
    }
    // A handler still running when the log ended never returned: its span is
    // open, and the replay should not pretend it was empty.
    for o in open {
        out.entry(o.cid)
            .or_default()
            .push(Span { start: o.start, end: events.len(), clean: false, open: true });
    }
    out
}

/// A test that came back false on an `if` line. Its else/elseif body may
/// still run, and that is when the `if` line can first be written: nothing
/// ran inside the test's own branch.
#[derive(Clone)]
struct IfHead {
    /// The full `script:line` of the `if`.
    src: String,
    file: String,
    line: usize,
    /// The `end` that closes the chain.
    end_line: usize,
    /// The test as it ran, with its arguments (`lib:IsA("Folder")`).
    call: String,
}

/// The replay as it is written: the log's events, the names they resolve to,
/// and how far inside a callback the last statement went.
struct Render<'a> {
    o: Out,
    r: Resolver,
    stats: Stats,
    seen_globals: HashSet<String>,
    /// The member reads already noted, with the value each answered: a
    /// property that changes value reads again.
    seen_gets: HashSet<(String, String, String)>,
    last_args: HashMap<(String, String), String>,
    /// The last call statement written, with the script line it ran on and
    /// the method it called. A result that came back through an engine helper
    /// (`WaitForChild`'s own `FindFirstChild`) binds this statement, not the
    /// helper's.
    last_call: Option<(String, String, String)>,
    /// A false `if` waiting for its `else`/`elseif` to run.
    if_head: Option<IfHead>,
    /// The globals read since the last statement, and the source line the
    /// first of them was read on.
    reads: Vec<String>,
    reads_src: String,
    /// The loaded sources and fetched bodies already written: a body is
    /// written once, however many times it is loaded or fetched.
    seen_bodies: HashSet<String>,
    current_src: String,
    source: Option<&'a Source>,
    events: &'a [serde_json::Value],
    spans: HashMap<u64, Vec<Span>>,
    /// Event indices already written inside a callback.
    consumed: HashSet<usize>,
}

impl Render<'_> {
    /// A held false `if` meets the next event. Inside the chain, its body is
    /// the `else`, and the `if` can now be written; past the chain, the test
    /// is only worth a comment.
    fn spill_head(&mut self, src: &str) -> std::io::Result<()> {
        let Some(h) = self.if_head.clone() else { return Ok(()) };
        let Some((file, line)) = split_src(src) else { return Ok(()) };
        if file != h.file {
            return Ok(());
        }
        if line > h.end_line {
            self.if_head = None;
            self.o.raw(format!("-- if {} then (false)", h.call))?;
            return Ok(());
        }
        if line <= h.line {
            return Ok(());
        }
        // Another condition of the chain is not the chain's body.
        if matches!(
            self.source.and_then(|s| s.condition(src)),
            Some("if") | Some("elseif") | Some("while")
        ) {
            return Ok(());
        }
        self.if_head = None;
        self.o.line_always(format!("if {} then", h.call))?;
        self.o.line_always("else")?;
        self.o.open_guard(h.end_line, &h.src);
        Ok(())
    }

    /// The globals read since the last statement, as one comment under the
    /// line they were read on. A VM reads dozens of globals for a single
    /// statement, and one `-- reads` line per name drowns that statement.
    fn flush_reads(&mut self) -> std::io::Result<()> {
        if self.reads.is_empty() {
            return Ok(());
        }
        let names = std::mem::take(&mut self.reads);
        let src = std::mem::take(&mut self.reads_src);
        if let Some((file, line)) = split_src(&src) {
            let line = line.to_string();
            self.o.at(file, &line);
            self.o.src = src.clone();
            self.current_src = src;
        } else {
            // No source line (a chunk without a file): a comment still says
            // what was read, without borrowing the previous line's heading.
            self.o.discard_heading();
        }
        // Eight to a line: wide read sets stay readable without a wall of
        // comments. A VM reads its whole string table on one line; the log
        // has every name, so the replay shows the first few and counts the
        // rest.
        const SHOWN: usize = 24;
        let shown = names.len().min(SHOWN);
        for chunk in names[..shown].chunks(8) {
            self.o.line(format!("-- reads {}", chunk.join(", ")))?;
        }
        if names.len() > shown {
            self.o.line(format!(
                "-- ... and {} more global read(s) on this line",
                names.len() - shown
            ))?;
        }
        Ok(())
    }

    /// The log ended while a false `if` was still held: it ran nothing, so
    /// it is a comment rather than a block.
    fn flush_head_end(&mut self) -> std::io::Result<()> {
        if let Some(h) = self.if_head.take() {
            self.o.raw(format!("-- if {} then (false)", h.call))?;
        }
        Ok(())
    }

    fn run(&mut self, i: usize) -> std::io::Result<()> {
        let v = &self.events[i];
        let ev = v.get("ev").and_then(|e| e.as_str()).unwrap_or("");
        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let path = v.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let key = v.get("key").and_then(|k| k.as_str()).unwrap_or("");
        let value = v.get("value").and_then(|x| x.as_str()).unwrap_or("");
        let src = v.get("src").and_then(|s| s.as_str()).unwrap_or("");

        // A held `if` belongs before whatever this event writes.
        self.spill_head(src)?;
        // The globals read since the last statement go out as one comment,
        // before the statement they were read for.
        if ev != "global_get" {
            self.flush_reads()?;
        }

        // A new source line: its heading prints when the next statement does.
        // A location without a line number (`[C]`, a callback of unknown
        // origin) is not a source line, so it writes no heading.
        if !src.is_empty() && src != self.current_src {
            if let Some((file, line_no)) = src.rsplit_once(':') {
                if line_no.parse::<usize>().is_ok() {
                    self.o.at(file, line_no);
                }
            }
            self.o.src = src.to_string();
            self.current_src = src.to_string();
        }

        match ev {
            "new" => {
                self.stats.instances += 1;
                let class = v.get("class").and_then(|c| c.as_str()).unwrap_or("Instance");
                // The variable the line assigns (`local part = Instance.new(...)`),
                // else the instance's own (initial) name.
                let from_line = self.source.and_then(|s| s.assigned(src));
                let var = match from_line {
                    Some(n) if self.r.bind(id, n.clone()) => n,
                    _ => match result_name(path) {
                        Some(n) if self.r.bind(id, n.clone()) => n,
                        _ => self.r.new_var(id),
                    },
                };
                self.o
                    .line(format!("local {var} = Instance.new({})", lua_string(class)))?;
            }
            // `--satisfy` made the instance the script was waiting on.
            "made" => {
                self.stats.instances += 1;
                let class = v.get("class").and_then(|c| c.as_str()).unwrap_or("Instance");
                self.o.note(format!(
                    "-- aimers: made {path} ({class}) so the script could continue"
                ));
            }
            // ... and gave a used one the class its members belong to.
            "class" => {
                let class = v.get("class").and_then(|c| c.as_str()).unwrap_or("Instance");
                let stub = v.get("stub").and_then(|b| b.as_bool()).unwrap_or(false);
                let tail = if stub {
                    "; it returns a permissive stub"
                } else {
                    ""
                };
                self.o.raw(format!(
                    "-- aimers: {path} is a {class} here (the server would have made it){tail}"
                ))?;
            }
            "set" => {
                self.stats.sets += 1;
                let who = self.r.subject(id, path);
                let vid = v.get("vid").and_then(|x| x.as_str()).unwrap_or("");
                let vpath = v.get("vpath").and_then(|x| x.as_str()).unwrap_or("");
                let rhs = if !vid.is_empty() {
                    self.r.subject(vid, vpath)
                } else if value == "<function>" {
                    // A callback property: the script assigned a closure, and
                    // the log knows that much about it.
                    "function(...)".to_string()
                } else {
                    value.to_string()
                };
                emit_decls(&mut self.o, &mut self.r)?;
                self.o.line(format!("{} = {rhs}", member(&who, key)))?;
            }
            "call" => {
                self.stats.calls += 1;
                let who = self.r.subject(id, path);
                emit_decls(&mut self.o, &mut self.r)?;
                let args = v.get("args").and_then(|a| a.as_str()).unwrap_or("[]");
                let args = args.trim_start_matches('[').trim_end_matches(']');
                // `game:GetService("Players")` becomes the local's
                // declaration; the next event on that service reads `Players`.
                if who == "game" && matches!(key, "GetService" | "service" | "getService" | "FindService") {
                    if let Some(service) = arg_string(args) {
                        let hint = self.source.and_then(|s| s.assigned(src));
                        self.r.service_named(&service, hint);
                        emit_decls(&mut self.o, &mut self.r)?;
                        return Ok(());
                    }
                }
                self.last_args
                    .insert((id.to_string(), key.to_string()), args.to_string());
                let args = instances_in(args, &self.r);
                let stmt = format!("{who}:{key}({args})");
                self.o.line(stmt.clone())?;
                if !src.is_empty() {
                    self.last_call = Some((stmt, src.to_string(), key.to_string()));
                }
            }
            // A call that returned an instance: bind its result to a name, at
            // the call itself (`local Baseplate = workspace:FindFirstChild("Baseplate")`);
            // later uses of the instance read as that name.
            "ret" => {
                let rid = v.get("rid").and_then(|x| x.as_str()).unwrap_or("");
                let rpath = v.get("rpath").and_then(|x| x.as_str()).unwrap_or("");
                if id.is_empty() || self.r.vars.contains_key(id) || self.r.locals.contains_key(id) {
                    return Ok(());
                }
                // `GetService` already became the service's declaration.
                if let Some(local) = self.r.services.get(path) {
                    self.r.locals.insert(id.to_string(), local.clone());
                    return Ok(());
                }
                // Only the call on this script line can bind this result:
                // an engine helper's result (`require`'s own `IsA`, a
                // `WaitForChild`'s `FindFirstChild`) is the script's own
                // statement, and an earlier line's call is not this one's.
                let Some((last, _, last_key)) = self.last_call.clone().filter(|(_, s, _)| s == src) else {
                    return Ok(());
                };
                let who = self.r.subject(rid, rpath);
                let args = self
                    .last_args
                    .get(&(rid.to_string(), key.to_string()))
                    .filter(|_| last_key == key)
                    .cloned();
                let call = match args {
                    Some(args) => format!("{who}:{key}({})", instances_in(&args, &self.r)),
                    // The helper's key differs from the script's own call.
                    None => last.clone(),
                };
                // The variable the line assigns, else the instance's own name.
                let mut var = None;
                if let Some(n) = self.source.and_then(|s| s.assigned(src)) {
                    if self.r.bind(id, n.clone()) {
                        var = Some(n);
                    }
                }
                if var.is_none() {
                    if let Some(n) = result_name(path) {
                        if self.r.bind(id, n.clone()) {
                            var = Some(n);
                        }
                    }
                }
                let var = var.unwrap_or_else(|| self.r.new_var(id));
                let stmt = format!("local {var} = {call}");
                let bound = self.o.rewrite_last(&call, stmt.clone())
                    || self.o.rewrite_last(&last, stmt.clone());
                if !bound {
                    emit_decls(&mut self.o, &mut self.r)?;
                    self.o.code(stmt)?;
                }
                // This result belongs to this call; a later helper result on
                // the same line is a different call's.
                self.last_call = None;
            }
            // A call that returned a value of another type: the line assigns
            // it (`local text = HttpService:JSONEncode(data)`), or, when the
            // call was a test that came back true, the statements that ran
            // under it go inside `if <test> then ... end`.
            "ret_value" => {
                let rid = v.get("rid").and_then(|x| x.as_str()).unwrap_or("");
                let rpath = v.get("rpath").and_then(|x| x.as_str()).unwrap_or("");
                // As in `ret`: only the call on this script line, not a
                // helper's result returned under a line that did not call it.
                let Some((last, _, last_key)) = self.last_call.clone().filter(|(_, s, _)| s == src) else {
                    return Ok(());
                };
                let who = self.r.subject(rid, rpath);
                let args = self
                    .last_args
                    .get(&(rid.to_string(), key.to_string()))
                    .filter(|_| last_key == key)
                    .cloned();
                let call = match args {
                    Some(args) => format!("{who}:{key}({})", instances_in(&args, &self.r)),
                    None => last.clone(),
                };
                let stored = self.source.and_then(|s| s.assigned(src)).is_some();
                let cond = self.source.and_then(|s| s.condition(src));
                let line = split_src(src).map(|(_, n)| n).unwrap_or(0);
                // A false test ran nothing of its own. Hold the `if` back:
                // if an `elseif` or the `else` body of the chain runs, that
                // is where the line belongs; otherwise it is a comment.
                if value == "false" && !stored {
                    let end = self.source.and_then(|s| s.block_end(src));
                    if let (Some(kw), Some(end), Some((file, _))) = (cond, end, split_src(src)) {
                        if kw == "if" {
                            self.if_head = Some(IfHead {
                                src: src.to_string(),
                                file: file.to_string(),
                                line,
                                end_line: end,
                                call: call.clone(),
                            });
                        }
                        if kw == "if" || kw == "elseif" {
                            self.o.drop_pending(&call);
                            self.o.drop_pending(&last);
                            self.last_call = None;
                            return Ok(());
                        }
                    }
                }
                // The block the call guarded: it ran because the test was true.
                // Only a condition (`if`/`elseif`/`while`) has a block, and
                // only the source can say where it closes; a minified line is
                // the whole program, so the guard is left out there.
                if value == "true" && !stored {
                    let block = self
                        .source
                        .and_then(|s| Some((s.condition(src)?, s.block_end(src)?)));
                    if let Some((kw, end)) = block {
                        if kw == "elseif" {
                            // The chain's `if` came back false: its line is
                            // written here, above the branch that did run.
                            let head = self.if_head.take().filter(|h| {
                                split_src(src).map(|(file, _)| file == h.file).unwrap_or(false)
                                    && h.end_line == end
                                    && h.line < line
                            });
                            self.o.drop_pending(&call);
                            self.o.drop_pending(&last);
                            if let Some(h) = head {
                                if let Some((f, _)) = split_src(src) {
                                    self.o.retract_heading(f, &line.to_string());
                                }
                                self.o.retract_heading(&h.file, &h.line.to_string());
                                self.o.at(&h.file, &h.line.to_string());
                                self.o.line_always(format!("if {} then", h.call))?;
                                if let Some((f, _)) = split_src(src) {
                                    self.o.at(f, &line.to_string());
                                }
                            }
                            self.o.line_always(format!("elseif {call} then"))?;
                            self.o.open_guard(end, src);
                            self.last_call = None;
                            return Ok(());
                        }
                        let tail = if kw == "while" { "do" } else { "then" };
                        let open = format!("{kw} {call} {tail}");
                        if self.o.guard_is_open(end, src) {
                            // A loop re-testing its condition: the branch is
                            // already written, and this pass changed nothing.
                            self.o.drop_pending(&call);
                            self.last_call = None;
                            return Ok(());
                        }
                        if self.o.rewrite_last(&call, open.clone())
                            || self.o.rewrite_last(&last, open)
                        {
                            self.if_head = None;
                            self.o.open_guard(end, src);
                            self.last_call = None;
                            return Ok(());
                        }
                    }
                }
                let Some(name) = self.source.and_then(|s| s.assigned(src)) else {
                    // Nobody stored the result: the log still says what came back.
                    if !value.is_empty()
                        && (self.o.pending_is(&call) || self.o.pending_is(&last))
                    {
                        self.o.note(format!("-- returns {value}"));
                    }
                    return Ok(());
                };
                if self.r.declared.contains(&name) {
                    return Ok(());
                }
                let stmt = format!("local {name} = {call}");
                if self.o.rewrite_last(&call, stmt.clone())
                    || self.o.rewrite_last(&last, stmt)
                {
                    self.r.declared.insert(name);
                }
                self.last_call = None;
            }
            // A scheduled function (`task.spawn`): the wrapper around it is
            // in the log, so its body can be written inside the call that
            // scheduled it, the way a connection's handler is.
            "spawn" => {
                let name = v.get("key").and_then(|k| k.as_str()).unwrap_or("task.spawn");
                let cid = v.get("cid").and_then(|c| c.as_u64()).unwrap_or(u64::MAX);
                let arg = v.get("arg").and_then(|a| a.as_str()).unwrap_or("");
                let open = if arg.is_empty() {
                    format!("{name}(function(...)")
                } else {
                    format!("{name}({arg}, function(...)")
                };
                let spans = self.spans.get(&cid).cloned();
                let nested = spans
                    .as_ref()
                    .filter(|s| !s.is_empty() && s.iter().all(|x| x.clean));
                let Some(spans) = nested else {
                    // A body the log ended inside is still running: say so
                    // rather than write a function with nothing in it. Its
                    // statements are in the log, in the order they ran.
                    let still_open = spans
                        .as_ref()
                        .is_some_and(|s| s.iter().any(|x| x.open));
                    if still_open {
                        self.o.line_always(open)?;
                        self.o.indent += 1;
                        self.o.line_always(
                            "-- still running when the log ended",
                        )?;
                        self.o.indent -= 1;
                        self.o.line_always("end)")?;
                    } else {
                        self.o.line_always(format!("{open} end)"))?;
                    }
                    return Ok(());
                };
                self.o.line_always(open)?;
                self.o.indent += 1;
                let outer = std::mem::replace(&mut self.o.context, cid);
                for s in spans {
                    self.consumed.insert(s.start);
                    self.consumed.insert(s.end);
                    for j in s.start + 1..s.end {
                        if self.consumed.contains(&j) {
                            continue;
                        }
                        self.consumed.insert(j);
                        self.run(j)?;
                    }
                }
                self.o.context = outer;
                self.o.indent -= 1;
                self.o.line_always("end)")?;
            }
            // A connection the script made. A handler that ran with nothing
            // else in between prints inside the closure
            // (`Part.Touched:Connect(function(hit) ... end)`).
            "connect" => {
                self.stats.connects += 1;
                let who = self.r.subject(id, path);
                emit_decls(&mut self.o, &mut self.r)?;
                let once = v.get("once").and_then(|o| o.as_bool()).unwrap_or(false);
                let verb = if once { "Once" } else { "Connect" };
                // The callback's parameters are on the script's own line.
                let params = self
                    .source
                    .and_then(|s| s.params(src))
                    .unwrap_or_else(|| "...".to_string());
                let open = format!("{}:{verb}(function({params})", member(&who, key));
                let cid = v.get("cid").and_then(|c| c.as_u64()).unwrap_or(u64::MAX);
                let spans = self
                    .spans
                    .get(&cid)
                    .filter(|s| !s.is_empty() && s.iter().all(|x| x.clean))
                    .cloned();
                let Some(spans) = spans else {
                    self.o.line_always(format!("{open} end)"))?;
                    return Ok(());
                };
                self.o.line_always(open)?;
                self.o.indent += 1;
                // Two handlers may run the same statement: each body keeps
                // its own copy, so the count is per connection.
                let outer = std::mem::replace(&mut self.o.context, cid);
                for s in &spans {
                    self.consumed.insert(s.start);
                    self.consumed.insert(s.end);
                    for j in s.start + 1..s.end {
                        if self.consumed.contains(&j) {
                            continue;
                        }
                        self.consumed.insert(j);
                        self.run(j)?;
                    }
                }
                self.o.context = outer;
                self.o.indent -= 1;
                self.o.line_always("end)")?;
            }
            "wait" => {
                self.stats.waits += 1;
                let who = self.r.subject(id, path);
                emit_decls(&mut self.o, &mut self.r)?;
                self.o.line(format!("{}:Wait()", member(&who, key)))?;
            }
            "get" => {
                self.stats.gets += 1;
                // A signal's read is the line above its `:Connect`/`:Wait`,
                // so it would only repeat that line.
                if value == "<RBXScriptSignal>" {
                    return Ok(());
                }
                // A method's read is the call that follows it.
                if value == "<function>" {
                    return Ok(());
                }
                // The first read of a member at a given value, with that
                // value: a property that changes reads again.
                if !id.is_empty()
                    && self
                        .seen_gets
                        .insert((id.to_string(), key.to_string(), value.to_string()))
                {
                    let who = self.r.subject(id, path);
                    // Resolve the value first: naming the instance in it can
                    // declare a local, which has to print above this comment.
                    let shown = instances_in(value, &self.r);
                    emit_decls(&mut self.o, &mut self.r)?;
                    let line = if value.is_empty() {
                        format!("-- {}", member(&who, key))
                    } else {
                        format!("-- {} = {}", member(&who, key), shown)
                    };
                    self.o.line(line)?;
                }
            }
            "global_get" => {
                self.stats.globals += 1;
                if self.seen_globals.insert(key.to_string()) {
                    // The comment belongs on the line the read happened on;
                    // a read on another line starts its own group.
                    if !self.reads.is_empty() && self.reads_src != src {
                        self.flush_reads()?;
                    }
                    if self.reads.is_empty() {
                        self.reads_src = src.to_string();
                    }
                    self.reads.push(key.to_string());
                }
            }
            "global_set" => {
                self.stats.globals += 1;
                let rhs = if value == "<function>" {
                    "function(...)"
                } else {
                    value
                };
                self.o.line(format!("{key} = {rhs}"))?;
            }
            // The script's own print/warn: a real call in the replay, as the
            // log recorded the arguments it ran with.
            "output" => {
                let level = v.get("level").and_then(|l| l.as_str()).unwrap_or("info");
                match level {
                    "print" => self.o.line(format!("print({value})"))?,
                    "warn" => self.o.line(format!("warn({value})"))?,
                    _ => self.o.line(format!("-- [[{level}]] {value}"))?,
                }
            }
            "error" => self.o.line(format!("-- [[ error ]] {value}"))?,
            // The source a `loadstring` ran: the next stage as code, written
            // under the call that loaded it.
            "loadsrc" => {
                let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("loadstring");
                let call = v.get("kind").and_then(|k| k.as_str()).unwrap_or("loadstring");
                let text = logged_string(value);
                if is_binary_chunk(&text) {
                    self.o.line(format!(
                        "-- {call}({}) ran a compiled chunk, {} byte(s)",
                        lua_string(name),
                        text.len()
                    ))?;
                } else {
                    let lines = text.lines().count();
                    self.o.line(format!(
                        "-- {call}({}) ran {lines} line(s)",
                        lua_string(name)
                    ))?;
                    if self.seen_bodies.insert(format!("text\0{text}")) {
                        for l in text.lines() {
                            if l.is_empty() {
                                self.o.raw("--")?;
                            } else {
                                self.o.raw(format!("-- {l}"))?;
                            }
                        }
                    }
                }
            }
            // A fetch: the URL, the answer, and the body when it is code (a
            // stage loader's payload), so the next stage is in the replay
            // even if it never runs.
            "http" => {
                let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("GET");
                let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("\"\"");
                let status = v.get("status").and_then(|s| s.as_i64()).unwrap_or(0);
                let text = logged_string(value);
                self.o.line(format!("-- {method} {url} -> {status}"))?;
                if looks_like_lua(&text) && self.seen_bodies.insert(format!("text\0{text}")) {
                    let total = text.lines().count();
                    for (i, l) in text.lines().enumerate() {
                        if i == 200 {
                            self.o.raw(format!("-- ... {} more line(s)", total - 200))?;
                            break;
                        }
                        if l.is_empty() {
                            self.o.raw("--")?;
                        } else {
                            self.o.raw(format!("-- {l}"))?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn write(jsonl: &Path, out: &Path, name: &str, source: Option<&Source>) -> std::io::Result<Stats> {
    let input = File::open(jsonl)?;
    let events: Vec<serde_json::Value> = BufReader::new(input)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect();
    let mut o = Out::new();
    o.raw(format!("-- aimers replay of {name}"))?;
    o.raw("-- the script's statements, each written once; ×N means it ran N times")?;
    o.raw("")?;

    let mut render = Render {
        o,
        r: Resolver::new(),
        stats: Stats::default(),
        seen_globals: HashSet::new(),
        seen_gets: HashSet::new(),
        last_args: HashMap::new(),
        last_call: None,
        if_head: None,
        reads: Vec::new(),
        reads_src: String::new(),
        seen_bodies: HashSet::new(),
        current_src: String::new(),
        source,
        events: &events,
        spans: callback_spans(&events),
        consumed: HashSet::new(),
    };
    let n = render.events.len();
    for i in 0..n {
        if render.consumed.contains(&i) {
            continue;
        }
        render.run(i)?;
    }

    render.flush_reads()?;
    render.flush_head_end()?;
    let stats = render.stats;
    let mut o = render.o;
    o.discard_heading();
    o.close_guards(usize::MAX)?;
    o.raw("")?;
    o.raw(format!(
        "-- {} instance(s), {} write(s), {} call(s), {} connection(s), {} wait(s), {} global read(s); {} gets in the JSONL",
        stats.instances, stats.sets, stats.calls, stats.connects, stats.waits, stats.globals, stats.gets
    ))?;
    let text = o.finish();
    std::fs::write(out, text)?;
    Ok(stats)
}

/// A member read or written as an expression (`Part.Name`, `Part["End"]`).
fn member(who: &str, key: &str) -> String {
    if is_ident(key) {
        format!("{who}.{key}")
    } else {
        format!("{who}[{}]", lua_string(key))
    }
}

/// The single string argument of a logged call (`["Players"]` -> `Players`).
fn arg_string(args: &str) -> Option<String> {
    let v = serde_json::from_str::<serde_json::Value>(&format!("[{args}]")).ok()?;
    let list = v.as_array()?;
    if list.len() != 1 {
        return None;
    }
    list[0].as_str().map(str::to_string)
}

/// A logged string event stores the text JSON-encoded (`"a\nb"`), so the
/// replay gets the text back rather than its quotes.
fn logged_string(value: &str) -> String {
    serde_json::from_str::<String>(value).unwrap_or_else(|_| value.trim_matches('"').to_string())
}

/// A compiled chunk rather than source (`loadstring` accepts bytecode): the
/// Luau header, `\x1bLua`.
fn is_binary_chunk(text: &str) -> bool {
    text.as_bytes().starts_with(b"\x1bLua")
}

/// A fetched body worth reading as code: printable, and shaped like Lua, so
/// a stage loader's `HttpGet` shows the next stage instead of an image.
fn looks_like_lua(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let sample: String = text.chars().take(4096).collect();
    if sample
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
    {
        return false;
    }
    let lower = sample.to_ascii_lowercase();
    ["function", "local ", "return", "loadstring", "game:"]
        .iter()
        .any(|k| lower.contains(k))
}

/// The name a returned instance can be bound to: the last segment of its
/// path (`Workspace.Baseplate` -> `Baseplate`), when it is an identifier.
fn result_name(path: &str) -> Option<String> {
    let last = path.rsplit('.').next()?;
    if last.is_empty() || last == "Instance" || !is_ident(last) {
        return None;
    }
    Some(last.to_string())
}

/// Call arguments with instance reprs as expressions: a logged
/// `[<Instance Workspace.Player>]` reads `workspace.Player`.
fn instances_in(args: &str, r: &Resolver) -> String {
    let mut out = String::new();
    let mut rest = args;
    while let Some(i) = rest.find("<Instance ") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + "<Instance ".len()..];
        let Some(j) = tail.find('>') else {
            out.push_str(&rest[i..]);
            return out;
        };
        out.push_str(&path_expr(&tail[..j], &r.services));
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    out
}

/// The named locals a subject resolution asked for, before its statement.
fn emit_decls(o: &mut Out, r: &mut Resolver) -> std::io::Result<()> {
    for d in r.take_decls() {
        o.code(d)?;
    }
    Ok(())
}
