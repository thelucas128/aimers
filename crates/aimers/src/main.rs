//! aimers: run a Roblox script headlessly on rustudio's engine and log
//! everything it touches.
//!
//! The engine is the real Luau VM with rustudio's Roblox API (services,
//! instances, signals, datatypes, the client DataModel) — not an emulation
//! of it. The log is the environment log: global reads and writes, instance
//! member reads and writes, method calls, instances created, errors and
//! output, each with the script line that caused it.

mod executor;
mod replay;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mlua::Value;
use rbx_dom_weak::types::{CFrame, Enum, Matrix3, Ref, Variant, Vector3};
use rbx_dom_weak::{ustr, InstanceBuilder};
use rustudio_core::doc::Document;
use rustudio_core::mech;
use rustudio_engine::runtime::{self, RunMode, Runtime, Side, World};
use rustudio_engine::trace;

struct Args {
    script: PathBuf,
    out: Option<PathBuf>,
    log: Option<PathBuf>,
    seconds: f64,
    dt: f64,
    place: Option<PathBuf>,
    server: bool,
    no_trace: bool,
    quiet: bool,
    all: bool,
    vanilla: bool,
    limit: u64,
    /// `Service.Path.To.Thing=Class` instances to put in the DataModel
    /// before the script runs (a RemoteFunction it waits for, a GUI folder).
    children: Vec<String>,
    /// Raise the signals the script connected to, so handler bodies run.
    fire: bool,
    /// Make a child of this class when the script looks for one that isn't
    /// there (the server would normally have made it).
    satisfy: Option<String>,
    /// Answer the script's remote traffic as an absent server would: a
    /// `FireServer` comes back as `OnClientEvent`, an `InvokeServer`
    /// returns a permissive stub.
    echo: bool,
}

