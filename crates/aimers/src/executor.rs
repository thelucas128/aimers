//! The executor surface: `aimers` runs scripts the way an exploit executor
//! does, so scripts that expect one (stage loaders, hub scripts) run instead
//! of stopping at `loadstring` or `getgenv`.
//!
//! These globals go into the scripts' public table after the engine has
//! bound its own (globals_parity::sync), so they never change what Studio
//! or the player see. Anything script-visible is here: `loadstring`,
//! `getgenv`/`getrenv`, `hookfunction` (which swaps the references to a
//! function), the file API in `aimers-workspace/`, and `HttpGet`/`request`
//! over `curl`.

use std::io::Write;
use std::path::{Path, PathBuf};

use mlua::{Function, Lua, MultiValue, Table, Value};

const VERSION: &str = "0.1.0";

/// Installs the executor globals into the scripts' public table.
pub fn install(lua: &Lua) -> mlua::Result<()> {
    let public = rustudio_engine::globals_parity::public_env(lua)?;
    rustudio_engine::runtime::security::expose_core_gui(true);
    // game:HttpGet and friends (runtime::index_entry reads this flag).
    rustudio_engine::runtime::executor_mode(true);
    let genv: Table = match lua.named_registry_value::<Option<Table>>("aimers_genv")? {
        Some(t) => t,
        None => {
            let t = lua.create_table()?;
            lua.set_named_registry_value("aimers_genv", t.clone())?;
            t
        }
    };

    // `task.spawn` and friends are plain Lua calls, so nothing in the engine
    // sees them. Their wrappers are C closures with the engine's own names,
    // so a script that checks `iscclosure(task.spawn)` or `debug.info` still
    // reads a real function.
    public.raw_set(
        "__aimers_span",
        lua.create_function(|lua, (cid, phase): (u64, String)| {
            rustudio_engine::trace::spawn_phase(lua, cid, &phase);
            Ok(())
        })?,
    )?;

    let prelude = lua.load(
        r#"
-- A Luau function reads back as a Lua closure; a C function ("[C]") is not.
function islclosure(f)
    return type(f) == "function" and debug.info(f, "s") ~= "[C]"
end
function iscclosure(f)
    return type(f) == "function" and not islclosure(f)
end

-- The function a task wrapper hands the engine: a Lua closure, because its
-- body may yield (task.wait), which a C function could not cross. Its marks
-- tell the replay where the body started and ended.
function __aimers_body(cid, f, ...)
    local args = table.pack(...)
    return function()
        __aimers_span(cid, "start")
        f(table.unpack(args, 1, args.n))
        __aimers_span(cid, "end")
    end
end
"#,
    );
    prelude.set_name("=aimers.executor").set_environment(public.clone()).exec()?;

    install_tasks(lua, &public)?;
    install_debug(lua, &public)?;
    install_loadstring(lua, &public)?;

    let name = lua.create_function(|_, ()| Ok(("aimers", VERSION)))?;
    public.raw_set("identifyexecutor", name)?;
    public.raw_set(
        "getexecutorname",
        lua.create_function(|_, ()| Ok("aimers"))?,
    )?;
    public.raw_set("getexecutorversion", lua.create_function(|_, ()| Ok(VERSION))?)?;
    public.raw_set("getgenv", lua.create_function(move |_, ()| Ok(genv.clone()))?)?;
    public.raw_set("getrenv", lua.create_function(|lua, ()| Ok(lua.globals()))?)?;
    public.raw_set(
        "getreg",
        lua.create_function(|lua, ()| -> mlua::Result<Table> { Ok(lua.globals()) })?,
    )?;

    // Identity checks hub scripts run before deciding anything.
    for f in [
        "is_sirhurt_closure",
        "is_synapse_function",
        "is_protosmasher_closure",
        "is_msec_closure",
        "is_krnl_closure",
        "is_fluxus_closure",
    ] {
        public.raw_set(f, lua.create_function(|_, _: Value| Ok(false))?)?;
    }
    public.raw_set("checkcaller", lua.create_function(|_, ()| Ok(true))?)?;
    // `cloneref` hides the original reference from weak tables and equality
    // checks in a real executor. Here an instance is already a plain handle,
    // so answering the value keeps hub scripts on their fast path.
    let cloneref = lua.create_function(|_, v: Value| Ok(v))?;
    public.raw_set("cloneref", cloneref.clone())?;
    public.raw_set("clonereference", cloneref)?;
    public.raw_set(
        "setthreadidentity",
        lua.create_function(|_, _: Value| Ok(()))?,
    )?;
    public.raw_set(
        "setthreadcontext",
        lua.create_function(|_, _: Value| Ok(()))?,
    )?;
    // `gethui` is the executor's hidden parent for its own GUI (CoreGui in a
    // game client); hub scripts parent their ScreenGui there.
    public.raw_set(
        "gethui",
        lua.create_function(|lua, ()| {
            let core = {
                let w = rustudio_engine::runtime::world(lua)?;
                let doc = w.d();
                doc.find_child(doc.dom.root_ref(), "CoreGui")
            };
            match core {
                Some(r) => rustudio_engine::runtime::inst_value(lua, r),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    public.raw_set(
        "getcallingscript",
        lua.create_function(|lua, ()| {
            let g = lua.globals();
            Ok(g.raw_get::<Value>("script")?)
        })?,
    )?;

    // `newcclosure(f)` hands the same function back: Luau closures are
    // already indistinguishable from C ones to `debug.info`, so wrapping
    // would only cost identity.
    public.raw_set(
        "newcclosure",
        lua.create_function(|_, f: Value| Ok(f))?,
    )?;
    // `hookfunction(old, new)` swaps every visible reference to `old` for
    // `new` and answers `old`, so the hook can still call it. Luau has no
    // way to rewrite a closure in place; swapping the references is what a
    // hook is here. A local that captured `old` before the hook keeps it,
    // which is also what an executor's VM does.
    let hook = lua.create_function(|lua, (old, new): (Value, Value)| {
        if let Value::Function(_) = &old {
            swap_references(lua, &old, &new)?;
        }
        Ok(old)
    })?;
    public.raw_set("hookfunction", hook.clone())?;
    public.raw_set("replaceclosure", hook)?;
    public.raw_set(
        "clonefunction",
        lua.create_function(|lua, f: Value| match f {
            Value::Function(f) => {
                let f = f.clone();
                Ok(Value::Function(lua.create_function(
                    move |_, args: mlua::MultiValue| f.call::<mlua::MultiValue>(args),
                )?))
            }
            other => Ok(other),
        })?,
    )?;
    // Only plain tables: an instance's metatable carries the engine's own
    // dispatch (`__namecall`, `__index`), and a replaced field would send
    // calls down a path the engine cannot answer.
    public.raw_set(
        "hookmetamethod",
        lua.create_function(|lua, (obj, name, new): (Value, String, Option<Value>)| {
            let Value::Table(t) = &obj else {
                return Ok(Value::Nil);
            };
            let Some(mt) = raw_metatable(lua, Value::Table(t.clone()))? else {
                return Ok(Value::Nil);
            };
            let old = mt.raw_get::<Value>(name.as_str())?;
            if let Some(new) = new {
                mt.raw_set(name.as_str(), new)?;
            }
            Ok(old)
        })?,
    )?;
    public.raw_set("getfunctionhash", lua.create_function(|_, _: Value| Ok(0i64))?)?;
    public.raw_set(
        "islclosure",
        lua.create_function(|_, f: Value| Ok(matches!(f, Value::Function(_))))?,
    )?;

    // Metatables, read-only flags.
    public.raw_set(
        "getrawmetatable",
        lua.create_function(|lua, v: Value| {
            Ok(raw_metatable(lua, v)?
                .map(Value::Table)
                .unwrap_or(Value::Nil))
        })?,
    )?;
    public.raw_set(
        "setrawmetatable",
        lua.create_function(|_, (_v, mt): (Value, Value)| {
            Ok(mt != Value::Nil)
        })?,
    )?;
    public.raw_set("setreadonly", lua.create_function(|_, _: (Value, Value)| Ok(()))?)?;
    public.raw_set("isreadonly", lua.create_function(|_, _: Value| Ok(false))?)?;
    public.raw_set("make_writeable", lua.create_function(|_, _: Value| Ok(()))?)?;
    public.raw_set("make_readonly", lua.create_function(|_, _: Value| Ok(()) )?)?;

    // Signals: connections, detectors and prompts.
    for f in [
        "firetouchinterest",
        "fireclickdetector",
        "fireproximityprompt",
        "firelplclickdetector",
        "setclipboard",
        "toclipboard",
        "queue_on_teleport",
        "queueonteleport",
        "setfpscap",
    ] {
        public.raw_set(f, lua.create_function(|_, _: mlua::MultiValue| Ok(()))?)?;
    }
    public.raw_set("getfpscap", lua.create_function(|_, ()| Ok(60))?)?;
    public.raw_set(
        "getconnections",
        lua.create_function(|lua, _: Value| lua.create_table())?,
    )?;
    public.raw_set(
        "getsignalnames",
        lua.create_function(|lua, _: Value| lua.create_table())?,
    )?;
    for f in [
        "getloadedmodules",
        "getrunningscripts",
        "getscripts",
        "getscriptbytecode",
        "getnilinstances",
    ] {
        public.raw_set(
            f,
            lua.create_function(|lua, _: mlua::MultiValue| lua.create_table())?,
        )?;
    }

    // The file API, rooted in aimers-workspace/ next to the current
    // directory; nothing outside it is reachable.
    let root = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("aimers-workspace");
    let _ = std::fs::create_dir_all(&root);
    install_files(lua, &public, root.clone())?;

    // HTTP: curl, so stage loaders can fetch their next stage.
    install_http(lua, &public)?;
    public.raw_set(
        "getcustomasset",
        lua.create_function(|_, p: String| Ok(format!("rbxasset://{p}")))?,
    )?;

    // crypt.* the names hubs probe for.
    let crypt = lua.create_table()?;
    crypt.raw_set(
        "base64encode",
        lua.create_function(|_, s: mlua::LuaString| Ok(b64_encode(&s.as_bytes())))?,
    )?;
    crypt.raw_set(
        "base64decode",
        lua.create_function(|_, s: mlua::LuaString| Ok(b64_decode(&s.to_string_lossy())))?,
    )?;
    crypt.raw_set(
        "base64",
        lua.create_table_from([(
            "encode",
            lua.create_function(|_, s: mlua::LuaString| Ok(b64_encode(&s.as_bytes())))?,
        )])?,
    )?;
    public.raw_set("crypt", crypt)?;

    let syn = lua.create_table()?;
    syn.raw_set("identifyexecutor", public.raw_get::<Value>("identifyexecutor")?)?;
    syn.raw_set("request", public.raw_get::<Value>("request")?)?;
    syn.raw_set("protect_gui", lua.create_function(|_, v: Value| Ok(v))?)?;
    syn.raw_set("unprotect_gui", lua.create_function(|_, v: Value| Ok(v))?)?;
    syn.raw_set("queue_on_teleport", lua.create_function(|_, _: Value| Ok(()))?)?;
    public.raw_set("syn", syn.clone())?;
    public.raw_set("Krnl", syn.clone())?;
    public.raw_set("fluxus", syn)?;

    Ok(())
}

fn safe_path(root: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains("..") || name.starts_with('/') || name.starts_with('\\') {
        return None;
    }
    // Drive letters and UNC paths are absolute too.
    if name.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    Some(root.join(name.replace('\\', "/")))
}

fn install_files(lua: &Lua, public: &Table, root: PathBuf) -> mlua::Result<()> {
    public.raw_set("__aimers_root", root.to_string_lossy().to_string())?;

    let write = |lua: &Lua, append: bool| {
        let root = root.clone();
        lua.create_function(move |_, (name, data): (String, mlua::LuaString)| {
            let Some(path) = safe_path(&root, &name) else {
                return Ok(false);
            };
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let mut f = match if append {
                std::fs::OpenOptions::new().create(true).append(true).open(&path)
            } else {
                std::fs::File::create(&path)
            } {
                Ok(f) => f,
                Err(_) => return Ok(false),
            };
            Ok(f.write_all(&data.as_bytes()).is_ok())
        })
    };
    public.raw_set("writefile", write(lua, false)?)?;
    public.raw_set("appendfile", write(lua, true)?)?;

    let read_root = root.clone();
    public.raw_set(
        "readfile",
        lua.create_function(move |_, name: String| match safe_path(&read_root, &name) {
            Some(p) => Ok(std::fs::read_to_string(p).ok()),
            None => Ok(None),
        })?,
    )?;
    let isfile_root = root.clone();
    public.raw_set(
        "isfile",
        lua.create_function(move |_, name: String| {
            Ok(safe_path(&isfile_root, &name).is_some_and(|p| p.is_file()))
        })?,
    )?;
    let isfolder_root = root.clone();
    public.raw_set(
        "isfolder",
        lua.create_function(move |_, name: String| {
            Ok(safe_path(&isfolder_root, &name).is_some_and(|p| p.is_dir()))
        })?,
    )?;
    let del_root = root.clone();
    public.raw_set(
        "delfile",
        lua.create_function(move |_, name: String| {
            Ok(safe_path(&del_root, &name).is_some_and(|p| std::fs::remove_file(p).is_ok()))
        })?,
    )?;
    let del_dir = root.clone();
    public.raw_set(
        "delfolder",
        lua.create_function(move |_, name: String| {
            Ok(safe_path(&del_dir, &name).is_some_and(|p| std::fs::remove_dir_all(p).is_ok()))
        })?,
    )?;
    let mk_root = root.clone();
    public.raw_set(
        "makefolder",
        lua.create_function(move |_, name: String| {
            Ok(safe_path(&mk_root, &name).is_some_and(|p| std::fs::create_dir_all(p).is_ok()))
        })?,
    )?;
    public.raw_set(
        "listfiles",
        lua.create_function(move |lua, name: Option<String>| {
            let dir = match &name {
                Some(n) => match safe_path(&root, n) {
                    Some(p) => p,
                    None => return Ok(lua.create_table()),
                },
                None => root.clone(),
            };
            let mut out = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    out.push(e.path().to_string_lossy().to_string());
                }
            }
            Ok(lua.create_sequence_from(out))
        })?,
    )?;
    Ok(())
}

/// `lua_getmetatable`: the real metatable, past a `__metatable` lock (what
/// `getrawmetatable` promises).
fn raw_metatable(lua: &Lua, v: Value) -> mlua::Result<Option<Table>> {
    unsafe {
        lua.exec_raw::<Option<Table>>(v, |l| {
            if mlua::ffi::lua_getmetatable(l, -1) != 0 {
                mlua::ffi::lua_remove(l, -2);
            } else {
                mlua::ffi::lua_pop(l, 1);
                mlua::ffi::lua_pushnil(l);
            }
        })
    }
}

/// Installs a `hookfunction` by swapping every place that names the old
/// function: the globals scripts read through, `_G`, `getgenv()`, the host
/// globals, and one level into library tables (`string.format`).
fn swap_references(lua: &Lua, old: &Value, new: &Value) -> mlua::Result<usize> {
    let mut roots: Vec<(Table, u32)> = Vec::new();
    if let Ok(public) = rustudio_engine::globals_parity::public_env(lua) {
        roots.push((public, 2));
    }
    for key in ["rs_script_G", "aimers_genv"] {
        if let Ok(Some(t)) = lua.named_registry_value::<Option<Table>>(key) {
            roots.push((t, 1));
        }
    }
    roots.push((lua.globals(), 1));
    let mut seen = std::collections::HashSet::new();
    let mut n = 0;
    for (t, depth) in roots {
        n += swap_in(t, old, new, depth, &mut seen)?;
    }
    Ok(n)
}

fn swap_in(
    t: Table,
    old: &Value,
    new: &Value,
    depth: u32,
    seen: &mut std::collections::HashSet<usize>,
) -> mlua::Result<usize> {
    if !seen.insert(t.to_pointer() as usize) {
        return Ok(0);
    }
    let mut n = 0;
    let mut nested = Vec::new();
    // Assigning over an existing key does not rehash, so writing while the
    // iterator walks `next` is safe.
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        if is_same_function(&v, old) {
            t.raw_set(k, new.clone())?;
            n += 1;
        } else if depth > 1 {
            if let Value::Table(sub) = v {
                nested.push(sub);
            }
        }
    }
    for sub in nested {
        n += swap_in(sub, old, new, depth - 1, seen)?;
    }
    Ok(n)
}

/// Two references to the same closure (`lua_topointer`: the GC object, one
/// per C closure as per Lua closure).
fn is_same_function(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Function(x), Value::Function(y)) => x.to_pointer() == y.to_pointer(),
        _ => false,
    }
}

