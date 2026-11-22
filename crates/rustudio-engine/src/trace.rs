//! aimers: the environment log.
//!
//! When a trace is running (the `aimers` CLI), the hooks installed across
//! the runtime write one JSON object per line for what the script touches:
//! global reads and writes, instance member reads and writes, the methods it
//! calls, the instances it makes and the errors it raises. Each line carries
//! the script line that caused it (`src`), so the log reads back as the
//! script's own statements in order.
//!
//! Off by default: every hook is a single atomic load when no trace is
//! running, so Studio and the player are unaffected.

use mlua::{Lua, MultiValue, Value};
use rbx_dom_weak::types::Ref;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

struct Sink {
    out: BufWriter<File>,
    start: Instant,
}

static ON: AtomicBool = AtomicBool::new(false);
static LIMIT: AtomicU64 = AtomicU64::new(1_000_000);
static WRITTEN: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static SINK: Mutex<Option<Sink>> = Mutex::new(None);
/// The target script's full DataModel name. Frames from it (and from
/// runtime-compiled chunks, whose names start with `[`) are logged; other
/// scripts — core scripts, plugins, the engine's own — are not. `None` logs
/// everything.
static ONLY: Mutex<Option<String>> = Mutex::new(None);
/// Scripts the harness loaded itself (`loadstring`, module sources): their
/// chunks are the target's, so they are logged too.
static ALLOW: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The signals the target script connected to, one entry per instance and
/// event. `aimers --fire` fires these so the handler bodies run and land in
/// the log, instead of the replay ending at `:Connect(function() ... end)`.
static CONNS: Mutex<Vec<(Ref, String)>> = Mutex::new(Vec::new());
/// Ids for spawned bodies (`task.spawn`) start above any connection id, so
/// the replay's span map never confuses the two.
static SPAWNS: AtomicU64 = AtomicU64::new(1 << 32);

/// Logs frames from `name` as well as the target script's (the executor's
/// `loadstring` calls this for each chunk it compiles).
pub fn allow_script(name: &str) {
    if !on() {
        return;
    }
    let name = name.trim_start_matches(['=', '@']).trim();
    if name.is_empty() {
        return;
    }
    let mut allow = ALLOW.lock().unwrap_or_else(|e| e.into_inner());
    if !allow.iter().any(|n| n == name) {
        allow.push(name.to_string());
    }
}

/// Starts writing the log at `path`. `limit` bounds the number of lines;
/// `only` is the target script's full DataModel name (`None`: every script).
pub fn start(path: &Path, limit: u64, only: Option<&str>) -> std::io::Result<()> {
    let file = File::create(path)?;
    LIMIT.store(limit, Ordering::Relaxed);
    WRITTEN.store(0, Ordering::Relaxed);
    DROPPED.store(0, Ordering::Relaxed);
    *ONLY.lock().unwrap_or_else(|e| e.into_inner()) = only.map(str::to_string);
    CALLBACKS.with(|c| c.borrow_mut().clear());
    CONNS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    MADE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    ECHOES.lock().unwrap_or_else(|e| e.into_inner()).clear();
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink {
        out: BufWriter::with_capacity(1 << 20, file),
        start: Instant::now(),
    });
    ON.store(true, Ordering::Release);
    Ok(())
}

/// Stops the trace and returns `(lines written, lines dropped past the limit)`.
pub fn stop() -> (u64, u64) {
    ON.store(false, Ordering::Release);
    if let Some(sink) = SINK.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = sink.out.flush();
    }
    (
        WRITTEN.load(Ordering::Relaxed),
        DROPPED.load(Ordering::Relaxed),
    )
}