fn usage() -> ! {
    eprintln!(
        "aimers — run a Roblox script on rustudio's engine and log its environment

usage: aimers <script.luau> [options]

  --out <file>      env log (JSONL)        [default: <script>.aimers.jsonl]
  --log <file>      readable replay        [default: <script>.aimers.lua]
  --seconds <n>     simulated run time     [default: 5]
  --dt <n>          frame step, seconds    [default: 0.0166667]
  --place <file>    run against a .rbxl/.rbxlx place instead of a new baseplate
  --server          run as a server Script instead of a client LocalScript
  --child <path>=<Class>
                    put an instance in the DataModel before the script runs,
                    e.g. ReplicatedStorage.SubmitPlatform=RemoteFunction
                    (repeatable; missing intermediate folders are created)
  --fire            raise every signal the script connected to, at 25%, 50%
                    and 75% of the run, so its handler bodies are logged
  --satisfy <Class>
                    when the script looks for a child that isn't there
                    (FindFirstChild/WaitForChild), make it, e.g. RemoteFunction
  --echo-remotes    answer the script's remotes as a server would: FireServer
                    comes back as OnClientEvent, InvokeServer returns a stub
  --all             log every script (core scripts, modules, plugins too)
  --vanilla         no executor globals (plain Roblox environment)
  --no-trace        execute without writing the env log
  --limit <n>       cap log lines          [default: 1000000]
  --quiet           do not echo print/warn output

The script runs in a client DataModel (Players.LocalPlayer, PlayerScripts)
unless --server is given."
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut script = None;
    let mut out = None;
    let mut log = None;
    let mut seconds = 5.0;
    let mut dt = 1.0 / 60.0;
    let mut place = None;
    let mut server = false;
    let mut no_trace = false;
    let mut quiet = false;
    let mut all = false;
    let mut vanilla = false;
    let mut limit = 1_000_000;
    let mut children = Vec::new();
    let mut fire = false;
    let mut satisfy = None;
    let mut echo = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => usage(),
            "--out" => out = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--log" => log = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--seconds" => {
                seconds = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--dt" => {
                dt = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--place" => place = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--limit" => {
                limit = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--child" => children.push(args.next().unwrap_or_else(|| usage())),
            "--satisfy" => satisfy = Some(args.next().unwrap_or_else(|| usage())),
            "--echo-remotes" => echo = true,
            "--server" => server = true,
            "--fire" => fire = true,
            "--all" => all = true,
            "--vanilla" => vanilla = true,
            "--no-trace" => no_trace = true,
            "--quiet" => quiet = true,
            other if other.starts_with('-') => {
                eprintln!("aimers: unknown option {other}");
                usage()
            }
            other => {
                if script.is_some() {
                    usage();
                }
                script = Some(PathBuf::from(other));
            }
        }
    }

    let Some(script) = script else { usage() };
    Args {
        script,
        out,
        log,
        seconds,
        dt,
        place,
        server,
        no_trace,
        quiet,
        all,
        vanilla,
        limit,
        children,
        fire,
        satisfy,
        echo,
    }
}

/// A minimal R6 body in Workspace, wired to `Player.Character`: enough for
/// the parts and Humanoid scripts reach for (`HumanoidRootPart`, `Head`,
/// `FindFirstChildOfClass("Humanoid")`). Aimers' scripts run on a player
/// that has already spawned, as with a real executor.
fn add_character(doc: &mut Document, player: Ref, name: &str) -> Ref {
    let Some(ws) = doc.workspace() else {
        return Ref::none();
    };
    let model = doc
        .dom
        .insert(ws, InstanceBuilder::new("Model").with_name(name));
    let part = |doc: &mut Document, part: &str, size: (f32, f32, f32), pos: (f32, f32, f32)| {
        doc.dom.insert(
            model,
            InstanceBuilder::new("Part")
                .with_name(part)
                .with_property("Size", Vector3::new(size.0, size.1, size.2))
                .with_property(
                    "CFrame",
                    CFrame::new(Vector3::new(pos.0, pos.1, pos.2), Matrix3::identity()),
                )
                .with_property("Anchored", true),
        );
    };
    // Standing on y = 0, the way a spawned R6 avatar is.
    part(doc, "HumanoidRootPart", (2.0, 2.0, 1.0), (0.0, 3.0, 0.0));
    part(doc, "Torso", (2.0, 2.0, 1.0), (0.0, 3.0, 0.0));
    part(doc, "Head", (2.0, 1.0, 1.0), (0.0, 4.5, 0.0));
    doc.dom
        .insert(model, InstanceBuilder::new("Humanoid").with_name("Humanoid"));
    if let Some(i) = doc.dom.get_by_ref_mut(player) {
        i.properties
            .insert(ustr("Character"), Variant::Ref(model));
    }
    doc.mark_prop(player, ustr("Character"));
    model
}

/// A client DataModel like Studio's Play: every service, the local Player
/// with its PlayerGui / Backpack / PlayerScripts, and a camera.
fn build_client(doc: &mut Document, source: &str, name: &str) -> Ref {
    let root = doc.dom.root_ref();
    // Executors run with CoreGui reachable; a LocalScript on a real client
    // is refused it, which is exactly the check hub scripts use to tell a
    // studio run from an executed one.
    if doc.find_child(root, "CoreGui").is_none() {
        doc.dom
            .insert(root, InstanceBuilder::new("CoreGui").with_name("CoreGui"));
    }
    let players = doc
        .find_child(root, "Players")
        .unwrap_or_else(|| doc.dom.insert(root, InstanceBuilder::new("Players").with_name("Players")));
    let player = doc.dom.insert(
        players,
        InstanceBuilder::new("Player")
            .with_name(mech::PLAYER_NAME)
            .with_property("DisplayName", mech::PLAYER_NAME)
            .with_property("UserId", 1i64),
    );
    doc.dom
        .insert(player, InstanceBuilder::new("PlayerGui").with_name("PlayerGui"));
    doc.dom
        .insert(player, InstanceBuilder::new("Backpack").with_name("Backpack"));
    let scripts = doc
        .dom
        .insert(player, InstanceBuilder::new("PlayerScripts").with_name("PlayerScripts"));
    if let Some(i) = doc.dom.get_by_ref_mut(players) {
        i.properties
            .insert(ustr("LocalPlayer"), Variant::Ref(player));
    }
    doc.mark_prop(players, ustr("LocalPlayer"));

    // Scripts run the way they do under an executor: the player is already
    // in the game, so `Player.Character` is set and the body exists.
    add_character(doc, player, mech::PLAYER_NAME);

    if let Some(ws) = doc.workspace() {
        let cam = doc.find_child(ws, "Camera").unwrap_or_else(|| {
            doc.dom.insert(
                ws,
                InstanceBuilder::new("Camera")
                    .with_name("Camera")
                    .with_property("FieldOfView", mech::FOV_DEG),
            )
        });
        if let Some(i) = doc.dom.get_by_ref_mut(ws) {
            i.properties.insert(ustr("CurrentCamera"), Variant::Ref(cam));
        }
        doc.mark_prop(ws, ustr("CurrentCamera"));
        if let Some(i) = doc.dom.get_by_ref_mut(cam) {
            i.properties.insert(
                ustr("CameraType"),
                Variant::Enum(Enum::from_u32(mech::CAMERA_TYPE_CUSTOM)),
            );
        }
        doc.mark_prop(cam, ustr("CameraType"));
    }

    doc.dom.insert(
        scripts,
        InstanceBuilder::new("LocalScript")
            .with_name(name)
            .with_property("Source", source.to_string()),
    )
}

/// A server DataModel: the services and a Script under ServerScriptService.
fn build_server(doc: &mut Document, source: &str, name: &str) -> Ref {
    let root = doc.dom.root_ref();
    let parent = doc
        .find_child(root, "ServerScriptService")
        .unwrap_or_else(|| {
            doc.dom.insert(
                root,
                InstanceBuilder::new("ServerScriptService").with_name("ServerScriptService"),
            )
        });
    doc.dom.insert(
        parent,
        InstanceBuilder::new("Script")
            .with_name(name)
            .with_property("Source", source.to_string()),
    )
}

/// The instance's full DataModel name (`Players.Player.PlayerScripts.foo`),
/// the name the script's frames carry.
fn full_name(doc: &Document, r: Ref) -> String {
    let root = doc.dom.root_ref();
    let mut parts = Vec::new();
    let mut cur = r;
    while let Some(i) = doc.dom.get_by_ref(cur) {
        if cur == root {
            break;
        }
        parts.push(i.name.clone());
        cur = i.parent();
        if cur.is_none() {
            break;
        }
    }
    parts.reverse();
    parts.join(".")
}

/// The child of `parent` with this name (DataModel names, not classes).
fn child_named(doc: &Document, parent: Ref, name: &str) -> Option<Ref> {
    doc.dom
        .get_by_ref(parent)?
        .children()
        .iter()
        .copied()
        .find(|c| doc.dom.get_by_ref(*c).is_some_and(|i| i.name == name))
}

/// `Service.Path.To.Name=Class`: puts the instance in the DataModel before
/// the script runs, creating any missing intermediate objects as Folders.
fn add_child(doc: &mut Document, spec: &str) -> Result<(), String> {
    let (path, class) = spec
        .split_once('=')
        .ok_or_else(|| format!("--child {spec}: expected <path>=<Class>"))?;
    let class = class.trim();
    if class.is_empty() {
        return Err(format!("--child {spec}: missing class"));
    }
    let mut segments = path.split('.').filter(|s| !s.is_empty());
    let Some(first) = segments.next() else {
        return Err(format!("--child {spec}: empty path"));
    };
    let root = doc.dom.root_ref();
    let mut cur = child_named(doc, root, first)
        .or_else(|| doc.find_child(root, first))
        .ok_or_else(|| format!("--child {spec}: no service {first}"))?;
    let rest: Vec<&str> = segments.collect();
    for (i, segment) in rest.iter().enumerate() {
        let leaf = i + 1 == rest.len();
        cur = match child_named(doc, cur, segment) {
            Some(existing) => existing,
            None => doc.dom.insert(
                cur,
                if leaf {
                    InstanceBuilder::new(class)
                } else {
                    InstanceBuilder::new("Folder")
                }
                .with_name(*segment),
            ),
        };
    }
    Ok(())
}

/// A service's ref in the world's DataModel (RunService, for frame signals).
fn service_ref(w: &World, name: &str) -> Option<Ref> {
    let doc = w.d();
    doc.find_child(doc.dom.root_ref(), name)
}

/// The local player in the client DataModel.
fn player_ref(w: &World) -> Option<Ref> {
    let doc = w.d();
    let players = doc.find_child(doc.dom.root_ref(), "Players")?;
    doc.dom
        .get_by_ref(players)?
        .children()
        .iter()
        .copied()
        .find(|c| doc.dom.get_by_ref(*c).is_some_and(|i| i.class == "Player"))
}

/// The local player's character model, when it has spawned.
fn character_ref(w: &World) -> Option<Ref> {
    let player = player_ref(w)?;
    match w.d().dom.get_by_ref(player)?.properties.get(&ustr("Character")) {
        Some(Variant::Ref(r)) if r.is_some() => Some(*r),
        _ => None,
    }
}

fn local_player(lua: &mlua::Lua) -> Option<Ref> {
    let w = runtime::world(lua).ok()?;
    player_ref(&w)
}

fn local_character(lua: &mlua::Lua) -> Option<Ref> {
    let w = runtime::world(lua).ok()?;
    character_ref(&w)
}

fn character_part(lua: &mlua::Lua, name: &str) -> Option<Ref> {
    let w = runtime::world(lua).ok()?;
    let character = character_ref(&w)?;
    child_named(w.d(), character, name)
}

fn first_child(lua: &mlua::Lua, r: Ref) -> Option<Ref> {
    let w = runtime::world(lua).ok()?;
    w.d().dom.get_by_ref(r)?.children().first().copied()
}

fn parent_of(lua: &mlua::Lua, r: Ref) -> Option<Ref> {
    let w = runtime::world(lua).ok()?;
    let parent = w.d().dom.get_by_ref(r)?.parent();
    parent.is_some().then_some(parent)
}

/// Arguments for a signal raised without a real cause: a Touched handler
/// wants the part that hit it, a Changed handler the property that changed.
/// The rest get none; a handler that indexes them will error into the log,
/// which still shows what it would have run.
fn enum_item(lua: &mlua::Lua, kind: &str, item: &str) -> Value {
    let Ok(enums) = lua.globals().get::<mlua::Table>("Enum") else {
        return Value::Nil;
    };
    let Ok(kind) = enums.get::<mlua::Table>(kind) else {
        return Value::Nil;
    };
    kind.get::<Value>(item).unwrap_or(Value::Nil)
}

fn signal_args(runtime: &Runtime, r: Ref, signal: &str) -> Vec<Value> {
    let lua = &runtime.lua;
    let event = signal.split(':').next().unwrap_or(signal);
    let text = |s: &str| lua.create_string(s).map(Value::String).unwrap_or(Value::Nil);
    match event {
        "MessageOut" => vec![
            text("message"),
            enum_item(lua, "MessageType", "MessageOutput"),
        ],
        "Touched" | "TouchEnded" => {
            let hit = character_part(lua, "HumanoidRootPart").unwrap_or(r);
            vec![runtime.inst(hit)]
        }
        "Changed" => vec![text("Value")],
        "ChildAdded" | "DescendantAdded" | "ChildRemoved" | "DescendantRemoving" => {
            match first_child(lua, r) {
                Some(child) => vec![runtime.inst(child)],
                None => vec![runtime.inst(r)],
            }
        }
        // A server-side remote handler and a Player's own events receive the
        // player first; a client remote handler receives only what the
        // server sent (which `--echo-remotes` supplies, if anything).
        "PlayerAdded" | "PlayerRemoving" | "OnServerEvent" => {
            match local_player(lua) {
                Some(player) => vec![runtime.inst(player)],
                None => Vec::new(),
            }
        }
        "OnClientEvent" => Vec::new(),
        "CharacterAdded" | "CharacterRemoving" => match local_character(lua) {
            Some(character) => vec![runtime.inst(character)],
            None => Vec::new(),
        },
        "AncestryChanged" => vec![
            runtime.inst(r),
            parent_of(lua, r).map_or(Value::Nil, |p| runtime.inst(p)),
        ],
        _ => Vec::new(),
    }
}

/// Puts `trace::stub_module` in front of `require`, so a module the harness
/// made (only the server had it) runs as a permissive stub instead of
/// failing the script.
fn wrap_require(runtime: &Runtime) -> mlua::Result<()> {
    let lua = &runtime.lua;
    let g = lua.globals();
    let orig: mlua::Function = g.get("require")?;
    let wrapped = lua.create_function(move |lua, v: Value| {
        trace::stub_module(lua, &v);
        orig.call::<Value>(v)
    })?;
    g.set("require", wrapped)
}

/// `--echo-remotes`: the value an `InvokeServer` with no server answers
/// with. Every member read and every call answers another stub, so the
/// script's code after the call keeps running instead of erroring.
fn install_remote_stub(runtime: &Runtime) -> mlua::Result<()> {
    runtime
        .lua
        .load(
            r#"
local function proxy()
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
    })
end
__rs_aimers_invoke = function() return proxy() end
"#,
        )
        .set_name("=aimers.echo")
        .exec()
}