/// `task.spawn`, `task.defer`, `task.delay` and the legacy `spawn`/`delay`,
/// wrapped so the replay can write the call that scheduled a function and
/// nest the body that ran. Each wrapper is a C closure named like the engine
/// function it replaces, so a script checking closures sees Roblox's own.
fn install_tasks(lua: &Lua, public: &Table) -> mlua::Result<()> {
    let Ok(real) = public.raw_get::<Table>("task") else {
        return Ok(());
    };
    let maker: Function = public.raw_get("__aimers_body")?;
    let wrapped = lua.create_table()?;
    for pair in real.pairs::<Value, Value>() {
        let (k, v) = pair?;
        wrapped.raw_set(k, v)?;
    }
    for (key, label, delayed) in [
        ("spawn", "task.spawn", false),
        ("defer", "task.defer", false),
        ("delay", "task.delay", true),
    ] {
        wrapped.raw_set(key, schedule(lua, &real, &maker, key, label, delayed)?)?;
    }
    public.raw_set("task", wrapped)?;
    public.raw_set("spawn", schedule(lua, &real, &maker, "spawn", "spawn", false)?)?;
    public.raw_set("delay", schedule(lua, &real, &maker, "delay", "delay", true)?)?;
    Ok(())
}

/// One task function: give the engine a Lua body (which may yield) that
/// carries the body's marks, then call the engine's own spawn. This wrapper
/// is a C closure, so `iscclosure(task.spawn)` is true.
fn schedule(
    lua: &Lua,
    real: &Table,
    maker: &Function,
    key: &'static str,
    label: &'static str,
    delayed: bool,
) -> mlua::Result<Function> {
    let real = real.clone();
    let maker = maker.clone();
    let f = lua.create_function(move |lua, args: MultiValue| -> mlua::Result<MultiValue> {
        let items = args.into_vec();
        let at = usize::from(delayed);
        let Some(Value::Function(target)) = items.get(at).cloned() else {
            // Not a function: the engine's own call raises its own error.
            return real.get::<Function>(key)?.call(MultiValue::from_vec(items));
        };
        let delay = if delayed { items.first().cloned() } else { None };
        let cid = rustudio_engine::trace::spawn(lua, label, delay.clone());
        let mut body = Vec::with_capacity(items.len() - at);
        body.push(Value::Integer(cid as i64));
        body.push(Value::Function(target));
        body.extend(items[at + 1..].iter().cloned());
        let runner = maker.call::<Function>(MultiValue::from_vec(body))?;
        let mut call = Vec::with_capacity(2);
        // `task.delay(delayTime, f)`: the time comes first.
        if let Some(d) = delay {
            call.push(d);
        }
        call.push(Value::Function(runner));
        real.get::<Function>(key)?.call(MultiValue::from_vec(call))
    })?;
    rustudio_engine::globals_parity::name_callback(lua, &f, key);
    Ok(f)
}

