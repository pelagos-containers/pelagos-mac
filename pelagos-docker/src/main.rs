//! pelagos-docker — Docker CLI shim for pelagos-mac.
//!
//! Accepts a subset of Docker CLI arguments and maps them to pelagos commands,
//! enabling the devcontainer CLI to use pelagos-mac as a backend via:
//!
//!   devcontainer --docker-path $(which pelagos-docker) up

mod config;
mod docker_types;
mod invoke;

use std::collections::HashMap;
use std::ffi::OsString;
use std::process;

use clap::{Parser, Subcommand};

use config::Config;
use docker_types::{
    parse_pelagos_ps, ContainerConfig, ContainerInspect, ContainerState, ImageInspect,
    NetworkSettings, PortBinding, PsRow,
};
use invoke::{args, run_pelagos, run_pelagos_inherited};

// ---------------------------------------------------------------------------
// CLI definition (matches Docker's flag names exactly)
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "pelagos-docker", about = "Docker CLI shim for pelagos-mac")]
struct Cli {
    #[command(subcommand)]
    command: DockerCmd,
}

#[derive(Subcommand)]
enum DockerCmd {
    /// Pull an image.
    Pull {
        image: String,
        #[arg(short = 'q', long)]
        quiet: bool,
    },

    /// Run a container.
    Run {
        /// Container name.
        #[arg(long)]
        name: Option<String>,
        /// Run in background.
        #[arg(short = 'd', long)]
        detach: bool,
        /// Bind mount: /host:/container.
        #[arg(short = 'v', long = "volume")]
        volumes: Vec<String>,
        /// --mount type=bind,source=X,target=Y (newer bind-mount syntax).
        #[arg(long = "mount")]
        mounts: Vec<String>,
        /// Environment variable KEY=VALUE.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Port forward host:container.
        #[arg(short = 'p', long = "publish")]
        ports: Vec<String>,
        /// Label KEY=VALUE (stored in sidecar, not forwarded to pelagos).
        #[arg(short = 'l', long = "label")]
        labels: Vec<String>,
        /// Override entrypoint.
        #[arg(long)]
        entrypoint: Option<String>,
        /// Remove container on exit (no-op: pelagos containers persist until rm).
        #[arg(long)]
        rm: bool,
        /// Attach to stdout/stderr (ignored: output is always streamed).
        #[arg(short = 'a', long = "attach")]
        attach: Vec<String>,
        /// Proxy signals to container process (ignored).
        #[arg(long = "sig-proxy")]
        sig_proxy: Option<String>,
        /// Image and optional command+args.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        image_and_args: Vec<String>,
    },

    /// Stream container events (blocks until killed; stub for devcontainer compatibility).
    Events {
        #[arg(long)]
        format: Option<String>,
        #[arg(long = "filter")]
        filters: Vec<String>,
    },

    /// Execute a command in a running container.
    Exec {
        #[arg(short = 'i', long)]
        interactive: bool,
        #[arg(short = 't', long)]
        tty: bool,
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// User to run the command as (passed through to exec-into).
        #[arg(short = 'u', long = "user")]
        user: Option<String>,
        /// Detach keys (ignored).
        #[arg(long = "detach-keys")]
        detach_keys: Option<String>,
        /// Container name followed by command and arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        name_and_args: Vec<String>,
    },

    /// Start one or more stopped containers.
    Start {
        /// One or more container names (Docker accepts multiple).
        #[arg(value_name = "NAME")]
        names: Vec<String>,
        /// Attach STDOUT/STDERR (ignored; pelagos always runs detached).
        #[arg(short = 'a', long)]
        attach: bool,
        /// Attach stdout/stderr before start (ignored).
        #[arg(short = 'i', long)]
        interactive: bool,
    },

    /// Stop a running container.
    Stop { name: String },

    /// Remove a container.
    Rm {
        #[arg(short = 'f', long)]
        force: bool,
        name: String,
    },

    /// List containers.
    Ps {
        #[arg(short = 'a', long)]
        all: bool,
        #[arg(short = 'q', long)]
        quiet: bool,
        #[arg(long = "filter")]
        filters: Vec<String>,
        #[arg(long)]
        format: Option<String>,
    },

    /// Fetch container or image logs.
    Logs {
        #[arg(short = 'f', long)]
        follow: bool,
        name: String,
    },