#[inline]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// The script part of a `src` (`Foo.Bar:12` -> `Foo.Bar`).
fn script_of(src: &str) -> &str {
    match src.rsplit_once(':') {
        Some((s, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => s,
        _ => src,
    }
}

fn wanted(src: &str) -> bool {
    let only = ONLY.lock().unwrap_or_else(|e| e.into_inner());
    match only.as_deref() {
        None => true,
        Some(filter) => {
            let script = script_of(src).trim();
            let filter = filter.trim();
            if script.is_empty() {
                return false;
            }
            if script == filter || script.starts_with('[') {
                return true;
            }
            ALLOW
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|n| n == script)
        }
    }
}

fn at(src: String, line: Option<usize>) -> String {
    match line {
        Some(l) => format!("{src}:{l}"),
        None => src,
    }
}

/// The script frame that made the current engine call, as `file:line`.
/// Empty when that frame belongs to the engine's own Lua
/// (`rustudio.prelude`): the work a helper does for the script is the
/// helper's, not a statement the script wrote (the prelude's `WaitForChild`
/// calls `FindFirstChild`, and `require` checks `IsA`).
fn origin(lua: &Lua) -> String {
    for level in 1..8 {
        let got = lua.inspect_stack(level, |d| {
            let src = d
                .source()
                .short_src
                .map(|s| s.trim_start_matches(['=', '@']).to_string())
                .unwrap_or_default();
            (src, d.current_line())
        });
        if let Some((src, line)) = got {
            if src.is_empty() || src == "[C]" {
                continue;
            }
            if src.starts_with("rustudio.") {
                return String::new();
            }
            return at(src, line);
        }
    }
    String::new()
}

/// `origin` that skips the engine's own frames too: the script line behind a
/// helper the script called, for a result the script ends up holding (the
/// child `WaitForChild` found).
fn nearest_script(lua: &Lua) -> String {
    for level in 1..8 {
        let got = lua.inspect_stack(level, |d| {
            let src = d
                .source()
                .short_src
                .map(|s| s.trim_start_matches(['=', '@']).to_string())
                .unwrap_or_default();
            (src, d.current_line())
        });
        if let Some((src, line)) = got {
            if !src.is_empty() && src != "[C]" && !src.starts_with("rustudio.") && !src.starts_with("aimers.") {
                return at(src, line);
            }
        }
    }
    String::new()
}

/// A stable short name for an instance inside one run: the low bits of its
/// `Ref`, so two unnamed `Instance.new("Part")`s stay apart.
fn id_of(r: Ref) -> String {
    if !r.is_some() {
        return String::new();
    }
    let full = r.to_string();
    full[full.len().saturating_sub(12)..].to_string()
}

fn quote(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

pub fn value_repr(lua: &Lua, v: &Value, depth: u8) -> String {
    match v {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => {
            if n.fract() == 0.0 {
                format!("{n:.0}")
            } else {
                format!("{n}")
            }
        }
        Value::String(s) => {
            let text = s.to_string_lossy();
            let text: String = text.chars().take(400).collect();
            quote(&text)
        }
        Value::Table(t) => {
            if depth >= 2 {
                return "{...}".into();
            }
            let mut parts = Vec::new();
            let mut n = 0;
            for pair in t.clone().pairs::<Value, Value>().flatten() {
                if n == 10 {
                    parts.push("...".into());
                    break;
                }
                parts.push(format!(
                    "[{}]={}",
                    value_repr(lua, &pair.0, depth + 1),
                    value_repr(lua, &pair.1, depth + 1)
                ));
                n += 1;
            }
            format!("{{{}}}", parts.join(","))
        }
        Value::Function(_) => "<function>".into(),
        Value::Thread(_) => "<thread>".into(),
        Value::UserData(u) => {
            if let Ok(i) = u.borrow::<crate::runtime::LInst>() {
                let name = instance_path(lua, i.0);
                format!("<Instance {name}>")
            } else if let Some(text) = datatype_repr(u) {
                text
            } else {
                match u.type_name() {
                    Ok(name) => format!("<{}>", name.to_string_lossy()),
                    Err(_) => "<userdata>".into(),
                }
            }
        }
        Value::Buffer(b) => format!("<buffer {}>", b.len()),
        other => format!("<{}>", other.type_name()),
    }
}

/// A Roblox datatype as the constructor that makes it again
/// (`Enum.Font.Gotham`, `UDim2.new(0, 0, 1, 0)`), so the log says what the
/// script set instead of just `<UDim2>`.
fn datatype_repr(u: &mlua::AnyUserData) -> Option<String> {
    use crate::ltypes::{
        g6, LBrickColor, LCFrame, LColor3, LEnumItem, LNumberRange, LRay, LUDim2, LVector2,
        LVector3,
    };
    let vec3 = |v: &glam::Vec3| format!("Vector3.new({}, {}, {})", g6(v.x), g6(v.y), g6(v.z));
    if let Ok(e) = u.borrow::<LEnumItem>() {
        if e.enum_type.is_empty() || e.name.is_empty() {
            return None;
        }
        return Some(format!("Enum.{}.{}", e.enum_type, e.name));
    }
    if let Ok(v) = u.borrow::<LVector2>() {
        return Some(format!("Vector2.new({}, {})", g6(v.0.x), g6(v.0.y)));
    }
    if let Ok(v) = u.borrow::<LVector3>() {
        return Some(vec3(&v.0));
    }
    if let Ok(c) = u.borrow::<LColor3>() {
        return Some(format!(
            "Color3.new({}, {}, {})",
            g6(c.0[0]),
            g6(c.0[1]),
            g6(c.0[2])
        ));
    }
    if let Ok(d) = u.borrow::<crate::ltypes::LUDim>() {
        return Some(format!("UDim.new({}, {})", g6(d.scale), d.offset));
    }
    if let Ok(d) = u.borrow::<LUDim2>() {
        return Some(format!(
            "UDim2.new({}, {}, {}, {})",
            g6(d.x.scale),
            d.x.offset,
            g6(d.y.scale),
            d.y.offset
        ));
    }
    if let Ok(c) = u.borrow::<LCFrame>() {
        let x = &c.0;
        if x.rot.abs_diff_eq(glam::Mat3::IDENTITY, 1e-5) {
            return Some(format!(
                "CFrame.new({}, {}, {})",
                g6(x.pos.x),
                g6(x.pos.y),
                g6(x.pos.z)
            ));
        }
        return Some(format!(
            "CFrame.fromMatrix({}, {}, {}, {})",
            vec3(&x.pos),
            vec3(&x.rot.x_axis),
            vec3(&x.rot.y_axis),
            vec3(&x.rot.z_axis)
        ));
    }
    if let Ok(r) = u.borrow::<LRay>() {
        return Some(format!(
            "Ray.new({}, {})",
            vec3(&r.origin),
            vec3(&r.direction)
        ));
    }
    if let Ok(n) = u.borrow::<LNumberRange>() {
        return Some(format!("NumberRange.new({}, {})", g6(n.min), g6(n.max)));
    }
    if let Ok(b) = u.borrow::<LBrickColor>() {
        return Some(format!("BrickColor.new({})", quote(&b.0.to_string())));
    }
    None
}

/// The full name of an instance (`Workspace.Part`), or `Instance` when the
/// DataModel isn't lendable right now.
pub fn instance_path(lua: &Lua, r: Ref) -> String {
    if !r.is_some() {
        return String::new();
    }
    match crate::runtime::world(lua) {
        Ok(w) => w.full_name(r),
        Err(_) => "Instance".to_string(),
    }
}

fn write(lua: &Lua, mut event: serde_json::Map<String, serde_json::Value>) {
    let mut src = event
        .get("src")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if src.is_empty() {
        src = origin(lua);
    }
    // Errors are always kept: a syntax error has no frame to filter on.
    let is_error = event.get("ev").and_then(|e| e.as_str()) == Some("error");
    // A callback's start and end are not script events, and the thread that
    // fired the signal may belong to an untraced script: they always land.
    let is_cb = event.get("ev").and_then(|e| e.as_str()) == Some("cb");
    if !is_cb {
        if !src.is_empty() {
            if !wanted(&src) {
                return;
            }
            event
                .entry("src")
                .or_insert_with(|| serde_json::Value::String(src));
        } else if !is_error {
            return;
        }
    }
    // The thread that ran this: a connection's handler runs on its own, and
    // the replay uses that to nest a callback's statements in its `Connect`.
    event
        .entry("tid")
        .or_insert_with(|| serde_json::Value::String(short(lua.current_thread().to_pointer() as usize)));
    if WRITTEN.load(Ordering::Relaxed) >= LIMIT.load(Ordering::Relaxed) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut guard = SINK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sink) = guard.as_mut() else { return };
    let t = sink.start.elapsed().as_secs_f64();
    event.insert(
        "t".into(),
        serde_json::Value::from((t * 1000.0).round() / 1000.0),
    );
    if serde_json::to_writer(&mut sink.out, &serde_json::Value::Object(event)).is_ok() {
        let _ = sink.out.write_all(b"\n");
        WRITTEN.fetch_add(1, Ordering::Relaxed);
    }
}

fn subject(lua: &Lua, r: Ref) -> (String, String) {
    (id_of(r), instance_path(lua, r))
}

pub fn global_get(lua: &Lua, name: &str, value: &Value) {
    if !on() {
        return;
    }
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "global_get".into());
    map.insert("key".into(), name.into());
    map.insert("value".into(), value_repr(lua, value, 0).into());
    write(lua, map);
}