/// `loadstring`/`load` as a C closure. A Lua implementation reads back as a
/// wrapped native to obfuscators (Hercules raises "Native function replaced
/// or wrapped"), so the compile and the environment hand-off happen here.
/// The chunk runs in the nearest Lua caller's environment, as in Studio.
fn install_loadstring(lua: &Lua, public: &Table) -> mlua::Result<()> {
    let f = lua.create_function(
        |lua, (src, chunk): (Value, Option<Value>)| -> mlua::Result<MultiValue> {
            let Value::String(src) = src else {
                return Ok(MultiValue::from_vec(vec![
                    Value::Nil,
                    Value::String(lua.create_string("loadstring: string expected")?),
                ]));
            };
            let chunk = match chunk {
                Some(Value::String(s)) => s.to_string_lossy().to_string(),
                _ => "loadstring".to_string(),
            };
            let bytes = src.as_bytes().to_vec();
            rustudio_engine::trace::allow_script(&chunk);
            rustudio_engine::trace::load_source(lua, &String::from_utf8_lossy(&bytes), &chunk);
            match lua
                .load(&bytes[..])
                .set_name(format!("={chunk}"))
                .into_function()
            {
                Ok(f) => {
                    if let Some(env) = caller_env(lua)? {
                        f.set_environment(env)?;
                    }
                    Ok(MultiValue::from_vec(vec![Value::Function(f)]))
                }
                Err(e) => Ok(MultiValue::from_vec(vec![
                    Value::Nil,
                    Value::String(lua.create_string(e.to_string())?),
                ])),
            }
        },
    )?;
    rustudio_engine::globals_parity::name_callback(lua, &f, "loadstring");
    public.raw_set("loadstring", &f)?;
    // `load` is the same function in Studio; one reference, two names.
    public.raw_set("load", &f)?;
    Ok(())
}

