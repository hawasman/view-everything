//! `ve mcp serve` run as a subprocess and driven with JSON-RPC lines: an agent is refused with an
//! approval code until approved for a Module, answers are capped and continue by cursor, bad
//! calls are structured refusals, and with no app running it reads the saved Index for an
//! approved agent.
#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ve_core::{CategoryMap, EntryMeta, Index, SyncState};
use ve_engine::net::{InterfaceCounters, Net, Protocol, Socket, SocketHolder, State};
use ve_engine::perf::{
    Backend, Counters, CpuCounters, CpuTimes, Memory, Perf, ProcessCounters, ProcessCountersTable, ProcessId, Reading,
};
use ve_engine::socket::{self, Claim};
use ve_engine::apps::Sources;
use ve_engine::{Config, Engine, Phase};
use ve_scan::VolumeInfo;

/// A running `ve mcp serve`, its socket at `socket` and its data and config folders in `home`.
struct Mcp {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(socket: &Path, home: &Path, agent: &str) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ve"))
            .args(["mcp", "serve"])
            .env(socket::PATH_ENV, socket)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_CONFIG_HOME", home.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut mcp = Mcp {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            next_id: 0,
        };
        let init = mcp.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": agent, "version": "1" } }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
        mcp.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        mcp
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
    }

    /// Sends a request and reads its response (the server answers in order).
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params }));
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
        assert_eq!(reply["id"], self.next_id, "{reply}");
        reply
    }

    /// Calls a tool and returns its result (`isError`, `content`, `structuredContent`).
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({ "name": tool, "arguments": arguments }))["result"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ve(args: &[&str], socket: &Path, home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ve"))
        .args(args)
        .env(socket::PATH_ENV, socket)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .unwrap()
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

fn refusal(result: &Value) -> &Value {
    assert_eq!(result["isError"], true, "{result}");
    &result["structuredContent"]["refusal"]
}

fn answer(result: &Value) -> &Value {
    assert_eq!(result["isError"], false, "{result}");
    &result["structuredContent"]
}

fn paths(answer: &Value) -> Vec<String> {
    answer["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_owned())
        .collect()
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An engine with no Locations, its recorder reading `backend` only when the test ticks it.
fn engine_reading(backend: impl Backend) -> Engine {
    let mut engine = Engine::start(
        Config {
            index_path: None,
            mounted: Vec::new(),
            volumes: Vec::new(),
            remotes: Vec::new(),
            intervals: Default::default(),
            categories: CategoryMap::builtin(),
            live_helper: false,
        },
        Arc::new(|| {}),
    );
    engine.perf = Perf::manual(Box::new(backend), Arc::new(|| {}));
    engine.net = Net::new(&engine.perf);
    engine
}

/// Serves `engine` on a socket in `home`, as the running app does.
fn host(engine: Engine, home: &Path) -> (Arc<Engine>, PathBuf) {
    let engine = Arc::new(engine);
    let path = home.join("run").join("engine.sock");
    let Claim::Host(listener) = socket::claim(&path).unwrap() else {
        panic!("nothing else listens there")
    };
    socket::serve(listener, engine.clone(), Arc::new(|_| {}));
    (engine, path)
}

/// Calls `tool`, which is refused with an approval code for `module`, and approves that code
/// as the user would from a shell.
fn approve(mcp: &mut Mcp, tool: &str, module: &str, socket: &Path, home: &Path) {
    let refused = mcp.call(tool, json!({}));
    assert_eq!(refusal(&refused)["reason"], "approval_pending", "{refused}");
    let code = refusal(&refused)["code"].as_str().unwrap().to_owned();
    let listed = ve(&["mcp", "--list-requests"], socket, home);
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert!(listed.contains(&format!("{code}\t{module}\t")), "{listed}");
    let approved = ve(&["mcp", "--approve", &code], socket, home);
    assert!(approved.status.success(), "{}", String::from_utf8_lossy(&approved.stderr));
}

/// A machine of 250 processes, process `pid` taking `pid` times process 1's CPU and the
/// lowest PIDs the most memory; the whole machine half busy, a quarter of its memory used.
/// `eth0` moves a hundred times what `wlan0` does; each process but the last holds a connection,
/// and a listening socket of a process that cannot be read makes 250.
#[derive(Default)]
struct Machine {
    step: u64,
}

impl Backend for Machine {
    fn read(&mut self) -> Counters {
        self.step += 1;
        Counters {
            cpu: Reading::Value(CpuCounters {
                total: CpuTimes {
                    busy: self.step * 500_000,
                    total: self.step * 1_000_000,
                },
                cores: Vec::new(),
            }),
            core_kinds: Arc::from([]),
            memory: Reading::Value(Memory {
                total: 4000,
                used: 1000,
                cache: 1000,
                free: 2000,
                available: Reading::Unsupported,
                swap: Reading::Unsupported,
            }),
            disks: Reading::Unsupported,
            network: Reading::Value(vec![
                InterfaceCounters {
                    name: "wlan0".into(),
                    received: self.step * 10,
                    sent: self.step,
                },
                InterfaceCounters {
                    name: "eth0".into(),
                    received: self.step * 1000,
                    sent: self.step * 100,
                },
            ]),
        }
    }

    fn processes(&mut self, _io: bool) -> Reading<ProcessCountersTable> {
        Reading::Value(ProcessCountersTable {
            processes: (1..=250u32)
                .map(|pid| ProcessCounters {
                    id: ProcessId { pid, start: 100 },
                    ppid: 1,
                    name: format!("proc-{pid:03}").into(),
                    cpu_ticks: u64::from(pid) * self.step * 10,
                    rss: Some(u64::from(251 - pid) * 4096),
                    io: None,
                })
                .collect(),
            boot_time: Some(1_700_000_000),
            ticks_per_second: 100,
        })
    }

    fn connections(&mut self) -> Reading<Vec<Socket>> {
        let mut sockets: Vec<Socket> = (1..250u16)
            .map(|pid| Socket {
                protocol: Protocol::Tcp,
                local: format!("127.0.0.1:{}", 10_000 + pid).parse().unwrap(),
                remote: "93.184.216.34:443".parse().unwrap(),
                state: State::Established,
                holder: SocketHolder::Pid(u32::from(pid)),
            })
            .collect();
        sockets.push(Socket {
            protocol: Protocol::Tcp,
            local: "0.0.0.0:22".parse().unwrap(),
            remote: "0.0.0.0:0".parse().unwrap(),
            state: State::Listen,
            holder: SocketHolder::Unknown,
        });
        Reading::Value(sockets)
    }
}

/// The PIDs of an answer's rows.
fn pids(answer: &Value) -> Vec<u64> {
    answer["rows"].as_array().unwrap().iter().map(|r| r["pid"].as_u64().unwrap()).collect()
}

#[test]
fn an_agent_is_refused_until_approved_then_answered_in_capped_pages() {
    let (engine_data, fixture, home) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    for n in 0..250 {
        // One file clearly the largest on disk (small files all take one block).
        let size = if n == 249 { 64 * 1024 } else { 1 };
        fs::write(fixture.path().join(format!("report-{n:03}.txt")), "x".repeat(size)).unwrap();
    }
    let volume = VolumeInfo {
        mount: fixture.path().to_path_buf(),
        identity: "uuid:fixture".into(),
        fs_type: "ext4".into(),
        label: "fixture".into(),
        removable: false,
        network: false,
        read_only: false,
    };
    let engine = Arc::new(Engine::start(
        Config {
            index_path: Some(engine_data.path().join("index.bin")),
            mounted: vec![volume.clone()],
            volumes: vec![volume],
            remotes: Vec::new(),
            intervals: Default::default(),
            categories: CategoryMap::builtin(),
            live_helper: false,
        },
        Arc::new(|| {}),
    ));
    wait_for("the first Sync", || {
        !engine.is_busy() && matches!(engine.shared.status.lock().phase, Phase::Idle) && !engine.shared.tasks.finished().is_empty()
    });
    let socket_path = home.path().join("run").join("engine.sock");
    let Claim::Host(listener) = socket::claim(&socket_path).unwrap() else {
        panic!("nothing else listens there")
    };
    socket::serve(listener, engine.clone(), Arc::new(|_| {}));
    let mut mcp = Mcp::start(&socket_path, home.path(), "test-agent");

    let tools = mcp.request("tools/list", json!({}))["result"]["tools"].clone();
    let names: Vec<_> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "files.find",
            "files.top",
            "files.locations",
            "files.summary",
            "performance.summary",
            "performance.processes",
            "network.interfaces",
            "network.connections",
            "apps.programs",
            "apps.startup"
        ]
    );
    assert!(tools.as_array().unwrap().iter().all(|t| t["annotations"]["readOnlyHint"] == true));

    // First contact: refused with a code, before the call itself is looked at.
    let refused = mcp.call("files.find", json!({ "query": "ext:txt" }));
    let code = refusal(&refused)["code"].as_str().unwrap().to_owned();
    assert_eq!(refusal(&refused)["reason"], "approval_pending");
    assert!(
        text(&refused).starts_with(&format!("approval pending, code {code}")),
        "{}",
        text(&refused)
    );
    assert!(text(&refused).contains(&format!("ve mcp --approve {code}")), "{}", text(&refused));

    let listed = ve(&["mcp", "--list-requests"], &socket_path, home.path());
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert!(listed.contains(&format!("{code}\tFiles\tin 10 min\ttest-agent")), "{listed}");
    let approved = ve(&["mcp", "--approve", &code.to_lowercase()], &socket_path, home.path());
    assert!(approved.status.success(), "{}", String::from_utf8_lossy(&approved.stderr));
    assert_eq!(String::from_utf8_lossy(&approved.stdout), "Approved: test-agent may read Files\n");

    // More than the cap asked for: 200 rows, and a cursor to the rest.
    let first = mcp.call("files.find", json!({ "query": "ext:txt", "limit": 500 }));
    let first = answer(&first).clone();
    assert_eq!((first["total"].as_u64(), first["limit"].as_u64()), (Some(250), Some(200)));
    assert_eq!(first["source"], "running app");
    assert!(first["on_disk_size"].as_u64().unwrap() > 0, "{first}");
    let mut seen = paths(&first);
    assert_eq!(seen.len(), 200);
    let cursor = first["next_cursor"].as_str().unwrap();
    let rest = mcp.call("files.find", json!({ "query": "ext:txt", "limit": 500, "cursor": cursor }));
    let rest = answer(&rest).clone();
    assert_eq!(rest["next_cursor"], Value::Null);
    seen.extend(paths(&rest));
    assert_eq!(seen.iter().collect::<HashSet<_>>().len(), 250, "pages neither overlap nor skip");
    // The default page is 50 rows.
    let top = mcp.call("files.top", json!({ "files": true }));
    let top = answer(&top);
    assert_eq!(paths(top).len(), 50);
    assert!(paths(top)[0].ends_with("report-249.txt"), "largest first: {:?}", &paths(top)[..3]);
    assert_eq!(top["next_cursor"], "50");

    // Bad calls are refusals the agent can correct.
    assert_eq!(refusal(&mcp.call("files.find", json!({ "query": "size:" })))["reason"], "bad_query");
    let outside = mcp.call("files.top", json!({ "path": "/nowhere/at/all" }));
    assert_eq!(refusal(&outside)["reason"], "bad_path");
    assert_eq!(refusal(&mcp.call("files.delete", json!({})))["reason"], "unknown_tool");
    assert_eq!(
        refusal(&mcp.call("files.find", json!({ "query": "x", "rm": true })))["reason"],
        "bad_arguments"
    );

    // Revoked: refused again.
    engine
        .permissions()
        .revoke(ve_engine::permissions::Kind::Agent, "test-agent")
        .unwrap();
    assert_eq!(refusal(&mcp.call("files.summary", json!({})))["reason"], "approval_pending");
    engine.shutdown();
}

#[test]
fn with_no_app_running_an_approved_agent_reads_the_saved_index() {
    let home = tempfile::tempdir().unwrap();
    let mut index = Index::new();
    let folder = EntryMeta {
        is_dir: true,
        ..Default::default()
    };
    let vol = index.add_volume(Path::new("/w"), "ext4", "uuid:w", folder, SyncState::Walk);
    let root = index.volume(vol).unwrap().root;
    let docs = index.insert(root, "docs", folder);
    for name in ["a.pdf", "b.pdf", "c.txt"] {
        let file = EntryMeta {
            logical: 1000,
            on_disk: 4096,
            ..Default::default()
        };
        index.insert(docs, name, file);
    }
    index.mark_synced(vol, 1_791_646_413, None);
    let data = home.path().join("data").join("view-everything");
    fs::create_dir_all(&data).unwrap();
    index.save(&data.join("index.bin")).unwrap();
    fs::write(data.join("agent-keys.json"), r#"{ "approved-agent": "k1" }"#).unwrap();
    let config = home.path().join("config").join("view-everything");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("settings.toml"),
        "version = 1\n\n[[permissions]]\nkind = \"agent\"\nsubject = \"approved-agent\"\ngranted_at = 1791646413\nmodule = \"Files\"\nkey = \"k1\"\n",
    )
    .unwrap();
    let nobody = home.path().join("nobody.sock");

    let mut mcp = Mcp::start(&nobody, home.path(), "approved-agent");
    let found = mcp.call("files.find", json!({ "query": "ext:pdf", "limit": 1 }));
    let found = answer(&found);
    assert_eq!(found["source"], "saved Index file");
    assert_eq!(paths(found), ["/w/docs/a.pdf"]);
    assert_eq!((found["total"].as_u64(), found["next_cursor"].as_str()), (Some(2), Some("1")));
    let summary = mcp.call("files.summary", json!({ "path": "/w/docs" }));
    assert_eq!(
        (answer(&summary)["files"].as_u64(), answer(&summary)["on_disk_size"].as_u64()),
        (Some(3), Some(12288))
    );
    let locations = mcp.call("files.locations", json!({}));
    let location = &answer(&locations)["rows"][0];
    assert_eq!((location["path"].as_str(), &location["online"]), (Some("/w"), &Value::Null));
    assert_eq!(
        refusal(&mcp.call("files.find", json!({ "query": "is:offline" })))["reason"],
        "bad_query"
    );
    let unread = mcp.call("performance.summary", json!({}));
    assert_eq!(refusal(&unread)["reason"], "not_running", "the recorder runs in the app");
    let unread = mcp.call("network.connections", json!({}));
    assert_eq!(refusal(&unread)["reason"], "not_running");
    let unread = mcp.call("apps.programs", json!({}));
    assert_eq!(refusal(&unread)["reason"], "not_running");

    // Same name, no key on file for it here: a new key, which the settings do not approve.
    fs::remove_file(data.join("agent-keys.json")).unwrap();
    let mut stranger = Mcp::start(&nobody, home.path(), "approved-agent");
    let refused = stranger.call("files.find", json!({ "query": "pdf" }));
    assert_eq!(refusal(&refused)["reason"], "not_approved");
    assert!(text(&refused).contains("not running"), "{}", text(&refused));
}

#[test]
fn performance_is_refused_until_approved_then_answered_from_the_recorder() {
    let home = tempfile::tempdir().unwrap();
    let engine = engine_reading(Machine::default());
    let t0 = chrono::Utc::now().timestamp() - 10;
    for t in t0..=t0 + 3 {
        engine.perf.tick(t);
    }
    let (engine, socket_path) = host(engine, home.path());
    let mut mcp = Mcp::start(&socket_path, home.path(), "perf-agent");

    approve(&mut mcp, "performance.summary", "Performance", &socket_path, home.path());
    let summary = mcp.call("performance.summary", json!({ "from": t0, "to": t0 + 3 }));
    let summary = answer(&summary);
    assert_eq!(summary["source"], "running app");
    // The first reading only primes the next: three seconds recorded.
    assert_eq!((summary["seconds"].as_u64(), summary["cpu"].as_f64()), (Some(3), Some(0.5)));
    assert_eq!(summary["memory"].as_f64(), Some(0.25));
    assert_eq!(
        (&summary["top_process"]["pid"], &summary["top_process"]["name"]),
        (&json!(250), &json!("proc-250"))
    );

    // Live lists every process: 200 rows at most, busiest first, then the rest by cursor.
    let first = mcp.call("performance.processes", json!({ "limit": 500 }));
    let first = answer(&first).clone();
    assert_eq!((first["total"].as_u64(), first["limit"].as_u64()), (Some(250), Some(200)));
    assert_eq!(pids(&first)[..3], [250, 249, 248]);
    let rest = mcp.call(
        "performance.processes",
        json!({ "limit": 500, "cursor": first["next_cursor"] }),
    );
    let rest = answer(&rest).clone();
    assert_eq!(rest["next_cursor"], Value::Null);
    let all: HashSet<u64> = pids(&first).into_iter().chain(pids(&rest)).collect();
    assert_eq!(all.len(), 250, "pages neither overlap nor skip");
    let by_memory = mcp.call("performance.processes", json!({ "sort": "memory", "limit": 1 }));
    assert_eq!(pids(answer(&by_memory)), [1]);
    assert_eq!(answer(&by_memory)["rows"][0]["memory"], 250 * 4096);
    // A past second keeps the top 200.
    let past = mcp.call("performance.processes", json!({ "at": t0 + 3 }));
    assert_eq!(answer(&past)["total"], 200);

    let gap = mcp.call("performance.processes", json!({ "at": t0 - 100 }));
    assert_eq!(refusal(&gap)["reason"], "no_sample");
    let backwards = mcp.call("performance.summary", json!({ "from": t0 + 3, "to": t0 }));
    assert_eq!(refusal(&backwards)["reason"], "bad_arguments");
    let unknown = mcp.call("performance.processes", json!({ "kill": true }));
    assert_eq!(refusal(&unknown)["reason"], "bad_arguments");

    // Approving Performance granted nothing else, and revoking it refuses again.
    assert_eq!(refusal(&mcp.call("files.locations", json!({})))["reason"], "approval_pending");
    engine.agents().revoke("perf-agent", "Performance").unwrap();
    assert_eq!(
        refusal(&mcp.call("performance.summary", json!({})))["reason"],
        "approval_pending"
    );
    engine.shutdown();
}

#[test]
fn network_is_refused_until_approved_then_answered_from_the_recorder() {
    let home = tempfile::tempdir().unwrap();
    let engine = engine_reading(Machine::default());
    let t0 = chrono::Utc::now().timestamp() - 10;
    for t in t0..=t0 + 3 {
        engine.perf.tick(t);
    }
    let (engine, socket_path) = host(engine, home.path());
    let mut mcp = Mcp::start(&socket_path, home.path(), "net-agent");

    approve(&mut mcp, "network.interfaces", "Network", &socket_path, home.path());
    let interfaces = mcp.call("network.interfaces", json!({ "from": t0, "to": t0 + 3, "limit": 1 }));
    let interfaces = answer(&interfaces).clone();
    // Distinct connections among those the seconds kept: the top 200 of each.
    assert_eq!(
        (interfaces["seconds"].as_u64(), interfaces["connections"].as_u64()),
        (Some(3), Some(200))
    );
    assert_eq!(interfaces["total"], 2);
    assert_eq!(
        interfaces["rows"][0],
        json!({ "name": "eth0", "received": 3000, "sent": 300, "last_seen": t0 + 3 }),
        "busiest first"
    );
    let rest = mcp.call(
        "network.interfaces",
        json!({ "from": t0, "to": t0 + 3, "cursor": interfaces["next_cursor"] }),
    );
    assert_eq!(answer(&rest)["rows"][0]["name"], "wlan0");
    assert_eq!(answer(&rest)["next_cursor"], Value::Null);

    // Live lists every connection: 200 at most, open conversations first, then the rest.
    let first = mcp.call("network.connections", json!({ "limit": 500 }));
    let first = answer(&first).clone();
    assert_eq!((first["total"].as_u64(), first["limit"].as_u64()), (Some(250), Some(200)));
    let row = &first["rows"][0];
    assert_eq!(
        (&row["protocol"], &row["state"], &row["remote"], &row["holder"]),
        (&json!("TCP"), &json!("established"), &json!("93.184.216.34:443"), &json!("process"))
    );
    assert_eq!(row["process"], format!("proc-{:03}", row["pid"].as_u64().unwrap()));
    let rest = mcp.call("network.connections", json!({ "limit": 500, "cursor": first["next_cursor"] }));
    let rest = answer(&rest).clone();
    assert_eq!((rest["rows"].as_array().unwrap().len(), &rest["next_cursor"]), (50, &Value::Null));
    let listening = &rest["rows"][49];
    assert_eq!(
        (&listening["state"], &listening["remote"], &listening["holder"], &listening["pid"]),
        (&json!("listening"), &Value::Null, &json!("unknown"), &Value::Null)
    );
    // A past second keeps the top 200.
    let past = mcp.call("network.connections", json!({ "at": t0 + 3 }));
    assert_eq!(answer(&past)["total"], 200);
    let gap = mcp.call("network.connections", json!({ "at": t0 - 100 }));
    assert_eq!(refusal(&gap)["reason"], "no_sample");

    // Approving Network granted nothing else, and revoking it refuses again.
    assert_eq!(
        refusal(&mcp.call("performance.summary", json!({})))["reason"],
        "approval_pending"
    );
    engine.agents().revoke("net-agent", "Network").unwrap();
    assert_eq!(
        refusal(&mcp.call("network.connections", json!({})))["reason"],
        "approval_pending"
    );
    engine.shutdown();
}

#[test]
fn apps_is_refused_until_approved_then_answered_with_on_disk_sizes() {
    let (engine_data, fixture, home) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let write = |path: &str, text: &str| {
        let path = fixture.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    // A Steam game in the indexed Location, a pacman package and an /opt folder that are not.
    let steamapps = "user/.local/share/Steam/steamapps";
    write(
        &format!("{steamapps}/appmanifest_70.acf"),
        "\"AppState\"\n{\n\t\"appid\"\t\t\"70\"\n\t\"name\"\t\t\"Half-Life\"\n\t\"installdir\"\t\t\"HL\"\n}\n",
    );
    write(&format!("{steamapps}/common/HL/hl.bin"), &"x".repeat(64 * 1024));
    write("system/var/lib/pacman/local/vim-9.1-1/desc", "%NAME%\nvim\n\n%VERSION%\n9.1-1\n");
    fs::create_dir_all(fixture.path().join("system/opt/tool")).unwrap();
    for (file, name) in [("backup.desktop", "Backup"), ("chat.desktop", "Chat")] {
        write(
            &format!("user/.config/autostart/{file}"),
            &format!("[Desktop Entry]\nType=Application\nName={name}\nExec={} --background\n", name.to_lowercase()),
        );
    }
    let volume = VolumeInfo {
        mount: fixture.path().to_path_buf(),
        identity: "uuid:apps".into(),
        fs_type: "ext4".into(),
        label: "apps".into(),
        removable: false,
        network: false,
        read_only: false,
    };
    let engine = Engine::start(
        Config {
            index_path: Some(engine_data.path().join("index.bin")),
            mounted: vec![volume.clone()],
            volumes: vec![volume],
            remotes: Vec::new(),
            intervals: Default::default(),
            categories: CategoryMap::builtin(),
            live_helper: false,
        },
        Arc::new(|| {}),
    );
    engine.apps.set_sources(Some(Sources::new(
        fixture.path().join("system"),
        Some(fixture.path().join("user")),
    )));
    let (engine, socket_path) = host(engine, home.path());
    let mut mcp = Mcp::start(&socket_path, home.path(), "apps-agent");

    approve(&mut mcp, "apps.startup", "Apps", &socket_path, home.path());
    // Sized once the Location is synced and its Owners detected.
    let mut programs = Value::Null;
    wait_for("Half-Life's size", || {
        programs = answer(&mcp.call("apps.programs", json!({}))).clone();
        programs["rows"][0]["on_disk_size"].as_u64().is_some()
    });
    assert_eq!((programs["total"].as_u64(), programs["sized"].as_u64()), (Some(3), Some(1)));
    let game = &programs["rows"][0];
    assert_eq!(
        (&game["name"], &game["kind"], &game["source"], &game["id"]),
        (&json!("Half-Life"), &json!("Game"), &json!("Steam"), &json!("70"))
    );
    assert!(game["on_disk_size"].as_u64().unwrap() >= 64 * 1024, "{game}");
    assert_eq!(game["roots"][0]["on_disk_size"], game["on_disk_size"]);
    // The unsized ones after it, by name, with no size rather than 0.
    let rest: Vec<_> = programs["rows"].as_array().unwrap()[1..]
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["on_disk_size"].clone()))
        .collect();
    assert_eq!(rest, [("tool", Value::Null), ("vim", Value::Null)]);
    let vim = mcp.call("apps.programs", json!({ "name": "VI" }));
    assert_eq!(answer(&vim)["rows"][0]["version"], "9.1-1");
    assert_eq!(answer(&vim)["total"], 1);

    let first = mcp.call("apps.startup", json!({ "limit": 1 }));
    let first = answer(&first).clone();
    assert_eq!((first["total"].as_u64(), first["next_cursor"].as_str()), (Some(2), Some("1")));
    assert_eq!(
        (&first["rows"][0]["name"], &first["rows"][0]["command"], &first["rows"][0]["scope"]),
        (&json!("Backup"), &json!("backup --background"), &json!("User"))
    );
    let rest = mcp.call("apps.startup", json!({ "limit": 1, "cursor": "1" }));
    assert_eq!(answer(&rest)["rows"][0]["name"], "Chat");
    assert_eq!(answer(&rest)["next_cursor"], Value::Null);
    let bad = mcp.call("apps.startup", json!({ "cursor": "soon" }));
    assert_eq!(refusal(&bad)["reason"], "bad_arguments");

    // Approving Apps granted nothing else, and revoking it refuses again.
    assert_eq!(refusal(&mcp.call("network.interfaces", json!({})))["reason"], "approval_pending");
    engine.agents().revoke("apps-agent", "Apps").unwrap();
    assert_eq!(refusal(&mcp.call("apps.programs", json!({})))["reason"], "approval_pending");
    engine.shutdown();
}