pub fn global_set(lua: &Lua, name: &str, value: &Value) {
    if !on() {
        return;
    }
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "global_set".into());
    map.insert("key".into(), name.into());
    map.insert("value".into(), value_repr(lua, value, 0).into());
    write(lua, map);
}

/// A member read with the value it produced (`part.Position` → the vector).
/// The fast index path answers with an `Fv` it may never turn into a Lua
/// value, so the value is formatted here.
pub fn get_value(lua: &Lua, r: Ref, key: &str, v: &crate::runtime::Fv) {
    if !on() {
        return;
    }
    let (id, path) = subject(lua, r);
    let value = match v {
        crate::runtime::Fv::V(x) => value_repr(lua, x, 0),
        crate::runtime::Fv::Inst(i) if i.is_some() => {
            format!("<Instance {}>", instance_path(lua, *i))
        }
        crate::runtime::Fv::Inst(_) => "nil".into(),
        crate::runtime::Fv::Insts(xs) => format!("<{} instances>", xs.len()),
        crate::runtime::Fv::Str(s) => quote(s),
        crate::runtime::Fv::Static(s) => quote(s),
        crate::runtime::Fv::CFrame(_) => "<CFrame>".into(),
    };
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "get".into());
    map.insert("id".into(), id.into());
    map.insert("path".into(), path.into());
    map.insert("key".into(), key.into());
    map.insert("value".into(), value.into());
    write(lua, map);
}

