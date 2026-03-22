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
    parse_pelagos_ps, ContainerConfig, ContainerInspect, ContainerState, HostConfig, ImageInspect,
    MountEntry, NetworkSettings, PortBinding, PsRow,
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

#[allow(clippy::large_enum_variant)] // Run variant is large by necessity (many CLI flags); instantiated once per process.
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
        /// Proxy signals to container process (accepted and ignored).
        #[arg(long = "sig-proxy")]
        sig_proxy: Option<String>,
        /// Working directory inside the container (accepted and ignored).
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        /// Username or UID (accepted and ignored; exec-into handles user).
        #[arg(short = 'u', long = "user")]
        user: Option<String>,
        /// Network mode (accepted and ignored).
        #[arg(long = "network")]
        network: Option<String>,
        /// Use init process (accepted and ignored).
        #[arg(long = "init")]
        init: bool,
        /// Additional labels as key=value (accept repeated --label-file; ignored).
        #[arg(long = "label-file")]
        label_file: Vec<String>,
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
        /// Working directory inside the container.
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
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

    /// Show port mappings for a container.
    Port {
        /// Container name.
        name: String,
    },
    /// Manage Docker contexts (stub — always returns a single default context).
    Context {
        /// Subcommand: ls, inspect, use, create, rm, show, update, export, import.
        sub: String,
        /// Optional arguments (context name, flags, etc.).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    // `docker -v` prints version info; intercept before clap since it's not a subcommand.
    // Check args()[1] specifically: `-v` is only the version flag when it is the *first*
    // and only argument. Any other position means it is a volume-mount flag (-v host:ctr).
    if std::env::args().nth(1).as_deref() == Some("-v") && std::env::args().count() == 2 {
        println!("Docker version 20.10.0, build pelagos");
        process::exit(0);
    }

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
            workdir: _,
            user: _,
            network: _,
            init: _,
            label_file: _,
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
            workdir,
            detach_keys: _,
            name_and_args,
        } => cmd_exec(
            &cfg,
            interactive,
            tty,
            user.as_deref(),
            workdir.as_deref(),
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
        DockerCmd::Context { sub, args: _ } => cmd_context(&sub),
        DockerCmd::Port { name } => cmd_port(&name),
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
/// Returns `None` for `type=volume` mounts (named volumes are not host-path
/// virtiofs shares and are silently skipped — managed by the pelagos runtime).
fn parse_mount_as_volume(mount_spec: &str) -> Option<String> {
    let mut mount_type: Option<&str> = None;
    let mut source = None;
    let mut target = None;
    for part in mount_spec.split(',') {
        if let Some(v) = part.strip_prefix("type=") {
            mount_type = Some(v);
        } else if let Some(v) = part.strip_prefix("source=") {
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
    // Named volumes (type=volume) are not host-path shares; skip them.
    if mount_type == Some("volume") {
        return None;
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

    // devcontainer CLI sends `docker run --sig-proxy=false -a STDOUT -a STDERR` and
    // expects to read "Container started" from the container's own echo via the
    // attached stdout stream. pelagos does not yet support `-a stdout` with --detach
    // (pelagos#117). The `-a` and `--sig-proxy` flags are accepted and ignored.

    let (image, cmd_args) = match image_and_args.split_first() {
        Some((img, rest)) => (img.clone(), rest.to_vec()),
        None => {
            eprintln!("pelagos-docker run: missing image");
            return 1;
        }
    };

    let mut sub: Vec<OsString> = Vec::new();

    // Port forwards are handled by a separate `pelagos port-proxy` process managed
    // by the shim (not by the pelagos daemon).  Do NOT pass --port to the main CLI:
    // the daemon validates that requested ports were registered at startup, which
    // fails when the daemon was started by an earlier `docker ps` without ports.

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

    // Collect bind/volume specs for the cache before we move sub.
    let bind_specs_for_cache: Vec<String> = mounts
        .iter()
        .filter_map(|m| parse_mount_as_volume(m))
        .collect();
    let vol_specs_for_cache: Vec<String> = volumes.clone();
    let labels_for_cache: HashMap<String, String> = label_args
        .iter()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect();

    // Spawn the port proxy before the container run so the listener is ready
    // as soon as the container process starts.  For non-detach runs the proxy
    // is killed when run_pelagos_inherited returns (container has exited).
    // For detach runs the proxy keeps running; its PID is stored in the cache
    // and killed on `docker stop` / `docker rm`.
    let proxy_child: Option<std::process::Child> = if ports.is_empty() {
        None
    } else {
        let mut proxy_cmd = std::process::Command::new(&cfg.pelagos_bin);
        proxy_cmd.args(cfg.pelagos_prefix_args());
        proxy_cmd.arg("port-proxy");
        for p in &ports {
            proxy_cmd.arg("--port").arg(p);
        }
        proxy_cmd.stdin(std::process::Stdio::null());
        proxy_cmd.stdout(std::process::Stdio::null());
        proxy_cmd.stderr(std::process::Stdio::null());
        match proxy_cmd.spawn() {
            Ok(child) => Some(child),
            Err(e) => {
                eprintln!("pelagos-docker: port-proxy spawn failed: {}", e);
                None
            }
        }
    };

    let exit_code = match run_pelagos_inherited(cfg, &sub) {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker run: {}", e);
            1
        }
    };

    // Cache the container metadata so that `docker inspect` can return valid
    // data even after pelagos removes the exited container from its state.
    if exit_code == 0 {
        // For detach runs: container is still running — keep proxy alive and
        // store its PID so docker stop/rm can kill it.
        // For non-detach runs: container has exited — kill proxy immediately.
        let proxy_pid: Option<u32> = if let Some(mut child) = proxy_child {
            if detach {
                let pid = child.id();
                std::mem::forget(child); // detach: don't wait, let it run
                Some(pid)
            } else {
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        } else {
            None
        };

        cache_container(
            &effective_name,
            CachedContainer {
                image: image.clone(),
                labels: labels_for_cache,
                vol_specs: vol_specs_for_cache,
                bind_specs: bind_specs_for_cache,
                started_at: {
                    let secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    format!("{}", secs)
                },
                port_specs: ports.clone(),
                proxy_pid,
            },
        );
    } else if let Some(mut child) = proxy_child {
        // Run failed: kill proxy regardless.
        let _ = child.kill();
        let _ = child.wait();
    }

    exit_code
}

fn cmd_exec(
    cfg: &Config,
    _interactive: bool,
    tty: bool,
    user: Option<&str>,
    workdir: Option<&str>,
    env: &[String],
    name_and_args: &[String],
) -> i32 {
    let (name, cmd_args) = match name_and_args.split_first() {
        Some((n, rest)) => (n.as_str(), rest),
        None => {
            eprintln!("pelagos-docker exec: missing container name");
            return 1;
        }
    };

    // `pelagos exec <container> [cmd...]` — enters running container's namespaces.
    // pelagos exec has no --env flag; inject env vars by prepending `env VAR=VAL …`
    // to the command so the container shell inherits them.
    let mut sub: Vec<OsString> = Vec::new();
    sub.push("exec".into());
    if tty {
        sub.push("-t".into());
    }
    if let Some(u) = user {
        sub.push("--user".into());
        sub.push(u.into());
    }
    if let Some(w) = workdir {
        sub.push("-w".into());
        sub.push(w.into());
    }
    sub.push(name.into());
    // Inject env vars (and HOME if missing) by prepending `env VAR=VAL …` to the command.
    // Docker sets HOME from /etc/passwd; pelagos doesn't, so we fill it in here.
    // (tracked in pelagos#120-followup — HOME not set from passwd)
    let caller_has_home = env.iter().any(|kv| kv.starts_with("HOME="));
    let effective_user = user.unwrap_or("root");
    let home_for_user = if effective_user == "root" || effective_user == "0" {
        Some("/root")
    } else {
        // Non-root: we don't know the home dir without querying /etc/passwd inside
        // the container.  Leave HOME unset for now; file pelagos issue for proper fix.
        None
    };
    let need_env_prefix = !env.is_empty() || (!caller_has_home && home_for_user.is_some());
    if need_env_prefix {
        sub.push("env".into());
        if !caller_has_home {
            if let Some(h) = home_for_user {
                sub.push(format!("HOME={}", h).into());
            }
        }
        for kv in env {
            sub.push(kv.into());
        }
    }
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

    // Do NOT seed known from existing containers at startup.
    //
    // Seeding races with `docker run`: if the container starts between the
    // `docker events` spawn and the first `pelagos ps --all` poll, it lands
    // in `known` before we see it as new, so no start event is ever emitted
    // and devcontainer CLI hangs forever waiting for it.
    //
    // Starting with an empty set means pre-existing containers from previous
    // suites also emit start events on the first poll — but devcontainer CLI
    // filters by `devcontainer.local_folder` label and ignores events for
    // containers it didn't launch, so spurious events are harmless.
    let mut known: HashSet<String> = HashSet::new();

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
    let exit = match run_pelagos_inherited(cfg, &args(&["stop", name])) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker stop: {}", e);
            1
        }
    };
    // Kill port-proxy if one is running for this container.
    kill_proxy_for_container(name);
    exit
}

fn cmd_rm(cfg: &Config, force: bool, name: &str) -> i32 {
    // Kill proxy first so the port is freed before we remove the cache entry.
    kill_proxy_for_container(name);
    let sub = if force {
        args(&["rm", "--force", name])
    } else {
        args(&["rm", name])
    };
    let exit = match run_pelagos_inherited(cfg, &sub) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker rm: {}", e);
            1
        }
    };
    // Remove from shim cache regardless of exit code: if pelagos already cleaned
    // the container up (which it does immediately on exit), rm returns non-zero,
    // but VS Code still expects the cache entry to be gone.
    uncache_container(name);
    exit
}

