# aimers

Run a Roblox/Luau script headlessly on rustudio's engine and write down
everything it touches: the environment log. Then read the log back as the
statements the script ran.

`aimers` is not an emulator. It runs the real Luau VM with rustudio's Roblox
API — services, instances, signals, datatypes, the client DataModel — and adds
the few globals a script written for an executor expects (`loadstring`,
`getgenv`, `HttpGet`).

## Usage

```sh
cargo build -p aimers
target/debug/aimers path/to/script.lua
```

That runs the script as a `LocalScript` on a Play-mode client for 5 seconds
and writes next to it:

- `script.aimers.jsonl` — the environment log, one JSON object per event
- `script.aimers.lua` — the same events as readable statements, grouped by
  script line

```
target/debug/aimers script.lua --seconds 10 --out log.jsonl --log replay.lua
target/debug/aimers script.lua --server          # a Script in ServerScriptService
target/debug/aimers script.lua --place base.rbxl # start from a place file
target/debug/aimers script.lua --all             # log core scripts and modules too
target/debug/aimers script.lua --vanilla         # no executor globals
target/debug/aimers script.lua --no-trace        # just run it
target/debug/aimers script.lua --fire --satisfy RemoteFunction --echo-remotes
```

| Flag | Meaning |
|---|---|
| `--out PATH` | JSONL log path (default `<script>.aimers.jsonl`) |
| `--log PATH` | replay path (default `<script>.aimers.lua`) |
| `--seconds N` | simulated time to run for (default 5) |
| `--dt N` | frame step (default 1/60) |
| `--place PATH` | start from a place instead of an empty one |
| `--server` | server `Script` instead of a client `LocalScript` |
| `--child PATH=Class` | put an instance in the DataModel before the script runs (`ReplicatedStorage.Remote=RemoteEvent`, repeatable) |
| `--fire` | raise every signal the script connected to, at 25%, 50%, 75% and the end of the run |
| `--satisfy Class` | make a child the script looks for and doesn't find (`FindFirstChild`/`WaitForChild`, or a `workspace.Map` read) |
| `--echo-remotes` | answer the script's remotes: `FireServer` comes back as `OnClientEvent`, `InvokeServer` returns a stub |
| `--all` | log every script, not just the target and its `loadstring` chunks |
| `--vanilla` | plain Roblox environment, no executor globals |
| `--no-trace` | run without writing the log |
| `--quiet` | don't print the script's output to the console |
| `--limit N` | stop after N log lines (default 1,000,000) |

Time is simulated: `task.wait`, `wait` and `Signal:Wait` are stepped in fixed
`--dt` frames, so a run is deterministic and bounded. Each frame raises the
`RunService` signals a client frame raises (`PostSimulation`, `Heartbeat`,
then `RenderStepped`, `PreRender`), so a script waiting on a frame, or a GUI
library that builds between `RenderStepped` waits, keeps going. Exit code is 1
when the script raised an error.

## The client

A client DataModel has every service, `Players.LocalPlayer` with `PlayerGui`,
`Backpack` and `PlayerScripts`, `Workspace.CurrentCamera`, `CoreGui`, and a
minimal spawned character (`Humanoid`, `HumanoidRootPart`, `Head`, `Torso`).
Scripts therefore see what they see under an executor on a live client: the
player exists, the character exists, `CoreGui` is reachable.

The target script runs with the trace already on, so the log starts with its
own first statement. By default only frames from that script (and chunks it
compiles through `loadstring`) are logged; `--all` opens the log to everything,
core scripts included.

## Driving the script forward

An obfuscated script spends its first second building a VM and then waits for
things a real server would provide: a remote that exists, a reply to
`InvokeServer`, an event that fires. Without help the log ends there. These
flags supply the missing half:

- `--child ReplicatedStorage.SubmitPlatform=RemoteFunction` puts the instance
  in the DataModel before the script starts. Use it when you know what the
  script expects; repeat it for each instance.
- `--satisfy RemoteFunction` does the same lazily: when `FindFirstChild` or
  `WaitForChild` misses, the child is made with that class under the instance
  the call was made on, and the call answers it. A plain `inst.Name` read of a
  missing PascalCase child (`workspace.Map`, `Remotes.Mechanics`) makes it the
  same way. A made instance's class is repaired from how the script uses it — a
  `FireServer` child becomes a `RemoteEvent`, an `InvokeServer` one a
  `RemoteFunction`, `IsA("ModuleScript")`/`require` a `ModuleScript` whose
  source is a permissive stub. A property the placeholder class lacks also
  repairs it when the instance's name points at the class
  (`Obsidian_ColorCorrection` written `Enabled` becomes a
  `ColorCorrectionEffect`). Each repair is a `class` event.