pub fn set(lua: &Lua, r: Ref, key: &str, value: &Value) {
    if !on() {
        return;
    }
    let (id, path) = subject(lua, r);
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "set".into());
    map.insert("id".into(), id.into());
    map.insert("path".into(), path.into());
    map.insert("key".into(), key.into());
    map.insert("value".into(), value_repr(lua, value, 0).into());
    // An instance on the right (`p.Parent = workspace`): log which one, so
    // the replay can write the variable it bound.
    if let Value::UserData(u) = value {
        if let Ok(inst) = u.borrow::<crate::runtime::LInst>() {
            let (vid, vpath) = subject(lua, inst.0);
            map.insert("vid".into(), vid.into());
            map.insert("vpath".into(), vpath.into());
        }
    }
    write(lua, map);
}

pub fn call(lua: &Lua, r: Ref, method: &str, args: &MultiValue) {
    if !on() {
        return;
    }
    let (id, path) = subject(lua, r);
    let mut parts = Vec::new();
    for v in args.iter().skip(1) {
        parts.push(value_repr(lua, v, 0));
    }
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "call".into());
    map.insert("id".into(), id.into());
    map.insert("path".into(), path.into());
    map.insert("key".into(), method.into());
    map.insert("args".into(), format!("[{}]", parts.join(",")).into());
    write(lua, map);
}

/// A method that returned an instance (`GetService`, `FindFirstChild`,
/// `Clone`, ...): the value is the instance, for the replay's bindings.
pub fn call_result(lua: &Lua, this: Ref, method: &str, result: &MultiValue) {
    if !on() {
        return;
    }
    // `namecall` answers in the dispatcher's form: (true, results…).
    let mut values = result.iter();
    match values.next() {
        Some(Value::Boolean(true)) => {}
        _ => return,
    }
    let Some(first) = values.next() else {
        return;
    };
    if let Value::UserData(u) = &first {
        if let Ok(inst) = u.borrow::<crate::runtime::LInst>() {
            result_instance(lua, this, method, inst.0);
        }
    } else if !matches!(first, Value::Nil) {
        // A value of another type: the replay can still bind it to the
        // variable the call's line assigns (`local text = svc:JSONEncode(t)`).
        result_value(lua, this, method, &first);
    }
}

/// `call_result` for a result that is not an instance.
fn result_value(lua: &Lua, this: Ref, method: &str, v: &Value) {
    let (rid, rpath) = subject(lua, this);
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "ret_value".into());
    map.insert("rid".into(), rid.into());
    map.insert("rpath".into(), rpath.into());
    map.insert("key".into(), method.into());
    map.insert("value".into(), value_repr(lua, v, 0).into());
    write(lua, map);
}

/// `call_result` for a fast path that already has the `Ref`.
pub fn result_instance(lua: &Lua, this: Ref, method: &str, r: Ref) {
    if !on() || !r.is_some() {
        return;
    }
    let (id, path) = subject(lua, r);
    let (rid, rpath) = subject(lua, this);
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "ret".into());
    map.insert("id".into(), id.into());
    map.insert("path".into(), path.into());
    map.insert("rid".into(), rid.into());
    map.insert("rpath".into(), rpath.into());
    map.insert("key".into(), method.into());
    // The script's line behind a helper (`WaitForChild` answers from its
    // own `FindFirstChild`), so the result still binds to the script's
    // variable.
    let src = nearest_script(lua);
    if !src.is_empty() {
        map.insert("src".into(), src.into());
    }
    write(lua, map);
}

pub fn new_instance(lua: &Lua, class: &str, r: Ref) {
    if !on() {
        return;
    }
    let (id, path) = subject(lua, r);
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "new".into());
    map.insert("id".into(), id.into());
    map.insert("class".into(), class.into());
    map.insert("path".into(), path.into());
    // `Instance.new` runs behind a capability gate (`rustudio.assets_cloud.gates`),
    // so the engine's own frame is on top: the statement that made the
    // instance is the nearest script frame. A core script's instances land on
    // the core script's name and the target filter drops them.
    let src = caller_src(lua);
    if !src.is_empty() {
        map.insert("src".into(), src.into());
    }
    write(lua, map);
}