/// Kill the port-proxy process for `name` if one is recorded in the shim cache.
fn kill_proxy_for_container(name: &str) {
    let cache = load_shim_containers();
    if let Some(entry) = cache.get(name) {
        if let Some(pid) = entry.proxy_pid {
            // Send SIGTERM via `kill` command — avoids a libc dependency.
            let _ = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status();
        }
    }
}

/// Call `pelagos inspect <name>` and return the parsed JSON value.
/// The host `pelagos inspect` delegates to `pelagos container inspect` in the guest.
/// Returns None if the container is not found, the command fails, or it times out.
///
/// A 2-second timeout guards against broken/stale containers: `pelagos inspect`
/// should never take more than a few hundred milliseconds on a healthy VM.  If it
/// does, the container state is unresolvable and the caller gets an empty result
/// rather than a permanent hang.
fn pelagos_container_inspect_json(cfg: &Config, name: &str) -> Option<serde_json::Value> {
    let out = invoke::run_pelagos_timeout(
        cfg,
        &args(&["inspect", name]),
        std::time::Duration::from_secs(2),
    )?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Fetch native pelagos labels for a container via `pelagos container inspect`.
/// Returns an empty map if the container is not found or inspect fails.
fn pelagos_container_labels(cfg: &Config, name: &str) -> HashMap<String, String> {
    pelagos_container_inspect_json(cfg, name)
        .and_then(|v| {
            v.get("labels")?.as_object().map(|obj| {
                obj.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Read the virtiofs share map from vm.mounts so we can translate VM-internal
/// paths (e.g. /mnt/share0/Projects/foo) back to host paths (/Users/cb/Projects/foo).
fn read_vm_share_map() -> Vec<(String, String)> {
    let path = match dirs_home() {
        Some(h) => h.join(".local/share/pelagos/vm.mounts"),
        None => return vec![],
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return vec![],
    };
    let entries: Vec<serde_json::Value> = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    entries
        .into_iter()
        .filter_map(|e| {
            let tag = e.get("tag")?.as_str()?.to_string();
            let host = e.get("host_path")?.as_str()?.to_string();
            Some((tag, host))
        })
        .collect()
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}

/// Translate a VM-internal volume spec like "/mnt/share0/Projects/foo:/workspace"
/// to a host-path spec like "/Users/cb/Projects/foo:/workspace".
fn translate_volume_spec(spec: &str, share_map: &[(String, String)]) -> String {
    let (vm_src, rest) = match spec.split_once(':') {
        Some(p) => p,
        None => return spec.to_string(),
    };
    // Try to match /mnt/<tag>/... → <host_path>/...
    for (tag, host_path) in share_map {
        let prefix = format!("/mnt/{}", tag);
        if let Some(suffix) = vm_src.strip_prefix(&prefix) {
            let host_src = if suffix.is_empty() {
                host_path.clone()
            } else {
                format!("{}{}", host_path.trim_end_matches('/'), suffix)
            };
            return format!("{}:{}", host_src, rest);
        }
    }
    spec.to_string()
}

fn cmd_ps(cfg: &Config, all: bool, quiet: bool, filters: &[String], format: Option<&str>) -> i32 {
    let mut sub = args(&["ps"]);
    if all {
        sub.push("--all".into());
    }
    let out = match invoke::run_pelagos_timeout(cfg, &sub, std::time::Duration::from_secs(5)) {
        Some(o) => o,
        None => {
            eprintln!("pelagos-docker ps: timed out (VM not responding)");
            return 1;
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut entries = parse_pelagos_ps(&stdout);

    // Apply client-side filters (pelagos ps has no --filter support).
    let label_filters: Vec<(&str, &str)> = filters
        .iter()
        .filter_map(|f| {
            let kv = f.strip_prefix("label=")?;
            let eq = kv.find('=')?;
            Some((&kv[..eq], &kv[eq + 1..]))
        })
        .collect();

    if !label_filters.is_empty() {
        entries.retain(|e| {
            let labels = pelagos_container_labels(cfg, &e.name);
            label_filters
                .iter()
                .all(|(k, v)| labels.get(*k).map(|lv| lv == v).unwrap_or(false))
        });
    }

    for f in filters {
        if let Some(val) = f.strip_prefix("name=") {
            entries.retain(|e| e.name.contains(val));
        }
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

    // Load shim cache once for per-container port specs.
    let shim_cache = load_shim_containers();
    // Load virtiofs share map for VM path → host path translation.
    let share_map = read_vm_share_map();

    let mut results: Vec<ContainerInspect> = Vec::new();
    let mut missing = false;

    for name in names {
        if let Some(entry) = entries.iter().find(|e| &e.name == name) {
            // `pelagos container inspect` gives us labels AND volume/bind specs.
            let native = pelagos_container_inspect_json(cfg, name);
            let container_labels: HashMap<String, String> = native
                .as_ref()
                .and_then(|v| {
                    v.get("labels")?.as_object().map(|obj| {
                        obj.iter()
                            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                            .collect()
                    })
                })
                .unwrap_or_default();

            // Collect volume specs from spawn_config, translate VM paths to host paths.
            let vol_specs: Vec<String> = native
                .as_ref()
                .and_then(|v| v.get("spawn_config")?.get("volume")?.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| translate_volume_spec(s, &share_map))
                        .collect()
                })
                .unwrap_or_default();
            let bind_specs: Vec<String> = native
                .as_ref()
                .and_then(|v| v.get("spawn_config")?.get("bind")?.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| translate_volume_spec(s, &share_map))
                        .collect()
                })
                .unwrap_or_default();

            // Build Docker-format Mounts list and HostConfig.Binds from all specs.
            let all_specs: Vec<&str> = vol_specs
                .iter()
                .chain(bind_specs.iter())
                .map(|s| s.as_str())
                .collect();
            let mounts: Vec<MountEntry> = all_specs
                .iter()
                .filter_map(|spec| {
                    let (src, dst) = spec.split_once(':')?;
                    Some(MountEntry {
                        mount_type: "bind".into(),
                        source: src.to_string(),
                        destination: dst.to_string(),
                        mode: String::new(),
                        rw: true,
                        propagation: "rprivate".into(),
                    })
                })
                .collect();
            let binds: Vec<String> = all_specs.iter().map(|s| s.to_string()).collect();

            // Per-container port specs from shim cache (populated by cmd_run -p).
            let port_specs_for_container = shim_cache
                .get(name.as_str())
                .map(|c| c.port_specs.clone())
                .unwrap_or_default();
            let ports = build_ports_map(&port_specs_for_container);
            // Extract started_at from pelagos inspect for lifecycle marker idempotency.
            let started_at = native
                .as_ref()
                .and_then(|v| v.get("started_at")?.as_str())
                .unwrap_or("")
                .to_string();
            results.push(ContainerInspect {
                id: entry.name.clone(),
                name: format!("/{}", entry.name),
                created: started_at.clone(),
                state: ContainerState {
                    running: entry.status == "running",
                    status: entry.status.clone(),
                    started_at,
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
                host_config: HostConfig {
                    binds,
                    port_bindings: ports.clone(),
                },
                mounts,
                network_settings: NetworkSettings { ports },
            });
        } else {
            // Container not in pelagos ps --all.  Check shim cache: pelagos removes
            // exited containers from its state immediately, but Docker retains them
            // until `docker rm`.  VS Code inspects after `docker run` completes and
            // expects to see the container in "exited" state.
            let cache = load_shim_containers();
            if let Some(cached) = cache.get(name.as_str()) {
                let share_map = read_vm_share_map();
                let all_specs: Vec<String> = cached
                    .vol_specs
                    .iter()
                    .chain(cached.bind_specs.iter())
                    .map(|s| translate_volume_spec(s, &share_map))
                    .collect();
                let mounts: Vec<MountEntry> = all_specs
                    .iter()
                    .filter_map(|spec| {
                        let (src, dst) = spec.split_once(':')?;
                        Some(MountEntry {
                            mount_type: "bind".into(),
                            source: src.to_string(),
                            destination: dst.to_string(),
                            mode: String::new(),
                            rw: true,
                            propagation: "rprivate".into(),
                        })
                    })
                    .collect();
                let binds: Vec<String> = all_specs.clone();
                let ports = build_ports_map(&cached.port_specs);
                results.push(ContainerInspect {
                    id: name.clone(),
                    name: format!("/{}", name),
                    created: cached.started_at.clone(),
                    state: ContainerState {
                        running: false,
                        status: "exited".into(),
                        started_at: cached.started_at.clone(),
                    },
                    config: ContainerConfig {
                        image: cached.image.clone(),
                        labels: cached.labels.clone(),
                        user: String::new(),
                        env: vec![],
                        cmd: vec![],
                        working_dir: String::new(),
                        entrypoint: None,
                    },
                    host_config: HostConfig {
                        binds,
                        port_bindings: ports.clone(),
                    },
                    mounts,
                    network_settings: NetworkSettings { ports },
                });
            } else {
                eprintln!("pelagos-docker inspect: container '{}' not found", name);
                missing = true;
            }
        }
    }

    let json = serde_json::to_string_pretty(&results).unwrap();
    println!("{}", json);
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
// Container state cache
//
// pelagos removes exited containers from its in-memory state immediately after
// the main process exits.  Docker, by contrast, retains exited containers until
// explicitly `docker rm`'d.  The VS Code devcontainer extension calls
// `docker inspect <container>` after `docker run` completes and expects to see
// the container in "exited" state.  Without this cache, inspect returns exit
// code 1 ("not found") and VS Code shows "Dev container not found."
//
// The cache is a JSON object at ~/.local/share/pelagos/shim-containers.json,
// keyed by container name.  Entries are written by cmd_run and removed by cmd_rm.
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct CachedContainer {
    image: String,
    labels: HashMap<String, String>,
    vol_specs: Vec<String>,
    bind_specs: Vec<String>,
    started_at: String,
    /// Port specs from `-p HOST:CONTAINER` flags, stored as-is (e.g. "5000:5000").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    port_specs: Vec<String>,
    /// PID of the running `pelagos port-proxy` process for detached containers.
    /// None for non-detach runs (proxy is killed when run exits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proxy_pid: Option<u32>,
}

fn shim_containers_path() -> Option<std::path::PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        std::path::PathBuf::from(xdg).join("pelagos")
    } else if let Ok(home) = std::env::var("HOME") {
        std::path::PathBuf::from(home).join(".local/share/pelagos")
    } else {
        return None;
    };
    Some(base.join("shim-containers.json"))
}

fn load_shim_containers() -> HashMap<String, CachedContainer> {
    let path = match shim_containers_path() {
        Some(p) => p,
        None => return HashMap::new(),
    };
    let s = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&s).unwrap_or_default()
}

fn save_shim_containers(map: &HashMap<String, CachedContainer>) {
    let path = match shim_containers_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(map) {
        let _ = std::fs::write(&path, json);
    }
}

fn cache_container(name: &str, entry: CachedContainer) {
    let mut map = load_shim_containers();
    map.insert(name.to_string(), entry);
    save_shim_containers(&map);
}

fn uncache_container(name: &str) {
    let mut map = load_shim_containers();
    if map.remove(name).is_some() {
        save_shim_containers(&map);
    }
}

// ---------------------------------------------------------------------------
// Port map helpers
// ---------------------------------------------------------------------------

/// Build Docker's NetworkSettings.Ports / HostConfig.PortBindings map from
/// per-container port specs (e.g. `["5000:5000", "8080:80/tcp"]`).
///
/// Docker spec: key is `container_port/proto`; value is a list of host bindings.
fn build_ports_map(port_specs: &[String]) -> HashMap<String, Vec<PortBinding>> {
    let mut map = HashMap::new();
    for spec in port_specs {
        // Strip optional protocol suffix: "5000:5000/tcp" → ("5000:5000", "tcp")
        let (spec_no_proto, proto) = if let Some((s, p)) = spec.rsplit_once('/') {
            (s, p.to_string())
        } else {
            (spec.as_str(), "tcp".to_string())
        };
        // Handle "[host_ip:]host_port:container_port"
        let parts: Vec<&str> = spec_no_proto.splitn(3, ':').collect();
        let (host_port, container_port) = match parts.len() {
            3 => (parts[1], parts[2]),
            2 => (parts[0], parts[1]),
            _ => continue,
        };
        let key = format!("{}/{}", container_port, proto);
        map.entry(key).or_insert_with(Vec::new).push(PortBinding {
            host_ip: "0.0.0.0".into(),
            host_port: host_port.to_string(),
        });
    }
    map
}

/// `docker port <container>` — print port mappings in Docker's text format:
///   5000/tcp -> 0.0.0.0:5000
fn cmd_port(name: &str) -> i32 {
    let cache = load_shim_containers();
    let port_specs = match cache.get(name) {
        Some(c) => c.port_specs.clone(),
        None => {
            eprintln!("Error: No such container: {}", name);
            return 1;
        }
    };
    if port_specs.is_empty() {
        // No ports — Docker prints nothing and exits 0.
        return 0;
    }
    for spec in &port_specs {
        let (spec_no_proto, proto) = if let Some((s, p)) = spec.rsplit_once('/') {
            (s, p.to_string())
        } else {
            (spec.as_str(), "tcp".to_string())
        };
        let parts: Vec<&str> = spec_no_proto.splitn(3, ':').collect();
        let (host_port, container_port) = match parts.len() {
            3 => (parts[1], parts[2]),
            2 => (parts[0], parts[1]),
            _ => continue,
        };
        println!("{}/{} -> 0.0.0.0:{}", container_port, proto, host_port);
    }
    0
}

// ---------------------------------------------------------------------------
// Build / Volume / Network
// ---------------------------------------------------------------------------

/// Parse `FROM <image> [AS <name>]` lines from a Dockerfile and return the
/// external base images that need to be pulled (skips `scratch` and
/// stage-alias forward-references from earlier multi-stage stages).
fn parse_from_images(dockerfile: &str) -> Vec<String> {
    let mut stage_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut images: Vec<String> = Vec::new();
    for line in dockerfile.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !trimmed.to_uppercase().starts_with("FROM ") {
            continue;
        }
        let rest = trimmed[5..].trim();
        let (image_part, alias) = if let Some(idx) = rest.to_uppercase().find(" AS ") {
            (rest[..idx].trim(), Some(rest[idx + 4..].trim()))
        } else {
            (rest, None)
        };
        // Strip optional platform flag: --platform=linux/amd64 <image>
        let image = if image_part.starts_with("--") {
            image_part
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string()
        } else {
            image_part.to_string()
        };
        if let Some(name) = alias {
            stage_names.insert(name.to_lowercase());
        }
        // Skip scratch, build-arg references, and known stage aliases
        if image.is_empty()
            || image == "scratch"
            || image.starts_with('$')
            || stage_names.contains(&image.to_lowercase())
        {
            continue;
        }
        images.push(image);
    }
    images
}

/// Pull a single image using the run-probe mechanism.
/// pelagos has no `image pull` command; the only way to populate the local
/// image cache is to run a container (which triggers an implicit pull).
fn pull_image_probe(cfg: &Config, image: &str) -> i32 {
    // Sanitize the image name into a valid container name (alphanumeric + dash).
    let safe: String = image
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let probe_name = format!("pelagos-docker-pull-{}", &safe[..safe.len().min(40)]);

    let mut sub = args(&["run", "--detach", "--name", &probe_name]);
    sub.push(OsString::from(image));
    sub.push(OsString::from("/bin/true"));

    let out = match run_pelagos(cfg, &sub) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("pelagos-docker build: pull probe for {}: {}", image, e);
            return 1;
        }
    };
    let _ = run_pelagos(cfg, &args(&["stop", &probe_name]));
    let _ = run_pelagos(cfg, &args(&["rm", &probe_name]));
    if out.status.success() {
        0
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        eprintln!(
            "pelagos-docker build: pull {} failed: {}",
            image,
            stderr.trim()
        );
        out.status.code().unwrap_or(1)
    }
}

/// Pull all base images referenced in `dockerfile_path`.
/// `build_args` are substituted into the Dockerfile text before parsing, so
/// `FROM ${_DEV_CONTAINERS_BASE_IMAGE}` (used by the devcontainer features
/// build) resolves to the real registry image.
///
/// Only attempts to pull images that look like registry references (contain a
/// `.` or `/` in the name part) — locally-built tags like
/// `dev_container_feature_content_temp` are skipped.
fn pull_base_images(cfg: &Config, dockerfile_path: &str, build_args: &[String]) -> i32 {
    let raw = match std::fs::read_to_string(dockerfile_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "pelagos-docker build: cannot read Dockerfile {}: {}",
                dockerfile_path, e
            );
            return 1;
        }
    };

    // Build a substitution map from --build-arg KEY=VALUE pairs.
    let mut arg_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for arg in build_args {
        if let Some(eq) = arg.find('=') {
            arg_map.insert(arg[..eq].to_string(), arg[eq + 1..].to_string());
        }
    }

    // Substitute ${VAR} and $VAR references in the Dockerfile text.
    let content = if arg_map.is_empty() {
        raw
    } else {
        let mut s = raw;
        for (k, v) in &arg_map {
            s = s.replace(&format!("${{{}}}", k), v);
            s = s.replace(&format!("${}", k), v);
        }
        s
    };

    for image in parse_from_images(&content) {
        // Skip images that have no dot or slash in the name part — they are
        // locally-built tags (e.g. `dev_container_feature_content_temp`), not
        // registry references. Registry images always include a hostname dot
        // (e.g. `public.ecr.aws/...`) or at minimum a slash (e.g. `library/ubuntu`).
        let name_part = image.split(':').next().unwrap_or(&image);
        if !name_part.contains('.') && !name_part.contains('/') {
            continue;
        }
        eprintln!("pelagos-docker build: pulling base image {}", image);
        let rc = pull_image_probe(cfg, &image);
        if rc != 0 {
            return rc;
        }
    }
    0
}

