//! Bounded network plumbing for the executable development cell.
//!
//! Linux remains the network implementation. This module only creates veth
//! pairs, assigns the host gateways, and installs narrowly named nftables rules
//! for outbound forwarding/NAT. Android owns `eth0` and `eth1` and its normal
//! networking services inside the private namespace. Each side is one
//! `ip -batch` and the rules are one `nft` transaction, so a cell start spawns
//! three helpers.
//!
//! There is one link per kind of host uplink, as a phone has one network per
//! radio: `eth0` stands for Wi-Fi (unmetered) and `eth1` for mobile data
//! (metered); Android's connectivity overlay gives each its transport. The
//! host decides which uplink each link's traffic leaves through and plugs a
//! link out while its uplink is gone, so Android switches networks the way it
//! does on a phone, without dropping the connections of the one that stays.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::CellSpec;
use crate::development::DevelopmentError;

/// One veth pair: the host end's name prefix and address, Android's end.
struct Link {
    prefix: &'static str,
    host_cidr: &'static str,
    cell_cidr: &'static str,
    peer: &'static str,
}

/// Wi-Fi first: `adb` and the host's own services reach Android on its
/// address.
const LINKS: [Link; 2] = [
    Link {
        prefix: "dlh",
        host_cidr: "10.177.0.1/30",
        cell_cidr: "10.177.0.2/32",
        peer: "eth0",
    },
    Link {
        prefix: "dlm",
        host_cidr: "10.177.0.5/30",
        cell_cidr: "10.177.0.6/32",
        peer: "eth1",
    },
];
const IPV4_FORWARD_PATH: &str = "/proc/sys/net/ipv4/ip_forward";
const IPV4_FORWARD_STATE_FILE: &str = "host-ipv4-forward.before";
const NETNS_DIR: &str = "/run/netns";
const NET_CLASS_DIR: &str = "/sys/class/net";

#[derive(Clone, Debug, Eq, PartialEq)]
struct NetworkNames {
    namespace: String,
    host_interfaces: [String; 2],
    filter_table: String,
    nat_table: String,
}

impl NetworkNames {
    fn for_spec(spec: &CellSpec) -> Self {
        let uid = spec.host_uid;
        Self {
            namespace: format!("droidloom-u{uid}"),
            host_interfaces: LINKS.map(|link| format!("{}{uid}", link.prefix)),
            filter_table: format!("droidloom_u{uid}"),
            nat_table: format!("droidloom_u{uid}_nat"),
        }
    }
}

/// Live host-owned networking objects for one development cell.
pub(crate) struct DevelopmentNetwork {
    names: NetworkNames,
    runtime_dir: PathBuf,
    resources: Vec<NetworkResource>,
    previous_ipv4_forward: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NetworkResource {
    Namespace,
    HostInterface(usize),
    FilterTable,
    NatTable,
    UfwForwardRule(usize),
}

impl DevelopmentNetwork {
    /// Create the private namespace and its outbound kernel paths.
    pub(crate) fn create(spec: &CellSpec) -> Result<Self, DevelopmentError> {
        let mut network = Self {
            names: NetworkNames::for_spec(spec),
            runtime_dir: spec.runtime_dir.clone(),
            resources: Vec::new(),
            previous_ipv4_forward: None,
        };

        if let Err(error) = network.setup() {
            if let Err(cleanup_error) = network.teardown() {
                eprintln!(
                    "droidloom-supervisor: network setup failed: {error}; cleanup also failed: {cleanup_error}"
                );
            }
            return Err(error);
        }
        Ok(network)
    }

    pub(crate) fn namespace(&self) -> &str {
        &self.names.namespace
    }

    fn setup(&mut self) -> Result<(), DevelopmentError> {
        self.setup_links()?;
        self.enable_forwarding()?;
        self.setup_rules()?;
        self.setup_ufw_forwarding()
    }

    fn setup_ufw_forwarding(&mut self) -> Result<(), DevelopmentError> {
        // A separate nftables accept verdict cannot override UFW's later drop.
        // Add only our exact outbound source/interface rule to its user chain.
        // It is transient and never modifies UFW's persistent configuration.
        if ufw_chain_exists()? {
            for index in 0..LINKS.len() {
                run("iptables", ufw_rule_arguments("-I", &self.names, index))?;
                self.resources.push(NetworkResource::UfwForwardRule(index));
            }
        }
        Ok(())
    }