pub fn error(lua: &Lua, message: &str) {
    if !on() {
        return;
    }
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "error".into());
    map.insert("value".into(), quote(message).into());
    if let Some((path, line)) = crate::runtime::error_origin(message) {
        map.insert("src".into(), format!("{path}:{line}").into());
    }
    write(lua, map);
}

pub fn output(lua: &Lua, level: &str, text: &str, src: &str) {
    if !on() {
        return;
    }
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "output".into());
    map.insert("level".into(), level.into());
    map.insert("value".into(), quote(text).into());
    let src = src.trim_start_matches(['=', '@']);
    if !src.is_empty() && !src.starts_with("rustudio.") {
        map.insert("src".into(), src.into());
    }
    write(lua, map);
}

/// The source a `loadstring` handed to the VM: a stage loader carries its
/// next stage in a string, and the replay shows that code instead of only
/// what running it touched. `name` is the chunk name it was compiled under.
pub fn load_source(lua: &Lua, src: &str, name: &str) {
    source(lua, src, name, "loadstring");
}

/// The source a `require` compiled. A module's body runs as part of the
/// requiring script, so the replay writes its code where the `require` was.
pub fn load_module(lua: &Lua, src: &str, name: &str) {
    source(lua, src, name, "require");
}

fn source(lua: &Lua, src: &str, name: &str, kind: &str) {
    if !on() {
        return;
    }
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "loadsrc".into());
    map.insert("kind".into(), kind.into());
    map.insert("name".into(), name.into());
    let text: String = src.chars().take(200_000).collect();
    map.insert("value".into(), quote(&text).into());
    // The executor's own `loadstring` wrapper is between the script and the
    // compile, so the caller is the nearest script frame, not the top one.
    let src_line = nearest_script(lua);
    if !src_line.is_empty() {
        map.insert("src".into(), src_line.into());
    }
    write(lua, map);
}

/// An HTTP request a script made (`HttpGet`, `HttpPost`, `request`): the
/// URL, the status that came back and the body, so a loader's next stage is
/// in the log even when it never runs.
pub fn http(lua: &Lua, method: &str, url: &str, status: i64, body: &str) {
    if !on() {
        return;
    }
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "http".into());
    map.insert("method".into(), method.into());
    map.insert("url".into(), quote(url).into());
    map.insert("status".into(), status.into());
    let text: String = body.chars().take(200_000).collect();
    map.insert("value".into(), quote(&text).into());
    let src_line = caller_src(lua);
    if !src_line.is_empty() {
        map.insert("src".into(), src_line.into());
    }
    write(lua, map);
}

/// A `task.spawn`/`task.defer`/`task.delay`/`spawn`/`delay` call: the replay
/// writes it as the wrapper it is (`task.spawn(function(...) ... end)`), and
/// the body's statements nest inside. Answers the id its `cb` events carry.
pub fn spawn(lua: &Lua, name: &str, arg: Option<Value>) -> u64 {
    let cid = SPAWNS.fetch_add(1, Ordering::Relaxed);
    if !on() {
        return cid;
    }
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "spawn".into());
    map.insert("key".into(), name.into());
    map.insert("cid".into(), cid.into());
    if let Some(v) = arg {
        map.insert("arg".into(), value_repr(lua, &v, 0).into());
    }
    // The executor's wrapper is between the script and this call.
    let src = nearest_script(lua);
    if !src.is_empty() {
        map.insert("src".into(), src.into());
    }
    write(lua, map);
    cid
}

/// The start or end of a spawned function's body, on the thread it runs on
/// (the executor's wrapper calls this around the script's function).
pub fn spawn_phase(lua: &Lua, cid: u64, phase: &str) {
    if !on() {
        return;
    }
    cb_event(lua, phase, cid, lua.current_thread().to_pointer() as usize);
}

/// The script line behind an engine entry point: the caller's line when the
/// frame is the script's own, or the nearest script frame when the engine's
/// own Lua (`Signal:Connect` -> `connect` -> `rs_connect`) is in between.
fn caller_src(lua: &Lua) -> String {
    let src = origin(lua);
    if src.is_empty() {
        nearest_script(lua)
    } else {
        src
    }
}