/// The environment of the nearest Lua frame above this C function (skipping
/// `pcall` and other C frames). `None` when every frame is C.
fn caller_env(lua: &Lua) -> mlua::Result<Option<Table>> {
    unsafe {
        lua.exec_raw::<Option<Table>>((), |l| {
            let mut ar: mlua::ffi::lua_Debug = std::mem::zeroed();
            let mut level = 1;
            // Luau's info letters are lowercase (`s`, not `S`): `f` alone
            // leaves `what` null, which made every frame look like Lua and
            // answered a `pcall(loadstring, ...)` caller with pcall's own
            // environment (the thread globals).
            while mlua::ffi::lua_getinfo(l, level, c"sf".as_ptr(), &mut ar) != 0 {
                let is_c = !ar.what.is_null()
                    && std::ffi::CStr::from_ptr(ar.what).to_bytes() == b"C";
                if !is_c {
                    mlua::ffi::lua_getfenv(l, -1);
                    if mlua::ffi::lua_istable(l, -1) != 0 {
                        // Drop the function under the environment.
                        mlua::ffi::lua_remove(l, -2);
                        return;
                    }
                    mlua::ffi::lua_pop(l, 1);
                }
                mlua::ffi::lua_pop(l, 1);
                level += 1;
            }
            mlua::ffi::lua_pushnil(l);
        })
    }
}