    /// Return low-level information on a container or image.
    Inspect {
        #[arg(long = "type")]
        inspect_type: Option<String>,
        names: Vec<String>,
    },

    /// Show Docker version information (static stub for tooling compatibility).
    Version {
        #[arg(short = 'f', long)]
        format: Option<String>,
    },

    /// Display system-wide information (static stub for tooling compatibility).
    Info {
        #[arg(short = 'f', long)]
        format: Option<String>,
    },

    /// Build an OCI image from a Dockerfile.
    Build {
        /// Image tag.
        #[arg(short = 't', long)]
        tag: String,
        /// Dockerfile path inside the build context.
        #[arg(short = 'f', long, default_value = "Dockerfile")]
        file: String,
        /// Build argument KEY=VALUE (repeatable).
        #[arg(long = "build-arg")]
        build_args: Vec<String>,
        /// Do not use the cache.
        #[arg(long)]
        no_cache: bool,
        /// Set the target build stage in a multi-stage Dockerfile.
        #[arg(long)]
        target: Option<String>,
        /// Build context path (default: .).
        #[arg(default_value = ".")]
        context: String,
    },

    /// BuildKit stub — not supported; exits 1 so callers fall back to plain build.
    #[command(name = "buildx")]
    Buildx {
        /// Subcommand and args (ignored).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        _args: Vec<String>,
    },

    /// Manage named volumes.
    Volume {
        /// Subcommand: create, ls, rm.
        sub: String,
        /// Volume name.
        name: Option<String>,
        /// Only display volume names.
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,
    },

    /// Manage named networks.
    Network {
        /// Subcommand: create, ls, rm, inspect.
        sub: String,
        /// Network name.
        name: Option<String>,
        /// Only display network IDs.
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,
    },

    /// Copy files between the host and a running container.
    /// Use `container:path` syntax for the container side.
    Cp {
        /// Source: `container:path` or local path.
        src: String,
        /// Destination: `container:path` or local path.
        dst: String,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pelagos-docker: {}", e);
            process::exit(1);
        }
    };

    let exit_code = match cli.command {
        DockerCmd::Pull { image, quiet } => cmd_pull(&cfg, &image, quiet),
        DockerCmd::Run {
            name,
            detach,
            volumes,
            mounts,
            env,
            ports,
            labels,
            entrypoint,
            rm: _,
            attach: _,
            sig_proxy: _,
            image_and_args,
        } => cmd_run(
            &cfg,
            RunOpts {
                name,
                detach,
                volumes,
                mounts,
                env,
                ports,
                label_args: labels,
                entrypoint,
                image_and_args,
            },
        ),
        DockerCmd::Exec {
            interactive,
            tty,
            env,
            user,
            detach_keys: _,
            name_and_args,
        } => cmd_exec(
            &cfg,
            interactive,
            tty,
            user.as_deref(),
            &env,
            &name_and_args,
        ),
        DockerCmd::Start {
            names,
            attach: _,
            interactive: _,
        } => cmd_start(&cfg, &names),
        DockerCmd::Stop { name } => cmd_stop(&cfg, &name),
        DockerCmd::Rm { force, name } => cmd_rm(&cfg, force, &name),
        DockerCmd::Ps {
            all,
            quiet,
            filters,
            format,
        } => cmd_ps(&cfg, all, quiet, &filters, format.as_deref()),
        DockerCmd::Logs { follow, name } => cmd_logs(&cfg, follow, &name),
        DockerCmd::Inspect {
            inspect_type,
            names,
        } => cmd_inspect(&cfg, inspect_type.as_deref(), &names),
        DockerCmd::Version { format } => cmd_version_with_format(format.as_deref()),
        DockerCmd::Info { format: _ } => cmd_info(),
        DockerCmd::Events { .. } => cmd_events(),
        DockerCmd::Build {
            tag,
            file,
            build_args,
            no_cache,
            target,
            context,
        } => cmd_build(
            &cfg,
            &tag,
            &file,
            &build_args,
            no_cache,
            target.as_deref(),
            &context,
        ),
        DockerCmd::Buildx { .. } => 1,
        DockerCmd::Volume { sub, name, quiet } => cmd_volume(&cfg, &sub, name.as_deref(), quiet),
        DockerCmd::Network { sub, name, quiet } => cmd_network(&cfg, &sub, name.as_deref(), quiet),
        DockerCmd::Cp { src, dst } => cmd_cp(&cfg, &src, &dst),
    };

    process::exit(exit_code);
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