    fn setup_links(&mut self) -> Result<(), DevelopmentError> {
        let NetworkNames {
            namespace,
            host_interfaces,
            ..
        } = &self.names;
        // Each peer is created inside the namespace under its Android name,
        // so it never appears on the host under a temporary name.
        let mut script = format!("netns add {namespace}\n");
        let mut cell_script = String::from("link set lo up\n");
        for (link, host) in LINKS.iter().zip(host_interfaces) {
            let (peer, cidr) = (link.peer, link.host_cidr);
            script.push_str(&format!(
                "link add {host} type veth peer name {peer} netns {namespace}\n\
                 address add {cidr} dev {host}\n\
                 link set {host} up\n"
            ));
            cell_script.push_str(&format!("link set {peer} up\n"));
        }
        let host = run_script("ip", ["-batch", "-"], &script);
        // A batch stops at its first failing line; own whatever it created.
        if namespace_exists(namespace) {
            self.resources.push(NetworkResource::Namespace);
        }
        for (index, interface) in host_interfaces.iter().enumerate() {
            if link_exists(interface) {
                self.resources.push(NetworkResource::HostInterface(index));
            }
        }
        host?;
        run_script("ip", ["-n", namespace, "-batch", "-"], &cell_script)
    }

    fn enable_forwarding(&mut self) -> Result<(), DevelopmentError> {
        let forwarding = fs::read_to_string(IPV4_FORWARD_PATH)
            .map_err(|source| io_error("read host IPv4 forwarding state", source))?;
        fs::write(self.forwarding_state_path(), forwarding.as_bytes())
            .map_err(|source| io_error("record host IPv4 forwarding state", source))?;
        self.previous_ipv4_forward = Some(forwarding.clone());
        if forwarding.trim() != "1" {
            fs::write(IPV4_FORWARD_PATH, "1\n")
                .map_err(|source| io_error("enable host IPv4 forwarding", source))?;
        }
        Ok(())
    }