/// Substitute `--build-arg` values into `FROM` lines in a Dockerfile.
///
/// The devcontainer CLI generates Dockerfiles like:
///   ARG _DEV_CONTAINERS_BASE_IMAGE=some_stage_alias
///   FROM ${_DEV_CONTAINERS_BASE_IMAGE} AS dev_containers_target_stage
/// and passes `--build-arg _DEV_CONTAINERS_BASE_IMAGE=some_stage_alias`.
///
/// pelagos build does not substitute ARG values in FROM lines, so it tries to
/// pull `$_DEV_CONTAINERS_BASE_IMAGE` literally and fails. We preprocess the
/// Dockerfile here so pelagos receives fully-resolved FROM lines.
///
/// Returns the path of a temp file if substitution was needed, or None if the
/// Dockerfile needed no changes (caller uses the original path).
fn preprocess_dockerfile_args(dockerfile_path: &str, build_args: &[String]) -> Option<String> {
    let content = std::fs::read_to_string(dockerfile_path).ok()?;

    // Collect --build-arg values (highest priority).
    let mut arg_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for kv in build_args {
        if let Some((k, v)) = kv.split_once('=') {
            arg_map.insert(k.to_string(), v.to_string());
        }
    }

    // Parse global ARG defaults (before the first FROM) as fallback.
    let mut in_global = true;
    for line in content.lines() {
        let t = line.trim();
        if in_global && t.to_ascii_uppercase().starts_with("FROM") {
            in_global = false;
        }
        if in_global && t.to_ascii_uppercase().starts_with("ARG ") {
            let rest = t[4..].trim();
            if let Some((k, v)) = rest.split_once('=') {
                arg_map
                    .entry(k.trim().to_string())
                    .or_insert_with(|| v.trim().to_string());
            }
        }
    }

    // Skip preprocessing if no FROM line contains a variable reference.
    let needs_subst = content
        .lines()
        .any(|l| l.trim().to_ascii_uppercase().starts_with("FROM") && l.contains('$'));
    if !needs_subst {
        return None;
    }

    // Substitute $VAR and ${VAR} in FROM lines.
    let resolved: String = content
        .lines()
        .map(|line| {
            let mut out = line.to_string();
            if line.trim().to_ascii_uppercase().starts_with("FROM") {
                for (k, v) in &arg_map {
                    out = out.replace(&format!("${{{}}}", k), v);
                    out = out.replace(&format!("${}", k), v);
                }
            }
            out + "\n"
        })
        .collect();

    // Write to a temp file alongside the original Dockerfile.
    let tmp_path = format!("{}.pelagos-resolved", dockerfile_path);
    if std::fs::write(&tmp_path, &resolved).is_err() {
        let tmp_path2 = format!(
            "/tmp/pelagos-dockerfile-{}.resolved",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_millis()
        );
        std::fs::write(&tmp_path2, &resolved).ok()?;
        return Some(tmp_path2);
    }
    Some(tmp_path)
}