fn cmd_pull(cfg: &Config, image: &str, quiet: bool) -> i32 {
    // pelagos has no pull-only command; run a no-op container to populate the cache.
    let mut sub = args(&["run", "--detach", "--name", "pelagos-docker-pull-probe"]);
    sub.push(OsString::from(image));
    sub.push(OsString::from("/bin/true"));

    let out = match run_pelagos(cfg, &sub) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("pelagos-docker pull: {}", e);
            return 1;
        }
    };

    // Stop the probe container immediately.
    let _ = run_pelagos(cfg, &args(&["stop", "pelagos-docker-pull-probe"]));
    let _ = run_pelagos(cfg, &args(&["rm", "pelagos-docker-pull-probe"]));

    if !quiet {
        let stderr = String::from_utf8_lossy(&out.stderr);
        for line in stderr.lines() {
            if !line.starts_with('[') {
                eprintln!("{}", line);
            }
        }
    }
    if out.status.success() {
        0
    } else {
        out.status.code().unwrap_or(1)
    }
}

struct RunOpts {
    name: Option<String>,
    detach: bool,
    volumes: Vec<String>,
    mounts: Vec<String>,
    env: Vec<String>,
    ports: Vec<String>,
    label_args: Vec<String>,
    entrypoint: Option<String>,
    image_and_args: Vec<String>,
}

/// Parse `--mount type=bind,source=X,target=Y[,...]` into a `-v X:Y` string.
fn parse_mount_as_volume(mount_spec: &str) -> Option<String> {
    let mut source = None;
    let mut target = None;
    for part in mount_spec.split(',') {
        if let Some(v) = part.strip_prefix("source=") {
            source = Some(v);
        } else if let Some(v) = part.strip_prefix("src=") {
            source = Some(v);
        } else if let Some(v) = part.strip_prefix("target=") {
            target = Some(v);
        } else if let Some(v) = part.strip_prefix("dst=") {
            target = Some(v);
        } else if let Some(v) = part.strip_prefix("destination=") {
            target = Some(v);
        }
    }
    match (source, target) {
        (Some(s), Some(t)) => Some(format!("{}:{}", s, t)),
        _ => None,
    }
}

fn cmd_run(cfg: &Config, opts: RunOpts) -> i32 {
    let RunOpts {
        name,
        detach,
        volumes,
        mounts,
        env,
        ports,
        label_args,
        entrypoint,
        image_and_args,
    } = opts;
    let (image, cmd_args) = match image_and_args.split_first() {
        Some((img, rest)) => (img.clone(), rest.to_vec()),
        None => {
            eprintln!("pelagos-docker run: missing image");
            return 1;
        }
    };

    let mut sub: Vec<OsString> = Vec::new();

    // Port forwards go before "run" as global flags on pelagos.
    for p in &ports {
        sub.push("--port".into());
        sub.push(p.into());
    }

    sub.push("run".into());

    // If no --name was given, generate one so labels can be stored correctly.
    // pelagos auto-assigns names like "pelagos-N" which we don't know in advance.
    let effective_name: String = name.clone().unwrap_or_else(|| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_millis();
        format!("dc-{}", ts)
    });
    sub.push("--name".into());
    sub.push(effective_name.as_str().into());
    if detach {
        sub.push("--detach".into());
    }
    for v in &volumes {
        sub.push("-v".into());
        sub.push(v.into());
    }
    // Convert --mount type=bind,source=X,target=Y to -v X:Y.
    for m in &mounts {
        if let Some(vol) = parse_mount_as_volume(m) {
            sub.push("-v".into());
            sub.push(vol.into());
        }
    }
    for e in &env {
        sub.push("-e".into());
        sub.push(e.into());
    }
    // Pass labels natively to pelagos run via --label KEY=VALUE (must be before image).
    for kv in &label_args {
        sub.push("--label".into());
        sub.push(kv.into());
    }
    if let Some(ep) = &entrypoint {
        // Prepend entrypoint to the command args.
        let mut new_args = vec![ep.clone()];
        new_args.extend(cmd_args.iter().cloned());
        sub.push(image.as_str().into());
        for a in new_args {
            sub.push(a.into());
        }
    } else {
        sub.push(image.as_str().into());
        for a in &cmd_args {
            sub.push(a.into());
        }
    }

    let exit_code = match run_pelagos_inherited(cfg, &sub) {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker run: {}", e);
            1
        }
    };
    exit_code
}