    /// Install both tables in one nftables transaction: all of it or nothing.
    fn setup_rules(&mut self) -> Result<(), DevelopmentError> {
        let NetworkNames {
            host_interfaces,
            filter_table,
            nat_table,
            ..
        } = &self.names;
        let (mut input, mut forward, mut nat) = (String::new(), String::new(), String::new());
        for (index, (link, host)) in LINKS.iter().zip(host_interfaces).enumerate() {
            let cell = link.cell_cidr;
            // adb and the host's services answer on the Wi-Fi link only
            if index == 0 {
                input.push_str(&format!(
                    "\t\tiifname \"{host}\" ip saddr {cell} tcp sport 5555 ct state established accept\n"
                ));
            }
            input.push_str(&format!("\t\tiifname \"{host}\" drop\n"));
            forward.push_str(&format!(
                "\t\tiifname \"{host}\" ip saddr != {cell} drop\n\
                 \t\tiifname \"{host}\" ip saddr {cell} accept\n\
                 \t\toifname \"{host}\" ip daddr {cell} ct state established,related accept\n\
                 \t\toifname \"{host}\" drop\n"
            ));
            nat.push_str(&format!("\t\tip saddr {cell} masquerade\n"));
        }
        run_script(
            "nft",
            ["-f", "-"],
            &format!(
                r#"table inet {filter_table} {{
	chain input {{
		type filter hook input priority filter; policy accept;
{input}	}}
	chain forward {{
		type filter hook forward priority filter; policy accept;
{forward}	}}
}}
table ip {nat_table} {{
	chain postrouting {{
		type nat hook postrouting priority srcnat; policy accept;
{nat}	}}
}}
"#
            ),
        )?;
        self.resources.push(NetworkResource::FilterTable);
        self.resources.push(NetworkResource::NatTable);
        Ok(())
    }

    /// Remove every object created by this instance in reverse ownership order.
    pub(crate) fn teardown(&mut self) -> Result<(), DevelopmentError> {
        let mut first_error = None;

        while let Some(resource) = self.resources.pop() {
            let result = match resource {
                NetworkResource::UfwForwardRule(index) => remove_ufw_rule(&self.names, index),
                NetworkResource::Namespace => run("ip", ["netns", "delete", &self.names.namespace]),
                NetworkResource::HostInterface(index) => {
                    run("ip", ["link", "delete", &self.names.host_interfaces[index]])
                }
                NetworkResource::FilterTable => {
                    run("nft", ["delete", "table", "inet", &self.names.filter_table])
                }
                NetworkResource::NatTable => {
                    run("nft", ["delete", "table", "ip", &self.names.nat_table])
                }
            };
            record_error(&mut first_error, result);
        }
        if let Some(previous) = self.previous_ipv4_forward.take() {
            if previous.trim() != "1" {
                record_error(
                    &mut first_error,
                    fs::write(IPV4_FORWARD_PATH, previous)
                        .map_err(|source| io_error("restore host IPv4 forwarding", source)),
                );
            }
            record_error(
                &mut first_error,
                remove_file_if_present(
                    &self.forwarding_state_path(),
                    "remove host IPv4 forwarding state",
                ),
            );
        }

        first_error.map_or(Ok(()), Err)
    }

    /// Remove exact, deterministically named objects left by an interrupted
    /// daemon. This never enumerates or deletes unrelated host networking.
    pub(crate) fn recover(spec: &CellSpec) -> Result<(), DevelopmentError> {
        let names = NetworkNames::for_spec(spec);
        let mut first_error = None;

        for index in 0..LINKS.len() {
            record_error(&mut first_error, remove_ufw_rule(&names, index));
        }
        record_error(
            &mut first_error,
            run_script(
                "nft",
                ["-f", "-"],
                &format!(
                    "destroy table ip {}\ndestroy table inet {}\n",
                    names.nat_table, names.filter_table
                ),
            ),
        );
        for interface in &names.host_interfaces {
            if link_exists(interface) {
                record_error(&mut first_error, run("ip", ["link", "delete", interface]));
            }
        }
        if namespace_exists(&names.namespace) {
            record_error(
                &mut first_error,
                run("ip", ["netns", "delete", &names.namespace]),
            );
        }

        let forwarding_state = spec.runtime_dir.join(IPV4_FORWARD_STATE_FILE);
        match fs::read_to_string(&forwarding_state) {
            Ok(previous) => {
                record_error(
                    &mut first_error,
                    fs::write(IPV4_FORWARD_PATH, previous).map_err(|source| {
                        io_error("restore stale host IPv4 forwarding state", source)
                    }),
                );
                record_error(
                    &mut first_error,
                    remove_file_if_present(
                        &forwarding_state,
                        "remove stale host IPv4 forwarding state",
                    ),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                record_error(
                    &mut first_error,
                    Err(io_error("read stale host IPv4 forwarding state", source)),
                );
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    fn forwarding_state_path(&self) -> PathBuf {
        // `runtime_dir` has already been created mode 0700 before networking
        // starts, so this recovery record is not exposed to the Android cell.
        self.runtime_dir.join(IPV4_FORWARD_STATE_FILE)
    }
}

fn ufw_rule_arguments<'a>(
    operation: &'a str,
    names: &'a NetworkNames,
    index: usize,
) -> [&'a str; 13] {
    [
        "-w",
        operation,
        "ufw-user-forward",
        "-i",
        &names.host_interfaces[index],
        "-s",
        LINKS[index].cell_cidr,
        "-m",
        "comment",
        "--comment",
        &names.namespace,
        "-j",
        "ACCEPT",
    ]
}

fn ufw_chain_exists() -> Result<bool, DevelopmentError> {
    match droidloom_cpu_placement::command("iptables")
        .args(["-w", "-S", "ufw-user-forward"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => Ok(status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect UFW forwarding chain", error)),
    }
}

fn remove_ufw_rule(names: &NetworkNames, index: usize) -> Result<(), DevelopmentError> {
    if ufw_chain_exists()?
        && command_succeeds("iptables", ufw_rule_arguments("-C", names, index))?
    {
        run("iptables", ufw_rule_arguments("-D", names, index))?;
    }
    Ok(())
}

fn command_succeeds<const N: usize>(
    program: &str,
    args: [&str; N],
) -> Result<bool, DevelopmentError> {
    droidloom_cpu_placement::command(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .map_err(|source| io_error(&format!("execute {program}"), source))
}

fn namespace_exists(namespace: &str) -> bool {
    Path::new(NETNS_DIR).join(namespace).exists()
}

fn link_exists(interface: &str) -> bool {
    Path::new(NET_CLASS_DIR).join(interface).exists()
}

fn remove_file_if_present(path: &Path, context: &str) -> Result<(), DevelopmentError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(context, source)),
    }
}

fn run<const N: usize>(program: &str, args: [&str; N]) -> Result<(), DevelopmentError> {
    let status = droidloom_cpu_placement::command(program)
        .args(args)
        .status()
        .map_err(|source| io_error(&format!("execute {program}"), source))?;
    command_status(program, status)
}

/// Run `program` once with `script` on its standard input.
fn run_script<const N: usize>(
    program: &str,
    args: [&str; N],
    script: &str,
) -> Result<(), DevelopmentError> {
    let mut child = droidloom_cpu_placement::command(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|source| io_error(&format!("execute {program}"), source))?;
    let written = child
        .stdin
        .take()
        .map_or(Ok(()), |mut stdin| stdin.write_all(script.as_bytes()));
    let status = child
        .wait()
        .map_err(|source| io_error(&format!("wait for {program}"), source))?;
    written.map_err(|source| io_error(&format!("write {program} script"), source))?;
    command_status(program, status)
}

fn command_status(program: &str, status: std::process::ExitStatus) -> Result<(), DevelopmentError> {
    if status.success() {
        Ok(())
    } else {
        Err(DevelopmentError::Command {
            program: program.into(),
            status,
        })
    }
}

fn record_error(slot: &mut Option<DevelopmentError>, result: Result<(), DevelopmentError>) {
    if let Err(error) = result
        && slot.is_none()
    {
        *slot = Some(error);
    }
}

fn io_error(context: &str, source: std::io::Error) -> DevelopmentError {
    DevelopmentError::Io {
        context: context.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IdRange;
    use std::path::PathBuf;

    fn spec(host_uid: u32) -> CellSpec {
        CellSpec {
            host_uid,
            subordinate_uids: IdRange {
                start: 100_000,
                count: 100_000,
            },
            subordinate_gids: IdRange {
                start: 200_000,
                count: 100_000,
            },
            image_dir: PathBuf::from("/images"),
            gapps_dir: None,
            vendor_image: PathBuf::from("/images/vendor.img"),
            android_init: None,
            android_file_overrides: Vec::new(),
            android_runtime_directories: Vec::new(),
            shared_storage_directories: Vec::new(),
            share_user_folders: false,
            data_dir: PathBuf::from("/data"),
            runtime_dir: PathBuf::from("/run/droidloom/cells/test"),
            gpu: Default::default(),
            render_node: PathBuf::from("/dev/dri/renderD128"),
            graphics_backend: crate::GraphicsBackend::default(),
            denial_socket: PathBuf::from("/run/user/1001/denial.sock"),
        }
    }

    #[test]
    fn network_objects_are_stable_and_interface_names_fit_linux() {
        let names = NetworkNames::for_spec(&spec(1001));
        assert_eq!(names.namespace, "droidloom-u1001");
        assert_eq!(names.host_interfaces, ["dlh1001", "dlm1001"]);
        assert!(names.host_interfaces.iter().all(|name| name.len() < libc::IF_NAMESIZE));
    }

    #[test]
    fn each_cell_address_and_gateway_share_a_bounded_subnet() {
        let subnets: Vec<_> = LINKS
            .iter()
            .map(|link| (link.host_cidr, link.cell_cidr, link.peer))
            .collect();
        assert_eq!(
            subnets,
            [
                ("10.177.0.1/30", "10.177.0.2/32", "eth0"),
                ("10.177.0.5/30", "10.177.0.6/32", "eth1"),
            ]
        );
    }
}