fn cmd_build(
    cfg: &Config,
    tag: &str,
    file: &str,
    build_args: &[String],
    no_cache: bool,
    target: Option<&str>,
    context: &str,
) -> i32 {
    // Preprocess: substitute --build-arg values into FROM lines so pelagos
    // receives fully-resolved image/stage references.
    let resolved = preprocess_dockerfile_args(file, build_args);
    let dockerfile = resolved.as_deref().unwrap_or(file);

    // Pull all FROM base images first — pelagos build requires them locally,
    // Docker does this transparently as part of build.
    let rc = pull_base_images(cfg, dockerfile, build_args);
    if rc != 0 {
        if let Some(ref p) = resolved {
            let _ = std::fs::remove_file(p);
        }
        return rc;
    }

    let mut sub: Vec<OsString> = args(&["build", "-t", tag, "-f", dockerfile]);
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
    // the same image. Track as pelagos issue #TBD.
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

/// `docker context` — devcontainer pre-flight check.
///
/// devcontainer CLI calls `docker context ls --format {{json .}}` to find which
/// context to use.  We always have exactly one context (the pelagos VM), so
/// return a single-entry list describing it.  All other subcommands are no-ops.
fn cmd_context(sub: &str) -> i32 {
    match sub {
        "ls" => {
            // One JSON object per line, matching Docker's --format {{json .}} output.
            println!(
                "{}",
                serde_json::json!({
                    "Current": true,
                    "Description": "pelagos VM",
                    "DockerEndpoint": "",
                    "Error": "",
                    "Name": "default",
                    "StackOrchestrator": ""
                })
            );
            0
        }
        "show" => {
            println!("default");
            0
        }
        // inspect / use / create / rm / update / export / import — accept silently.
        _ => 0,
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