fn cmd_exec(
    cfg: &Config,
    _interactive: bool,
    tty: bool,
    user: Option<&str>,
    _env: &[String],
    name_and_args: &[String],
) -> i32 {
    let (name, cmd_args) = match name_and_args.split_first() {
        Some((n, rest)) => (n.as_str(), rest),
        None => {
            eprintln!("pelagos-docker exec: missing container name");
            return 1;
        }
    };

    // `pelagos exec-into <container> [cmd...]` — enters running container's namespaces.
    let mut sub: Vec<OsString> = Vec::new();
    sub.push("exec-into".into());
    if tty {
        sub.push("-t".into());
    }
    if let Some(u) = user {
        sub.push("--user".into());
        sub.push(u.into());
    }
    sub.push(name.into());
    for a in cmd_args {
        sub.push(a.into());
    }

    match run_pelagos_inherited(cfg, &sub) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker exec: {}", e);
            1
        }
    }
}

fn cmd_version_with_format(format: Option<&str>) -> i32 {
    // devcontainer calls `docker version --format {{.Server.Version}}` to get a bare version.
    if let Some(fmt) = format {
        if fmt.contains("Server.Version") || fmt.contains("server.version") {
            println!("20.10.0");
            return 0;
        }
    }
    cmd_version()
}

fn cmd_version() -> i32 {
    println!(
        "{}",
        serde_json::json!({
            "Client": {
                "Version": "20.10.0",
                "ApiVersion": "1.41",
                "Os": "darwin",
                "Arch": "arm64",
                "GoVersion": "go1.21.0",
                "GitCommit": "pelagos",
                "BuildTime": "2025-01-01T00:00:00.000000000+00:00"
            },
            "Server": {
                "Engine": {
                    "Version": "20.10.0",
                    "ApiVersion": "1.41",
                    "MinAPIVersion": "1.12",
                    "GitCommit": "pelagos",
                    "GoVersion": "go1.21.0",
                    "Os": "linux",
                    "Arch": "arm64",
                    "BuildTime": "2025-01-01T00:00:00.000000000+00:00"
                }
            }
        })
    );
    0
}

fn cmd_info() -> i32 {
    println!(
        "{}",
        serde_json::json!({
            "ID": "pelagos",
            "ServerVersion": "20.10.0",
            "OperatingSystem": "Alpine Linux (VM)",
            "OSType": "linux",
            "Architecture": "aarch64",
            "NCPU": 1,
            "MemTotal": 1073741824,
            "Containers": 0,
            "ContainersRunning": 0,
            "ContainersPaused": 0,
            "ContainersStopped": 0,
            "Images": 0,
            "DockerRootDir": "/var/lib/docker",
            "HttpProxy": "",
            "HttpsProxy": "",
            "NoProxy": "",
            "Labels": [],
            "ExperimentalBuild": false,
            "RegistryConfig": {
                "AllowNondistributableArtifactsCIDRs": [],
                "AllowNondistributableArtifactsHostnames": [],
                "InsecureRegistryCIDRs": [],
                "IndexConfigs": {},
                "Mirrors": []
            }
        })
    );
    0
}