/// The executor `debug` library. Luau's `debug` has `info` and `traceback`;
/// obfuscators (Hercules among them) refuse to run unless the executor's
/// full library is there, and the parts they use (`getupvalue`, the raw
/// metatable) are missing from Luau. The raw access is here, through the C
/// API, so a locked `__metatable` doesn't hide anything and an upvalue keeps
/// its identity.
fn install_debug(lua: &Lua, public: &Table) -> mlua::Result<()> {
    public.raw_set(
        "__aimers_getupvalue",
        lua.create_function(|lua, (f, n): (Function, i32)| -> mlua::Result<MultiValue> {
            unsafe {
                lua.exec_raw::<MultiValue>((f,), |l| {
                    let base = mlua::ffi::lua_gettop(l);
                    let name = mlua::ffi::lua_getupvalue(l, base, n);
                    if name.is_null() {
                        mlua::ffi::lua_settop(l, base - 1);
                    } else {
                        // f, value -> name, value
                        mlua::ffi::lua_pushstring(l, name);
                        mlua::ffi::lua_insert(l, -2);
                        mlua::ffi::lua_remove(l, base);
                    }
                })
            }
        })?,
    )?;
    public.raw_set(
        "__aimers_setupvalue",
        lua.create_function(
            |lua, (f, n, v): (Function, i32, Value)| -> mlua::Result<MultiValue> {
                unsafe {
                    lua.exec_raw::<MultiValue>((f, v), |l| {
                        let base = mlua::ffi::lua_gettop(l);
                        let name = mlua::ffi::lua_setupvalue(l, base - 1, n);
                        if name.is_null() {
                            // No such upvalue: the value (if any) stays.
                            mlua::ffi::lua_settop(l, base - 2);
                        } else {
                            // f (value popped) -> name
                            mlua::ffi::lua_pushstring(l, name);
                            mlua::ffi::lua_remove(l, base - 1);
                        }
                    })
                }
            },
        )?,
    )?;
    public.raw_set(
        "__aimers_nups",
        // Luau's debug info doesn't carry the upvalue count, so walk them.
        lua.create_function(|lua, f: Function| -> mlua::Result<i64> {
            unsafe {
                lua.exec_raw::<i64>((f,), |l| {
                    let base = mlua::ffi::lua_gettop(l);
                    let mut n: i64 = 0;
                    loop {
                        if mlua::ffi::lua_getupvalue(l, base, (n + 1) as i32).is_null() {
                            break;
                        }
                        mlua::ffi::lua_pop(l, 1);
                        n += 1;
                    }
                    mlua::ffi::lua_settop(l, base - 1);
                    mlua::ffi::lua_pushinteger(l, n);
                })
            }
        })?,
    )?;
    public.raw_set(
        "__aimers_getmetatable",
        lua.create_function(|lua, v: Value| -> mlua::Result<MultiValue> {
            unsafe {
                lua.exec_raw::<MultiValue>((v,), |l| {
                    let base = mlua::ffi::lua_gettop(l);
                    if mlua::ffi::lua_getmetatable(l, base) == 0 {
                        mlua::ffi::lua_settop(l, base - 1);
                    } else {
                        // v, metatable -> metatable
                        mlua::ffi::lua_remove(l, base);
                    }
                })
            }
        })?,
    )?;
    public.raw_set(
        "__aimers_setmetatable",
        lua.create_function(|lua, (v, mt): (Value, Value)| -> mlua::Result<Value> {
            unsafe {
                lua.exec_raw::<Value>((v, mt), |l| {
                    let base = mlua::ffi::lua_gettop(l);
                    mlua::ffi::lua_setmetatable(l, base - 1);
                    // lua_setmetatable pops the metatable; keep the value as
                    // the answer, as `setmetatable` does.
                    mlua::ffi::lua_settop(l, base - 1);
                })
            }
        })?,
    )?;

    let prelude = lua.load(
        r#"
local debug = debug
local info = debug.info
local getuv, setupv, nups = __aimers_getupvalue, __aimers_setupvalue, __aimers_nups
local getmt, setmt = __aimers_getmetatable, __aimers_setmetatable
local registry = {}
local impl = {}

-- Lua 5.1-shaped `getinfo`, which is what obfuscators and hubs call.
function impl.getinfo(f, what)
    local ok, s, l, n = pcall(info, f, "sln")
    if not ok then return nil end
    local isfn = type(f) == "function"
    return {
        source = s,
        short_src = s,
        currentline = l,
        linedefined = l,
        lastlinedefined = l,
        what = (s == "[C]") and "C" or "Lua",
        name = n,
        namewhat = "",
        nups = isfn and nups(f) or 0,
        func = isfn and f or nil,
    }
end

function impl.getupvalue(f, n)
    if type(f) ~= "function" or type(n) ~= "number" then return nil end
    return getuv(f, n)
end

function impl.setupvalue(f, n, v)
    if type(f) ~= "function" or type(n) ~= "number" then return nil end
    return setupv(f, n, v)
end

function impl.getupvalues(f)
    local out = {}
    if type(f) ~= "function" then return out end
    for i = 1, nups(f) do
        local name, value = getuv(f, i)
        if name ~= nil then out[name ~= "" and name or i] = value end
    end
    return out
end

-- Lua 5.1's local access needs stack levels an executor's own frames would
-- shift; the functions exist, and answer nil.
function impl.getlocal() return nil end
function impl.setlocal() return nil end
function impl.sethook() end
function impl.gethook() return nil end
function impl.getregistry() return registry end
function impl.getfenv(f) return getfenv(f) end
function impl.setfenv(f, env) return setfenv(f, env) end
function impl.getmetatable(v) return getmt(v) end
function impl.setmetatable(v, mt) return setmt(v, mt) end
function impl.getconstants() return {} end
function impl.getconstant() return nil end
function impl.setconstant() end
function impl.checkmemorycategory() return true end
return impl
"#,
    );
    let impls: Table = prelude
        .set_name("=aimers.debug")
        .set_environment(public.clone())
        .eval()?;
    // Obfuscators (Hercules) read a Lua-implemented debug function as a
    // wrapped native and refuse to run. Each one is a C closure that calls
    // its Lua body, so `debug.info(f, "s")` is "[C]" as with an executor.
    let debug: Table = public.raw_get("debug")?;
    for pair in impls.pairs::<String, Function>() {
        let (name, imp) = pair?;
        let f = lua.create_function(move |_, args: MultiValue| imp.call::<MultiValue>(args))?;
        rustudio_engine::globals_parity::name_callback(lua, &f, &name);
        debug.raw_set(name, f)?;
    }
    Ok(())
}