/// Raises every traced connection not raised yet. Called a few times through
/// the run so a connection made after an earlier point still runs.
fn fire_pending(runtime: &Runtime, fired: &mut HashSet<(Ref, String)>) {
    for (r, signal) in trace::traced_connections() {
        if !fired.insert((r, signal.clone())) {
            continue;
        }
        let args = signal_args(runtime, r, &signal);
        runtime.fire(r, &signal, args);
    }
}

fn main() -> ExitCode {
    let args = parse_args();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("aimers: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<ExitCode, String> {
    let source = std::fs::read_to_string(&args.script)
        .map_err(|e| format!("cannot read {}: {e}", args.script.display()))?
        // A UTF-8 BOM (some editors write one) is not Luau.
        .trim_start_matches('\u{feff}')
        .to_string();
    let name = args
        .script
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script")
        .to_string();

    let stem = args.script.with_extension("");
    let log_path = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}.aimers.jsonl", stem.display())));
    let replay_path = args
        .log
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}.aimers.lua", stem.display())));

    let mut doc = match &args.place {
        Some(path) => Document::open(path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?,
        None => Document::new_place(),
    };
    doc.ensure_services();

    let script_ref = if args.server {
        build_server(&mut doc, &source, &name)
    } else {
        build_client(&mut doc, &source, &name)
    };
    let script_name = full_name(&doc, script_ref);

    for spec in &args.children {
        add_child(&mut doc, spec)?;
    }

    if !args.no_trace {
        // Filter before the engine's warm-up and core scripts run, so the log
        // holds the target script's own statements only (unless --all).
        let only = if args.all {
            None
        } else {
            Some(script_name.as_str())
        };
        trace::start(&log_path, args.limit, only)
            .map_err(|e| format!("cannot write {}: {e}", log_path.display()))?;
        if let Some(class) = &args.satisfy {
            trace::satisfy_with(class);
        }
        if args.echo {
            trace::echo_remotes(true);
        }
    }

    let mode = if args.server {
        RunMode::Run
    } else {
        RunMode::Play
    };

    let runtime = Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    runtime
        .install_side()
        .map_err(|e| format!("runtime: {}", runtime::describe_error(&e)))?;
    rustudio_engine::lua_bridge::seed_vm(&runtime.lua, 2);

    let mut world = World::new(doc, mode);
    world.side = if args.server {
        Side::Server
    } else {
        Side::Client
    };
    world.has_peer = false;
    world.realtime = false;
    runtime.enter(world);
    runtime
        .bind_globals()
        .map_err(|e| format!("globals: {}", runtime::describe_error(&e)))?;
    if args.satisfy.is_some() {
        wrap_require(&runtime).map_err(|e| format!("require: {}", runtime::describe_error(&e)))?;
    }
    if args.echo {
        install_remote_stub(&runtime)
            .map_err(|e| format!("remote stub: {}", runtime::describe_error(&e)))?;
    }
    if !args.vanilla {
        executor::install(&runtime.lua)
            .map_err(|e| format!("executor surface: {}", runtime::describe_error(&e)))?;
    }
    runtime.warm_queries();

    eprintln!(
        "aimers: running {} ({}) for {}s",
        args.script.display(),
        if args.server { "server" } else { "client" },
        args.seconds
    );

    runtime.run_script(script_ref);

    let frames = (args.seconds / args.dt).ceil() as u64;
    let mut marks = HashSet::new();
    if args.fire {
        for share in 1..=3 {
            marks.insert(frames * share / 4);
        }
    }
    let mut fired = HashSet::new();
    // The frame signals a headless step loop would otherwise never raise:
    // PostSimulation and Heartbeat before the step, RenderStepped and
    // PreRender after it. The player fires the same set (a server run has no
    // render side and gets the two simulation signals). GUI libraries wait on
    // RenderStepped while they build, and a script's Heartbeat loop only runs
    // if something fires it.
    let run_service = runtime
        .world_mut()
        .and_then(|w| service_ref(&w, "RunService"));
    let dt = Value::Number(args.dt);
    for i in 0..frames {
        if let Some(rs) = run_service {
            runtime.fire(rs, "PostSimulation", vec![dt.clone()]);
            runtime.fire(rs, "Heartbeat", vec![dt.clone()]);
        }
        runtime.step(args.dt);
        if let Some(rs) = run_service {
            if !args.server {
                runtime.fire(rs, "RenderStepped", vec![dt.clone()]);
                runtime.fire(rs, "PreRender", vec![dt.clone()]);
                runtime.dispatch();
            }
        }
        if marks.contains(&i) {
            fire_pending(&runtime, &mut fired);
        }
    }
    if args.fire {
        fire_pending(&runtime, &mut fired);
        eprintln!("aimers: fired {} traced connections", fired.len());
    }

    let world = runtime.leave();
    if !args.quiet {
        for (level, line) in &world.output {
            println!("[{}] {}", level_str(*level), line);
        }
    }
    let errors = world
        .output
        .iter()
        .filter(|(l, _)| matches!(l, rustudio_core::log::Level::Error))
        .count();

    drop(runtime);

    if !args.no_trace {
        let (written, dropped) = trace::stop();
        eprintln!("aimers: {written} log lines -> {}", log_path.display());
        if dropped > 0 {
            eprintln!("aimers: {dropped} lines past --limit were dropped");
        }
        let replay_source = replay::Source::new(&name, &source);
        match replay::write(&log_path, &replay_path, &name, Some(&replay_source)) {
            Ok(stats) => eprintln!(
                "aimers: replay ({} calls, {} writes, {} connections, {} instances) -> {}",
                stats.calls,
                stats.sets,
                stats.connects,
                stats.instances,
                replay_path.display()
            ),
            Err(e) => eprintln!("aimers: replay: {e}"),
        }
    }

    Ok(if errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn level_str(level: rustudio_core::log::Level) -> &'static str {
    match level {
        rustudio_core::log::Level::Info => "info",
        rustudio_core::log::Level::Warn => "warn",
        rustudio_core::log::Level::Error => "error",
    }
}

#[allow(dead_code)]
fn _assert_path(_: &Path) {}