/// `docker events` implementation. Polls `pelagos ps` every 500ms and emits a
/// JSON start event for each newly-seen container. Blocks until killed.
///
/// devcontainer CLI uses `docker events --filter event=start` to detect when
/// a container has started. Without real-time event support, we poll ps and
/// emit synthetic start events when new containers appear.
fn cmd_events() -> i32 {
    use std::collections::HashSet;
    use std::io::Write;

    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(_) => {
            // Can't locate pelagos; block forever so devcontainer CLI can kill us.
            let mut buf = String::new();
            let _ = std::io::stdin().read_line(&mut buf);
            return 0;
        }
    };

    let mut known: HashSet<String> = HashSet::new();
    // Seed with existing containers so we only emit events for NEW ones.
    if let Ok(out) = run_pelagos(&cfg, &args(&["ps", "--all"])) {
        let s = String::from_utf8_lossy(&out.stdout);
        for e in parse_pelagos_ps(&s) {
            known.insert(e.name);
        }
    }

    let stdout = std::io::stdout();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Ok(out) = run_pelagos(&cfg, &args(&["ps", "--all"])) {
            let s = String::from_utf8_lossy(&out.stdout);
            for e in parse_pelagos_ps(&s) {
                if known.insert(e.name.clone()) {
                    // New container — emit a start event.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    // Include native pelagos labels in Actor.Attributes so devcontainer CLI
                    // can verify the event belongs to the container it launched.
                    let mut attrs: HashMap<String, String> =
                        pelagos_container_labels(&cfg, &e.name);
                    attrs.insert("image".into(), e.image.clone());
                    attrs.insert("name".into(), e.name.clone());
                    let event = serde_json::json!({
                        "status": "start",
                        "id": e.name,
                        "from": e.image,
                        "Type": "container",
                        "Action": "start",
                        "Actor": {
                            "ID": e.name,
                            "Attributes": attrs
                        },
                        "scope": "local",
                        "time": now,
                        "timeNano": now * 1_000_000_000u64
                    });
                    let mut lock = stdout.lock();
                    let _ = writeln!(lock, "{}", event);
                    let _ = lock.flush();
                }
            }
        }
    }
}

fn cmd_start(cfg: &Config, names: &[String]) -> i32 {
    let mut exit = 0i32;
    for name in names {
        match run_pelagos_inherited(cfg, &args(&["start", name])) {
            Ok(s) => {
                let code = s.code().unwrap_or(1);
                if code != 0 {
                    exit = code;
                }
            }
            Err(e) => {
                eprintln!("pelagos-docker start: {}", e);
                exit = 1;
            }
        }
    }
    exit
}

fn cmd_stop(cfg: &Config, name: &str) -> i32 {
    match run_pelagos_inherited(cfg, &args(&["stop", name])) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker stop: {}", e);
            1
        }
    }
}

fn cmd_rm(cfg: &Config, force: bool, name: &str) -> i32 {
    let sub = if force {
        args(&["rm", "--force", name])
    } else {
        args(&["rm", name])
    };
    match run_pelagos_inherited(cfg, &sub) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker rm: {}", e);
            1
        }
    }
}

/// Fetch native pelagos labels for a container via `pelagos inspect <name>`.
/// Returns an empty map if the container is not found or inspect fails.
fn pelagos_container_labels(cfg: &Config, name: &str) -> HashMap<String, String> {
    let out = match run_pelagos(cfg, &args(&["inspect", name])) {
        Ok(o) if o.status.success() => o,
        _ => return HashMap::new(),
    };
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| {
            v.get("labels")?.as_object().map(|obj| {
                obj.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn cmd_ps(cfg: &Config, all: bool, quiet: bool, filters: &[String], format: Option<&str>) -> i32 {
    let mut sub = args(&["ps"]);
    if all {
        sub.push("--all".into());
    }
    // Pass label= filters to pelagos natively; handle name= filters ourselves below.
    for f in filters {
        if f.starts_with("label=") {
            sub.push("--filter".into());
            sub.push(f.as_str().into());
        }
    }
    let out = match run_pelagos(cfg, &sub) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("pelagos-docker ps: {}", e);
            return 1;
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut entries = parse_pelagos_ps(&stdout);

    // Apply remaining filters that pelagos doesn't handle (name=).
    for f in filters {
        if let Some(val) = f.strip_prefix("name=") {
            entries.retain(|e| e.name.contains(val));
        }
        // label= already forwarded to pelagos; other types silently ignored.
    }

    // -q: output only container IDs (we use names as IDs).
    if quiet {
        for e in &entries {
            println!("{}", e.name);
        }
        return 0;
    }

    let emit_json = format
        .map(|f| f.contains("json") || f == "{{json .}}")
        .unwrap_or(false);

    if emit_json {
        // Emit one JSON object per line (Docker's --format json behaviour).
        for entry in &entries {
            let row = PsRow {
                id: entry.name.clone(),
                names: entry.name.clone(),
                image: entry.image.clone(),
                status: entry.status.clone(),
                state: entry.status.clone(),
            };
            println!("{}", serde_json::to_string(&row).unwrap());
        }
    } else {
        // Plain tabular output.
        if !entries.is_empty() {
            println!("{:<30} {:<12} IMAGE", "NAMES", "STATUS");
            for e in &entries {
                println!("{:<30} {:<12} {}", e.name, e.status, e.image);
            }
        }
    }
    0
}

fn cmd_logs(cfg: &Config, follow: bool, name: &str) -> i32 {
    let sub = if follow {
        args(&["logs", "--follow", name])
    } else {
        args(&["logs", name])
    };
    match run_pelagos_inherited(cfg, &sub) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker logs: {}", e);
            1
        }
    }
}

fn cmd_inspect(cfg: &Config, inspect_type: Option<&str>, names: &[String]) -> i32 {
    if names.is_empty() {
        eprintln!("pelagos-docker inspect: requires at least one name");
        return 1;
    }

    match inspect_type {
        Some("image") => return cmd_inspect_image(cfg, names),
        Some("container") => return cmd_inspect_container(cfg, names),
        _ => {}
    }

    // Auto-detect: try container first; if none found, treat as image.
    // devcontainer CLI calls `docker inspect <image>` without --type.
    let sub = args(&["ps", "--all"]);
    let known_containers: Vec<String> = run_pelagos(cfg, &sub)
        .ok()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout).into_owned();
            parse_pelagos_ps(&s).into_iter().map(|e| e.name).collect()
        })
        .unwrap_or_default();

    let all_are_containers = names.iter().all(|n| known_containers.contains(n));
    if all_are_containers {
        cmd_inspect_container(cfg, names)
    } else {
        cmd_inspect_image(cfg, names)
    }
}