fn install_http(lua: &Lua, public: &Table) -> mlua::Result<()> {
    let base = public.clone();
    // HttpGet(url) and game:HttpGet(url) are the same call (an executor
    // answers both): the first argument is an instance in the second form.
    let get = lua.create_function(|lua, args: (Value, Option<String>)| {
        let Some(url) = http_url(args) else { return Ok(None) };
        let (code, body) = curl(&url, "GET", &[], None);
        rustudio_engine::trace::http(lua, "GET", &url, code, &body);
        if code >= 200 && code < 400 {
            Ok(Some(body))
        } else {
            Ok(None)
        }
    })?;
    public.raw_set("HttpGet", get.clone())?;
    public.raw_set("HttpGetAsync", get.clone())?;
    public.raw_set("httpget", get)?;

    let post = lua.create_function(|lua, args: (Value, Option<String>, Option<String>)| {
        let (url, body) = match args {
            (Value::String(s), Some(body), None) => (s.to_string_lossy(), Some(body)),
            (Value::String(s), None, body) => (s.to_string_lossy(), body),
            (_, Some(url), body) => (url, body),
            _ => return Ok(None),
        };
        let (code, text) = curl(&url, "POST", &[], body.as_deref());
        rustudio_engine::trace::http(lua, "POST", &url, code, &text);
        if code >= 200 && code < 400 {
            Ok(Some(text))
        } else {
            Ok(None)
        }
    })?;
    public.raw_set("HttpPost", post.clone())?;
    public.raw_set("HttpPostAsync", post)?;

    let request = lua.create_function(|lua, opts: Table| {
        let url: String = opts.get("Url").or_else(|_| opts.get("url")).unwrap_or_default();
        let method: String = opts
            .get("Method")
            .or_else(|_| opts.get("method"))
            .unwrap_or_else(|_| "GET".into());
        let body: Option<String> = opts.get("Body").or_else(|_| opts.get("body")).ok();
        let mut headers = Vec::new();
        if let Ok(h) = opts.get::<Table>("Headers").or_else(|_| opts.get::<Table>("headers")) {
            for pair in h.pairs::<String, String>().flatten() {
                headers.push(pair);
            }
        }
        let (code, text) = curl(&url, &method, &headers, body.as_deref());
        rustudio_engine::trace::http(lua, &method, &url, code, &text);
        let t = lua.create_table()?;
        t.set("Success", (200..400).contains(&code))?;
        t.set("StatusCode", code)?;
        t.set("StatusMessage", "")?;
        t.set("Body", text)?;
        t.set("Headers", lua.create_table()?)?;
        Ok(t)
    })?;
    public.raw_set("request", request.clone())?;
    public.raw_set("http_request", request.clone())?;
    public.raw_set("httpRequest", request.clone())?;
    let http = lua.create_table()?;
    http.raw_set("request", request)?;
    base.raw_set("http", http)?;
    Ok(())
}

