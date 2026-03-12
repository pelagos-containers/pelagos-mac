//! pelagos-docker — Docker CLI shim for pelagos-mac.
//!
//! Accepts a subset of Docker CLI arguments and maps them to pelagos commands,
//! enabling the devcontainer CLI to use pelagos-mac as a backend via:
//!
//!   devcontainer --docker-path $(which pelagos-docker) build
//!
//! # Known limitation
//!
//! `docker exec` is not supported: it requires exec-into-a-running-container-by-name,
//! which the pelagos runtime does not yet provide. devcontainer post-create lifecycle
//! hooks will not work until this is implemented upstream. See issue #56.

mod config;
mod docker_types;
mod invoke;
mod labels;

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
        /// Environment variable KEY=VALUE.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Port forward host:container.
        #[arg(short = 'p', long = "publish")]
        ports: Vec<String>,
        /// Label KEY=VALUE (stored in sidecar, not forwarded to pelagos).
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Override entrypoint.
        #[arg(long)]
        entrypoint: Option<String>,
        /// Remove container on exit (no-op: pelagos containers persist until rm).
        #[arg(long)]
        rm: bool,
        /// Image and optional command+args.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        image_and_args: Vec<String>,
    },

    /// Execute a command in a running container (not yet supported).
    Exec {
        #[arg(short = 'i', long)]
        interactive: bool,
        #[arg(short = 't', long)]
        tty: bool,
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Container name and command.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        name_and_args: Vec<String>,
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
        #[arg(long)]
        all: bool,
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
            env,
            ports,
            labels,
            entrypoint,
            rm: _,
            image_and_args,
        } => cmd_run(
            &cfg,
            RunOpts {
                name,
                detach,
                volumes,
                env,
                ports,
                label_args: labels,
                entrypoint,
                image_and_args,
            },
        ),
        DockerCmd::Exec {
            interactive: _,
            tty: _,
            env: _,
            name_and_args: _,
        } => cmd_exec_stub(),
        DockerCmd::Stop { name } => cmd_stop(&cfg, &name),
        DockerCmd::Rm { force, name } => cmd_rm(&cfg, force, &name),
        DockerCmd::Ps {
            all,
            filters,
            format,
        } => cmd_ps(&cfg, all, &filters, format.as_deref()),
        DockerCmd::Logs { follow, name } => cmd_logs(&cfg, follow, &name),
        DockerCmd::Inspect {
            inspect_type,
            names,
        } => cmd_inspect(&cfg, inspect_type.as_deref(), &names),
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
    env: Vec<String>,
    ports: Vec<String>,
    label_args: Vec<String>,
    entrypoint: Option<String>,
    image_and_args: Vec<String>,
}

fn cmd_run(cfg: &Config, opts: RunOpts) -> i32 {
    let RunOpts {
        name,
        detach,
        volumes,
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

    if let Some(ref n) = name {
        sub.push("--name".into());
        sub.push(n.into());
    }
    if detach {
        sub.push("--detach".into());
    }
    for v in &volumes {
        sub.push("-v".into());
        sub.push(v.into());
    }
    for e in &env {
        sub.push("-e".into());
        sub.push(e.into());
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

    // Store labels in sidecar (resolve container name).
    if !label_args.is_empty() {
        let container_name = name.clone().unwrap_or_else(|| image.clone());
        let mut label_map = HashMap::new();
        for kv in &label_args {
            if let Some((k, v)) = kv.split_once('=') {
                label_map.insert(k.to_string(), v.to_string());
            } else {
                label_map.insert(kv.clone(), String::new());
            }
        }
        let _ = labels::set(&container_name, label_map);
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

fn cmd_exec_stub() -> i32 {
    eprintln!(
        "pelagos-docker: 'docker exec' is not yet supported.\n\
         It requires exec-into-a-running-container-by-name, which the pelagos \
         runtime does not yet provide. See issue #56 for tracking."
    );
    1
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
    labels::remove(name);
    match run_pelagos_inherited(cfg, &sub) {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pelagos-docker rm: {}", e);
            1
        }
    }
}

fn cmd_ps(cfg: &Config, all: bool, filters: &[String], format: Option<&str>) -> i32 {
    let mut sub = args(&["ps"]);
    if all {
        sub.push("--all".into());
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

    // Apply --filter name=<value> filters.
    for f in filters {
        if let Some(val) = f.strip_prefix("name=") {
            entries.retain(|e| e.name.contains(val));
        }
        // Other filter types (status=, etc.) silently ignored for now.
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

    match inspect_type.unwrap_or("container") {
        "image" => cmd_inspect_image(cfg, names),
        _ => cmd_inspect_container(cfg, names),
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
            let container_labels = labels::get(name);
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