fn cmd_inspect_container(cfg: &Config, names: &[String]) -> i32 {
    // Fetch full ps --all output to find each requested container.
    let sub = args(&["ps", "--all"]);
    let out = match run_pelagos(cfg, &sub) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("pelagos-docker inspect: {}", e);
            return 1;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let entries = parse_pelagos_ps(&stdout);

    // Load port forwards from state file.
    let port_map = load_port_map();

    let mut results: Vec<ContainerInspect> = Vec::new();
    let mut missing = false;

    for name in names {
        if let Some(entry) = entries.iter().find(|e| &e.name == name) {
            let container_labels = pelagos_container_labels(cfg, name);
            let ports = build_ports_map(name, &port_map);
            results.push(ContainerInspect {
                id: entry.name.clone(),
                name: format!("/{}", entry.name),
                state: ContainerState {
                    running: entry.status == "running",
                    status: entry.status.clone(),
                },
                config: ContainerConfig {
                    image: entry.image.clone(),
                    labels: container_labels,
                    user: String::new(),
                    env: vec![],
                    cmd: vec![],
                    working_dir: String::new(),
                    entrypoint: None,
                },
                mounts: vec![],
                network_settings: NetworkSettings { ports },
            });
        } else {
            eprintln!("pelagos-docker inspect: container '{}' not found", name);
            missing = true;
        }
    }

    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    if missing {
        1
    } else {
        0
    }
}

fn cmd_inspect_image(cfg: &Config, names: &[String]) -> i32 {
    // Minimal stub: just confirm the image exists in pelagos's cache by running
    // `pelagos image ls` (or attempting a no-op run). devcontainer only checks
    // existence (exit code), not the JSON content for images.
    //
    // Emit a minimal ImageInspect array so JSON consumers don't crash.
    let results: Vec<ImageInspect> = names
        .iter()
        .map(|n| ImageInspect {
            id: n.clone(),
            repo_tags: vec![n.clone()],
            config: docker_types::ImageConfig {
                user: String::new(),
                env: vec![],
                cmd: vec![],
                working_dir: String::new(),
                entrypoint: None,
                labels: HashMap::new(),
            },
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&results).unwrap());

    // Verify the images actually exist via a quick ps-based probe.
    // For now: trust the caller; return 0 (image existence check is best-effort).
    let _ = cfg; // suppress unused warning
    0
}

// ---------------------------------------------------------------------------
// Port map helpers
// ---------------------------------------------------------------------------