/// `signal:Connect(fn)` / `signal:Once(fn)`: the replay writes the
/// connection where it happened (`Part.Touched:Connect(function(...) end)`).
/// `cid` is the connection's id, which its handler's `cb` events carry.
pub fn connect(lua: &Lua, r: Ref, signal: &str, once: bool, cid: u64) {
    if !on() {
        return;
    }
    // A connection the target made: `--fire` re-raises it later. Only the
    // target's own connections count, not the engine's or a core script's.
    let src = caller_src(lua);
    if !src.is_empty() && wanted(&src) {
        let mut conns = CONNS.lock().unwrap_or_else(|e| e.into_inner());
        if !conns.iter().any(|(cr, cn)| *cr == r && cn == signal) {
            conns.push((r, signal.to_string()));
        }
    }
    let (id, path) = subject(lua, r);
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "connect".into());
    map.insert("id".into(), id.into());
    map.insert("path".into(), path.into());
    map.insert("key".into(), signal.into());
    map.insert("once".into(), once.into());
    map.insert("cid".into(), cid.into());
    if !src.is_empty() {
        map.insert("src".into(), src.into());
    }
    write(lua, map);
}

/// The signals the target script has connected to so far, in connection
/// order and deduplicated by (instance, event).
pub fn traced_connections() -> Vec<(Ref, String)> {
    CONNS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `--satisfy <Class>`: when the target asks for a child that isn't there,
/// the harness makes one, so a script waiting on a server-created remote
/// keeps running headlessly.
static SATISFY: Mutex<Option<String>> = Mutex::new(None);

pub fn satisfy_with(class: &str) {
    *SATISFY.lock().unwrap_or_else(|e| e.into_inner()) = Some(class.to_string());
}

/// Instances the harness made (so a `require` of one can turn it into a
/// ModuleScript with stub code, and a check at a remote can leave it alone).
static MADE: Mutex<Vec<Ref>> = Mutex::new(Vec::new());

/// `--echo-remotes`: a `FireServer` on a made remote is delivered back as
/// `OnClientEvent` with the same arguments, the answer a server would give,
/// so the client's reply handlers run.
static ECHO: AtomicBool = AtomicBool::new(false);

/// How many times each made remote has been echoed (a handler that fires
/// back would otherwise echo itself forever).
static ECHOES: Mutex<Vec<(Ref, u32)>> = Mutex::new(Vec::new());

pub fn echo_remotes(on: bool) {
    ECHO.store(on, Ordering::Relaxed);
}

#[inline]
pub fn echoing() -> bool {
    ECHO.load(Ordering::Relaxed)
}

pub fn made(r: Ref) -> bool {
    MADE.lock().unwrap_or_else(|e| e.into_inner()).contains(&r)
}

/// Whether this fire of `r` should be echoed back (at most 3 per remote).
pub fn may_echo(r: Ref) -> bool {
    let mut echoes = ECHOES.lock().unwrap_or_else(|e| e.into_inner());
    match echoes.iter_mut().find(|(x, _)| *x == r) {
        Some((_, n)) if *n >= 3 => false,
        Some((_, n)) => {
            *n += 1;
            true
        }
        None => {
            echoes.push((r, 1));
            true
        }
    }
}

/// What a made ModuleScript returns: a table that answers every index with
/// itself, so a module the real server would have supplied (`Cryptography`,
/// a config table) lets the script keep running instead of erroring out.
const STUB_MODULE: &str = r#"local function proxy()
    local t = {}
    return setmetatable(t, {
        __index = function() return proxy() end,
        __call = function() return proxy() end,
        __tostring = function() return "" end,
        __concat = function() return "" end,
        __add = function() return 0 end,
        __sub = function() return 0 end,
        __mul = function() return 0 end,
        __div = function() return 0 end,
        __mod = function() return 0 end,
        __pow = function() return 0 end,
        __unm = function() return 0 end,
        __len = function() return 0 end,
        __eq = function() return false end,
        __lt = function() return false end,
        __le = function() return false end,
    })
end
return proxy()"#;

/// The class `--satisfy` creates, when it is on.
pub fn satisfying() -> Option<String> {
    SATISFY.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Runs before the method does: if the target scripts asks `this` for a
/// child that doesn't exist and `--satisfy` names a class, the child is
/// made now, so this very call (or its next poll) finds it. One log event
/// records what the harness made, so the replay can say so.
pub fn satisfy(lua: &Lua, this: Ref, method: &str, args: &MultiValue) {
    if !on() {
        return;
    }
    let Some(Value::String(name)) = args.get(1) else {
        return;
    };
    let Ok(name) = name.to_str() else { return };
    let name = name.to_string();
    satisfy_missing(lua, this, method, &name);
}

/// `satisfy` for the native fast path, which has the argument as a `&str`.
pub fn satisfy_missing(lua: &Lua, this: Ref, method: &str, name: &str) {
    if !on() || name.is_empty() {
        return;
    }
    if !matches!(method, "FindFirstChild" | "WaitForChild") {
        return;
    }
    let src = origin(lua);
    if src.is_empty() || !wanted(&src) {
        return;
    }
    let Some(class) = satisfying() else { return };
    make_child(lua, this, name, &class);
}

/// `satisfy` on a plain member read (`workspace.Map`, `Remotes.Mechanics`). A
/// real place held the child, so make it now rather than fail the script's
/// dot access on its absence. Only PascalCase names: a lowercase miss is a
/// misspelt member, not an instance.
pub fn satisfy_member(lua: &Lua, this: Ref, name: &str) -> bool {
    if !on() || name.is_empty() || !name.starts_with(|c: char| c.is_ascii_uppercase()) {
        return false;
    }
    let src = origin(lua);
    if src.is_empty() || !wanted(&src) {
        return false;
    }
    let Some(class) = satisfying() else { return false };
    make_child(lua, this, name, &class)
}

/// `--satisfy`: a made stub's class is repaired from a property the script
/// writes that the stub's class cannot take. Only a class whose name shares a
/// long prefix with the instance's name is tried, so
/// `Lighting.Obsidian_ColorCorrection` becomes a `ColorCorrectionEffect` on
/// `Enabled = false` while an unrelated property write still fails as it
/// should.
pub fn adopt_property(lua: &Lua, r: Ref, key: &str) -> bool {
    if !on() || !made(r) {
        return false;
    }
    let src = origin(lua);
    if src.is_empty() || !wanted(&src) {
        return false;
    }
    let Some(api) = rustudio_core::api::get() else { return false };
    let (class, name) = {
        let Ok(w) = crate::runtime::world(lua) else {
            return false;
        };
        let Some(i) = w.d().dom.get_by_ref(r) else {
            return false;
        };
        (i.class.to_string(), i.name.clone())
    };
    if api
        .lookup(&class, key)
        .is_some_and(|(_, m)| m.kind == rustudio_core::api::Kind::Property)
    {
        return false;
    }
    let Some(class) = named_class(api, &name, key) else {
        return false;
    };
    to_class(lua, r, &class, "")
}

/// The creatable class with a property `key` whose name shares the longest
/// prefix (at least six letters) with `name`, e.g. `Obsidian_ColorCorrection`
/// → `ColorCorrectionEffect`.
fn named_class(api: &rustudio_core::api::Api, name: &str, key: &str) -> Option<String> {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let inst = norm(name);
    let mut best: Option<(usize, &str)> = None;
    for class in &api.class_order {
        let Some(info) = api.classes.get(class) else { continue };
        if !info.creatable {
            continue;
        }
        let Some((_, member)) = api.lookup(class, key) else {
            continue;
        };
        if member.kind != rustudio_core::api::Kind::Property {
            continue;
        }
        let cls = norm(class);
        let cap = cls.len().min(inst.len());
        let mut score = 0;
        for k in (6..=cap).rev() {
            if inst.contains(&cls[..k]) {
                score = k;
                break;
            }
        }
        if score >= 6 && best.is_none_or(|(b, _)| score > b) {
            best = Some((score, class));
        }
    }
    best.map(|(_, class)| class.to_string())
}

/// Makes `name` under `this` as `class` unless it is already there, and logs
/// the `made` event the replay shows. Answers whether it inserted one.
fn make_child(lua: &Lua, this: Ref, name: &str, class: &str) -> bool {
    let made = {
        let Ok(mut w) = crate::runtime::world(lua) else {
            return false;
        };
        let exists = w
            .d()
            .dom
            .get_by_ref(this)
            .is_some_and(|i| {
                i.children().iter().any(|c| {
                    w.d()
                        .dom
                        .get_by_ref(*c)
                        .is_some_and(|i| i.name == name)
                })
            });
        if exists {
            return false;
        }
        let r = w.dm().dom.insert(
            this,
            rbx_dom_weak::InstanceBuilder::new(class).with_name(name),
        );
        MADE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(r);
        (r, w.full_name(r))
    };
    let (r, path) = made;
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "made".into());
    map.insert("id".into(), id_of(r).into());
    map.insert("path".into(), path.into());
    map.insert("class".into(), class.into());
    write(lua, map);
    true
}

/// A `require` of an instance the harness made: give it ModuleScript code
/// (called before the real `require`, so the class is right in time).
pub fn stub_module(lua: &Lua, v: &Value) -> bool {
    let Value::UserData(u) = v else {
        return false;
    };
    let Ok(inst) = u.borrow::<crate::runtime::LInst>() else {
        return false;
    };
    to_class(lua, inst.0, "ModuleScript", STUB_MODULE)
}

/// `x:IsA("ModuleScript")` on a made instance: make that true.
pub fn isa(lua: &Lua, r: Ref, class: &str) -> bool {
    class == "ModuleScript" && to_class(lua, r, class, STUB_MODULE)
}

/// The class a member implies for an instance the harness made: a remote
/// the script fires is a RemoteEvent, one it invokes a RemoteFunction.
fn implied_class(key: &str) -> Option<&'static str> {
    Some(match key {
        "FireServer" | "FireClient" | "FireAllClients" | "OnClientEvent" | "OnServerEvent" => {
            "RemoteEvent"
        }
        "InvokeServer" | "InvokeClient" | "OnClientInvoke" | "OnServerInvoke" => "RemoteFunction",
        "Fire" | "Event" => "BindableEvent",
        "Invoke" | "OnInvoke" => "BindableFunction",
        _ => return None,
    })
}