- `--fire` raises every signal the script connected to at 25%, 50%, 75% and
  the end of the run, with arguments shaped for the event (`Touched` gets a
  part, `MessageOut` a message, `OnServerEvent`/`PlayerAdded` the local
  player). The handler bodies then run and are logged, so the replay shows
  them inside their `:Connect`.
- `--echo-remotes` answers the remotes the harness made the way a server
  would: `FireServer` re-fires `OnClientEvent` with the same arguments (at
  most three times per remote), and `InvokeServer` returns a permissive proxy
  table, so code that indexes the reply keeps going.

`--satisfy` and `--echo-remotes` need the trace on (they live in the event
hooks). Every instance they create is flagged in the log, so a replay never
pretends the script found something for real.

## The environment log

One JSON object per line, in the order the events happened. Every event has
`ev`, `src` (the script line that caused it, `Name:line`) and `t` (seconds
since the run started).

| `ev` | Fields | When |
|---|---|---|
| `global_get` | `key`, `value` | a script reads a global (`game`, `print`, `task`, ...) |
| `global_set` | `key`, `value` | a script assigns a global |
| `get` | `id`, `path`, `key`, `value` | an instance member is read |
| `set` | `id`, `path`, `key`, `value`, `vid`, `vpath` | an instance member is written (the `v*` fields name an instance on the right) |
| `call` | `id`, `path`, `key`, `args` | a method is called |
| `ret` | `id`, `path`, `rid`, `rpath`, `key` | a method returned an instance (`rid`/`rpath`: the instance it was called on) |
| `ret_value` | `rid`, `rpath`, `key`, `value` | a method returned a non-instance value (string, number, boolean) |
| `connect` | `id`, `path`, `key`, `once`, `cid` | `signal:Connect(fn)` / `:Once(fn)` (`cid`: the connection's id) |
| `spawn` | `key`, `cid`, `arg` | a function scheduled by `task.spawn`/`task.defer`/`task.delay`/`spawn`/`delay` (`arg`: the delay) |
| `wait` | `id`, `path`, `key` | `signal:Wait()` |
| `cb` | `cid`, `phase` (`start`/`end`) | a connection's handler or a spawned body began or finished on its own thread |
| `new` | `id`, `class`, `path` | `Instance.new` made an instance |
| `made` | `id`, `class`, `path` | `--satisfy` made an instance the script was looking for |
| `class` | `id`, `path`, `class`, `stub` | a made instance was given the class its use implies (or repaired in place) |
| `output` | `level`, `value` | `print` / `warn` / engine messages, with the text they ran with |
| `loadsrc` | `kind`, `name`, `value` | source a chunk was compiled from (`kind`: `loadstring` or `require`; `name`: the chunk name) |
| `http` | `method`, `url`, `status`, `value` | an `HttpGet`/`HttpPost`/`request` was answered (with its body) |
| `error` | `value`, `src` | a runtime error |

Every event also has `tid` (the thread that ran it): a connection's handler
runs on its own thread, so a `cb` start/end pair brackets the statements it
ran.

A method reached the other way round — `obj.Method(obj, ...)`, index then call,
the way an obfuscated VM calls the engine — is the same `call`/`ret` event as
`obj:Method(...)`.

`made` and `class` are what the driving flags did, not what the script saw:
`made` means the instance did not exist until `--satisfy` created it, `class`
means it was given (or its created class changed to) the class the script's
use implies, with `stub` saying a `ModuleScript` was filled with a permissive
stub source.

`id` is the low bits of the instance's `Ref`, stable within a run; `path` is
its full DataModel name (`Workspace.Model.Part`). Values are compact:
strings quoted and truncated, tables one level deep, instances as
`<Instance Path>`, functions as `<function>`, and the datatypes as the
constructor that makes them again (`Enum.Font.SourceSansBold`,
`Vector2.new(1, 1)`, `UDim2.new(0, 100, 0, 11)`,
`Color3.new(1, 0.5, 0)`, `CFrame.fromMatrix(...)` when the frame is rotated,
`BrickColor.new("Bright red")`).

## The replay

`script.aimers.lua` reads like the script's own statements in the order they
ran: `Instance.new` results are bound to the variable the line assigns
(`local part = Instance.new("Part")`), method results that are instances are
bound at the call that returned them (`local Baseplate =
workspace:FindFirstChild("Baseplate")`, `local Made = v0:Clone()`), services
become locals the first time the script gets them (`local Players =
game:GetService("Players")`, then `Players.LocalPlayer`), and other call
results take the variable their line assigns (`local text =
HttpService:JSONEncode(data)`). Member writes appear as assignments. Calls
come back as calls, with instance arguments as expressions
(`Players:GetPlayerFromCharacter(workspace.Player)`), and `print` and `warn`
appear as themselves.

Signals are connections with their handler inside:

```lua
workspace.ChildAdded:Connect(function(child)
    -- line 18
    print("child Late")
end)
```

The log records which thread ran a handler (`tid`, and a `cb` start/end
pair), so a callback whose statements ran with nothing else in between is
written inside its `:Connect`; repeated fires add their statements to the
same body. A handler that never ran prints `function(...) end`, and one whose
run interleaved with another thread (it yielded, the main script carried on)
is left out of the closure and its statements appear on their own lines, in
the order they happened.

A statement is written once, however many times it ran, and its line ends
with `-- ×N` when it ran more than once (an asset status fetched for seven
assets, a `WaitForChild` polled every frame). The same statement in another
handler is another statement: each callback body keeps its own copy. A
member read is noted the first time with the value it answered
(`-- part.Size = Vector3.new(4, 1, 2)`) and again when the value changes, so
a counter or label that updates is visible; a read that answers the same
value stays noted once.

Globals are collected the same way: the first read of each since the last
statement goes into one comment on the line it was read on (`-- reads game,
print, task`), eight to a line. A VM reads dozens of globals for one
statement, so grouping keeps that statement visible; a read on another line
starts a new comment, and a global already noted is not written again. A line
that reads more than 24 (an obfuscator's string table) names the first 24 and
counts the rest; every name is still in the JSONL.

A call that came back `true`, whose result nobody stored, opened a branch:

```lua
if library:IsA("ModuleScript") then
    -- line 20
    print("module ")
end
```

The call keeps its arguments and becomes the `if … then`; the statements
that ran under it are indented, and `end` is placed where the source's block
words close the `if`. A `while <test> do` is drawn the same way, but only the
pass that opened the block is written: later passes test the same condition
again and add nothing. That placement is derived from the source, not
recorded: on a minified one-line script the whole program is the line, so
the guard is left out and the call stands alone. A call result that nobody
stored but that is worth seeing gets a `-- returns <value>` note under it.

A test that came back false is only written when it led somewhere. Its line
is held; if an `elseif` of the chain runs, the `if` and then the `elseif`
are written above that branch:

```lua
-- line 27
if lib:IsA("Folder") then
-- line 29
elseif lib:IsA("ModuleScript") then
    print("module again")
end
```

If the `else` body runs instead, the `if` and a bare `else` come first; a
false test whose chain ran nothing else is a comment (`-- if <test> then
(false)`) so the reader still sees it was asked. Only taken branches appear:
a false `elseif` test is left out, and an `else` is written only when its
body ran.

A source the script compiled with `loadstring` is shown where it was
compiled, one comment line per line of the stage, and then the stage's own
statements are logged and written under the chunk's name. A `ModuleScript`
the script `require`s shows its own `Source` the same way, under
`-- require("ReplicatedStorage.Greeter") ran 5 line(s)` and then the module's
statements under the module's chunk name. A compiled chunk (bytecode handed
to `loadstring`) has no lines to show: it reads as `-- loadstring("...") ran
a compiled chunk, N byte(s)`. An `HttpGet`/`HttpPost`/`request` is written as
`-- GET "url" -> 200`, with the body underneath when it is code (a loader's
next stage) rather than an image or JSON. A body fetched and then loaded is
written once, where it first appeared.

A function the script scheduled reads as the call that scheduled it, with
its statements inside:

```lua
task.spawn(function(...)
    -- line 12
    workspace.Part.Name = "spawned"
end)
```

`task.spawn`, `task.defer`, `task.delay` and the legacy `spawn`/`delay` are
wrapped by the executor surface, so the call is in the log even though it is
a plain Lua call. The wrappers are C closures named like the engine's own
(`iscclosure(task.spawn)` is true, `debug.info(task.spawn, "n")` is `spawn`),
so a script that checks the functions still reads what it expects. As with a
connection, only a body that ran without another thread's events in between
is nested; a body that yields while another handler runs is written in the
order its statements happened. A body still running when the run ends says
so inside the call, rather than reading as an empty function.

An instance `--satisfy` made is noted under the call that needed it
(`-- aimers: made ReplicatedStorage.HandshakeFunction (RemoteFunction) so the
script could continue`), and a class repair sits just above the statement that
uses it, so a reader can tell what the script found from what aimers supplied.
On a minified one-line script the source line is a whole program: handler
parameters are written as `...` (the first `function(` on the line is not the
handler's) and locals take the instance's own name or `vN`. A function the
script assigns appears as a function expression (`getMult32 = function(...)`).

## Executor surface

Scripts written for an executor stop at `loadstring` or `getgenv` otherwise.
With the executor surface on (default), these exist in the scripts' public
table:

- `loadstring` / `load` (compiles with the caller's environment; a C closure,
  so a nativeness check reads it as native rather than as a wrapper), `getgenv`,
  `getrenv`, `getreg`
- `gethui` (the client's `CoreGui`, where a hub parents its own GUI),
  `cloneref` / `clonereference` (answers the instance), `setthreadidentity` /
  `setthreadcontext` (no-ops, as the identity is already the script's)
- `debug` completed to the executor shape: the engine's `info` / `traceback`
  plus `getinfo`, `getupvalue` / `setupvalue` / `getupvalues`, `getmetatable` /
  `setmetatable` (raw, past a locked `__metatable`), `getfenv` / `setfenv`,
  `getregistry`, `sethook` / `gethook` and the memory-category stubs. Each is a
  C closure, so `debug.info(f, "s")` is `[C]` for it, as with a real executor;
  `getlocal` / `setlocal` answer nil, since an executor's own frame sits
  between the script and the level they would name
- `task.spawn`, `task.defer`, `task.delay` and the legacy `spawn` / `delay`,
  wrapped so their bodies are in the log; otherwise the same functions
- `identifyexecutor`, `getexecutorname`, `getexecutorversion`
- `hookfunction` / `replaceclosure`: swaps every visible reference to the old
  function (globals, library tables like `string`, `_G`, `getgenv()`) for the
  new one and answers the old, so the hook can call it; a local captured
  before the hook keeps the function it captured
- `hookmetamethod` on plain tables (installs the field, answers the old one);
  instance metatables are left alone, since their `__namecall` / `__index` are
  the engine's own dispatch
- `clonefunction` (calls through), `newcclosure` (same function back)
- `getrawmetatable` (the real metatable, past `__metatable`), `setrawmetatable`,
  `setreadonly`, `isreadonly`, `islclosure`, `iscclosure`
- signal/detector stubs (`getconnections`, `fireclickdetector`,
  `firetouchinterest`, `fireproximityprompt`, `setclipboard`, `setfpscap`, ...)
- files: `writefile`, `readfile`, `appendfile`, `isfile`, `isfolder`,
  `makefolder`, `delfile`, `delfolder`, `listfiles` — all rooted in
  `aimers-workspace/` next to the working directory; `..` and absolute paths
  are refused
- HTTP: `HttpGet` / `HttpGetAsync` / `httpget`, `HttpPost` / `HttpPostAsync`,
  `request` / `http_request` / `httpRequest`, `http.request`, `syn.*`,
  `Krnl.*`, `fluxus.*` (through `curl`, 20 s cap)
- `game:HttpGet` and friends on any instance, as executors provide them
- `getcustomasset`, `crypt.base64encode` / `base64decode`

`--vanilla` leaves all of this out and runs with the stock engine surface.

## Limits

- No rendering, physics stepping of its own or asset downloads beyond the
  engine's simulated world; `--place` gives the script whatever instances the
  place holds.
- Scripts that wait for something that never exists (`workspace:WaitForChild("Some Game Thing")`)
  yield for the whole run; the log shows the polling.
- `HttpGet` reaches the network. If a script fetches a stage, that stage runs
  and is logged too; the URL, status and body are in the replay. Bodies that
  don't look like Lua (an image, JSON) get the request line only.
- Files the script writes stay in `aimers-workspace/`.
- A callback that yields is not nested in its `:Connect`: its statements are
  in the log, but not contiguously, so the replay writes them in order after
  the connection.
- Calls to plain Lua functions (`hookfunction`, `string.format`) are not
  events; the log has the global read that got the function. `loadstring`,
  `require`, the HTTP functions and the task functions are the exceptions:
  the source they compile, the module they run, the request they make and the
  body they schedule are logged.
- `--satisfy`, `--fire` and `--echo-remotes` supply guesses: the class comes
  from how the script uses an instance, and an echoed remote repeats the
  script's own arguments. Neither is a real server, so a script that checks
  the reply's shape may still stop; the log marks every instance they made.

## Layout

| File | What it does |
|---|---|
| `crates/aimers/src/main.rs` | CLI, client/server DataModel, character, trace lifecycle, output |
| `crates/aimers/src/executor.rs` | the executor globals above |
| `crates/aimers/src/replay.rs` | JSONL to readable replay |
| `crates/rustudio-engine/src/trace.rs` | the sink and the event hooks' formatting |
| `crates/rustudio-engine/src/runtime.rs`, `runtime_routes.rs` | where the hooks are called |