/// Load the running daemon's port forwards from the state file.
fn load_port_map() -> Vec<(u16, u16)> {
    let base = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        std::path::PathBuf::from(xdg).join("pelagos")
    } else if let Ok(home) = std::env::var("HOME") {
        std::path::PathBuf::from(home).join(".local/share/pelagos")
    } else {
        return Vec::new();
    };
    let path = base.join("vm.ports");
    let s = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    // vm.ports is a JSON array: [{"host_port":N,"container_port":M}, ...]
    let arr: Vec<serde_json::Value> = serde_json::from_str(&s).unwrap_or_default();
    arr.iter()
        .filter_map(|v| {
            let hp = v["host_port"].as_u64()? as u16;
            let cp = v["container_port"].as_u64()? as u16;
            Some((hp, cp))
        })
        .collect()
}

/// Build Docker's NetworkSettings.Ports map for a container.
/// We have no per-container port info, so we expose all daemon-level forwards.
fn build_ports_map(_container: &str, port_map: &[(u16, u16)]) -> HashMap<String, Vec<PortBinding>> {
    let mut map = HashMap::new();
    for (host_port, container_port) in port_map {
        let key = format!("{}/tcp", container_port);
        map.entry(key).or_insert_with(Vec::new).push(PortBinding {
            host_ip: "0.0.0.0".into(),
            host_port: host_port.to_string(),
        });
    }
    map
}

// ---------------------------------------------------------------------------
// Build / Volume / Network
// ---------------------------------------------------------------------------

fn cmd_build(
    cfg: &Config,
    tag: &str,
    file: &str,
    build_args: &[String],
    no_cache: bool,
    target: Option<&str>,
    context: &str,
) -> i32 {
    let mut sub: Vec<OsString> = args(&["build", "-t", tag, "-f", file]);
    for arg in build_args {
        sub.push("--build-arg".into());
        sub.push(arg.into());
    }
    if no_cache {
        sub.push("--no-cache".into());
    }
    // --target is accepted but not forwarded: pelagos build does not yet support
    // multi-stage target selection. The devcontainer CLI always makes
    // dev_containers_target_stage the final stage, so omitting --target produces
    // the same image. Re-wire once pelagos build gains --target support.
    let _ = target;
    sub.push(context.into());
    match run_pelagos_inherited(cfg, &sub) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker build: {}", e);
            1
        }
    }
}

fn cmd_volume(cfg: &Config, sub: &str, name: Option<&str>, quiet: bool) -> i32 {
    let mut a: Vec<OsString> = args(&["volume", sub]);
    if let Some(n) = name {
        a.push(n.into());
    }
    if sub == "ls" && quiet {
        // Capture output and print only the name column (skip header).
        let out = match run_pelagos(cfg, &a) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("pelagos-docker volume ls: {}", e);
                return 1;
            }
        };
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().skip(1) {
            let name_col = line.split_whitespace().last().unwrap_or("").to_string();
            if !name_col.is_empty() {
                println!("{}", name_col);
            }
        }
        return if out.status.success() { 0 } else { 1 };
    }
    match run_pelagos_inherited(cfg, &a) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker volume: {}", e);
            1
        }
    }
}

fn cmd_cp(cfg: &Config, src: &str, dst: &str) -> i32 {
    // Pass src and dst verbatim; the host `pelagos cp` command handles
    // the `container:path` parsing.
    let a: Vec<OsString> = args(&["cp", src, dst]);
    match run_pelagos_inherited(cfg, &a) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker cp: {}", e);
            1
        }
    }
}

fn cmd_network(cfg: &Config, sub: &str, name: Option<&str>, _quiet: bool) -> i32 {
    let mut a: Vec<OsString> = args(&["network", sub]);
    // `docker network create <name>` auto-assigns a subnet; pelagos requires one explicitly.
    // Pick 10.88.<hash>.0/24 derived from the name so repeated calls are idempotent.
    if sub == "create" {
        if let Some(n) = name {
            let hash: u8 = n.bytes().fold(0u8, |acc, b| acc.wrapping_add(b));
            let subnet = format!("10.88.{}.0/24", hash);
            a.push("--subnet".into());
            a.push(subnet.into());
            a.push(n.into());
        }
    } else if let Some(n) = name {
        a.push(n.into());
    }
    match run_pelagos_inherited(cfg, &a) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker network: {}", e);
            1
        }
    }
}
