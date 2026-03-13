//! Docker JSON output types.
//!
//! Only the fields devcontainer CLI actually reads are populated.
//! Everything else is omitted or stubbed with defaults.

use std::collections::HashMap;

use serde::Serialize;

// ---------------------------------------------------------------------------
// Container inspect
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerInspect {
    pub id: String,
    pub name: String,
    pub state: ContainerState,
    pub config: ContainerConfig,
    pub host_config: HostConfig,
    pub mounts: Vec<MountEntry>,
    pub network_settings: NetworkSettings,
}

/// Docker HostConfig — only the fields devcontainer CLI reads.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct HostConfig {
    /// Bind mounts in "host:container[:options]" format.
    pub binds: Vec<String>,
}

/// One entry in the container's Mounts list.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct MountEntry {
    #[serde(rename = "Type")]
    pub mount_type: String,
    pub source: String,
    pub destination: String,
    pub mode: String,
    #[serde(rename = "RW")]
    pub rw: bool,
    pub propagation: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerState {
    pub status: String,
    pub running: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerConfig {
    pub image: String,
    pub labels: HashMap<String, String>,
    pub user: String,
    pub env: Vec<String>,
    pub cmd: Vec<String>,
    pub working_dir: String,
    pub entrypoint: Option<Vec<String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct NetworkSettings {
    /// Map of "port/proto" → list of host bindings.
    pub ports: HashMap<String, Vec<PortBinding>>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PortBinding {
    pub host_ip: String,
    pub host_port: String,
}

// ---------------------------------------------------------------------------
// Image inspect
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageInspect {
    pub id: String,
    pub repo_tags: Vec<String>,
    pub config: ImageConfig,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageConfig {
    pub user: String,
    pub env: Vec<String>,
    pub cmd: Vec<String>,
    pub working_dir: String,
    pub entrypoint: Option<Vec<String>>,
    pub labels: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// `docker ps --format json` row
// ---------------------------------------------------------------------------

/// One row emitted by `docker ps --format '{{json .}}'`.
/// devcontainer filters by name and checks Status.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PsRow {
    pub id: String,
    pub names: String,
    pub image: String,
    pub status: String,
    pub state: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse pelagos ps output into (name, image, status) triples.
///
/// pelagos ps header:
///   NAME   STATUS   PID   ROOTFS   COMMAND   HEALTH   STARTED
pub fn parse_pelagos_ps(output: &str) -> Vec<PsEntry> {
    let mut entries = Vec::new();
    for line in output.lines() {
        // Skip header, log lines ([...]), blank lines, and "No containers found." messages.
        if line.is_empty()
            || line.starts_with('[')
            || line.to_uppercase().starts_with("NAME")
            || line.to_lowercase().starts_with("no containers")
        {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        entries.push(PsEntry {
            name: cols[0].to_string(),
            status: cols[1].to_string(),
            image: cols[3].to_string(),
        });
    }
    entries
}

#[derive(Debug, Clone)]
pub struct PsEntry {
    pub name: String,
    pub status: String,
    pub image: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const PS_OUTPUT: &str = "\
NAME                   STATUS        PID  ROOTFS                                       COMMAND       HEALTH      STARTED
mybox                  running       123  public.ecr.aws/docker/library/alpine:latest  /bin/sh         -         1 minute ago
oldbox                 exited        456  public.ecr.aws/docker/library/ubuntu:24.04   /bin/bash       -         5 minutes ago
";

    #[test]
    fn parse_ps_two_entries() {
        let entries = parse_pelagos_ps(PS_OUTPUT);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "mybox");
        assert_eq!(entries[0].status, "running");
        assert_eq!(entries[1].name, "oldbox");
        assert_eq!(entries[1].status, "exited");
    }

    #[test]
    fn parse_ps_skips_log_lines() {
        let output = "[INFO] some log\nNAME  STATUS  PID  ROOTFS  CMD  HEALTH  STARTED\nbox1  running  1  alpine  sh  -  now\n";
        let entries = parse_pelagos_ps(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "box1");
    }

    #[test]
    fn parse_ps_empty() {
        assert!(parse_pelagos_ps("").is_empty());
        assert!(parse_pelagos_ps("No containers found.\n").is_empty());
        assert!(
            parse_pelagos_ps("No containers found. Use 'pelagos run' to start one.\n").is_empty()
        );
    }
}