/// The URL of `HttpGet(url)` or `game:HttpGet(url)`: a string first, or an
/// instance (the method form) followed by the URL.
fn http_url(args: (Value, Option<String>)) -> Option<String> {
    match args {
        (Value::String(s), _) => Some(s.to_string_lossy()),
        (_, Some(url)) => Some(url),
        _ => None,
    }
}

/// `curl` with a 20 s cap; returns (HTTP status, body). Status 0 is "no
/// answer" (DNS, TLS, curl missing).
fn curl(url: &str, method: &str, headers: &[(String, String)], body: Option<&str>) -> (i64, String) {
    if url.is_empty() {
        return (0, String::new());
    }
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "-L", "--max-time", "20", "-X", method]);
    for (k, v) in headers {
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    if let Some(b) = body {
        cmd.arg("--data-binary").arg(b);
    }
    cmd.arg("-w").arg("\n%{http_code}");
    cmd.arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    match cmd.output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            match text.rsplit_once('\n') {
                Some((body, code)) => (code.trim().parse().unwrap_or(0), body.to_string()),
                None => (0, text),
            }
        }
        Err(_) => (0, String::new()),
    }
}

fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn b64_decode(s: &str) -> String {
    let val = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => 0,
        }
    };
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        buf = (buf << 6) | val(c);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    String::from_utf8_lossy(&out).to_string()
}