/// Before a member read or call on a made instance, give it the class that
/// member belongs to (a script firing `SubmitPlatform:FireServer()` needs a
/// RemoteEvent, not whatever class the lookup happened to give it).
pub fn adopt(lua: &Lua, r: Ref, key: &str) {
    if !on() {
        return;
    }
    let Some(class) = implied_class(key) else { return };
    let src = origin(lua);
    if src.is_empty() || !wanted(&src) {
        return;
    }
    to_class(lua, r, class, "");
}

/// Changes a made instance's class in place (its `Ref` stays valid, so the
/// script's own userdata keeps working). `source` is ModuleScript code.
fn to_class(lua: &Lua, r: Ref, class: &str, source: &str) -> bool {
    if !on()
        || !MADE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&r)
    {
        return false;
    }
    let path = {
        let Ok(mut w) = crate::runtime::world(lua) else {
            return false;
        };
        let Some(i) = w.dm().dom.get_by_ref_mut(r) else {
            return false;
        };
        if i.class == class {
            return false;
        }
        i.class = rbx_dom_weak::ustr(class);
        if !source.is_empty() {
            i.properties.insert(
                rbx_dom_weak::ustr("Source"),
                rbx_dom_weak::types::Variant::String(source.to_string()),
            );
        }
        w.full_name(r)
    };
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "class".into());
    map.insert("id".into(), id_of(r).into());
    map.insert("path".into(), path.into());
    map.insert("class".into(), class.into());
    map.insert("stub".into(), (!source.is_empty()).into());
    write(lua, map);
    true
}

/// The thread a connection's handler will run on, keyed by the connection's
/// id. `resume_enter` writes the `cb` start, so the log has the handler's
/// statements between its start and end.
pub fn callback(th: &mlua::Thread, cid: u64) {
    if !on() {
        return;
    }
    let key = th.to_pointer() as usize;
    CALLBACKS.with(|c| c.borrow_mut().insert(key, Callback { cid, started: false }));
}

struct Callback {
    cid: u64,
    started: bool,
}

thread_local! {
    static CALLBACKS: std::cell::RefCell<std::collections::HashMap<usize, Callback>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn cb_event(lua: &Lua, phase: &str, cid: u64, tid: usize) {
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "cb".into());
    map.insert("phase".into(), phase.into());
    map.insert("cid".into(), cid.into());
    map.insert("tid".into(), short(tid).into());
    write(lua, map);
}

/// A handler thread is about to run: the first resume writes its start.
pub fn resume_enter(lua: &Lua, th: &mlua::Thread) {
    if !on() {
        return;
    }
    let key = th.to_pointer() as usize;
    let cid = CALLBACKS.with(|c| {
        let mut m = c.borrow_mut();
        match m.get_mut(&key) {
            Some(cb) if !cb.started => {
                cb.started = true;
                Some(cb.cid)
            }
            _ => None,
        }
    });
    if let Some(cid) = cid {
        cb_event(lua, "start", cid, key);
    }
}

/// A handler thread finished (it may yield many times first).
pub fn resume_leave(lua: &Lua, th: &mlua::Thread) {
    use mlua::thread::ThreadStatus;
    if !on()
        || !matches!(
            th.status(),
            ThreadStatus::Finished | ThreadStatus::Error
        )
    {
        return;
    }
    let key = th.to_pointer() as usize;
    let cid = CALLBACKS.with(|c| c.borrow_mut().remove(&key).map(|cb| cb.cid));
    if let Some(cid) = cid {
        cb_event(lua, "end", cid, key);
    }
}

fn short(id: usize) -> String {
    format!("{:x}", id & 0xffff_ffff)
}

/// `signal:Wait()`: the script is parked until the signal fires.
pub fn wait_signal(lua: &Lua, r: Ref, signal: &str) {
    if !on() {
        return;
    }
    let (id, path) = subject(lua, r);
    let mut map = serde_json::Map::new();
    map.insert("ev".into(), "wait".into());
    map.insert("id".into(), id.into());
    map.insert("path".into(), path.into());
    map.insert("key".into(), signal.into());
    let src = caller_src(lua);
    if !src.is_empty() {
        map.insert("src".into(), src.into());
    }
    write(lua, map);
}
