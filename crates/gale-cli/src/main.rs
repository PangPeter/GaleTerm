use std::io::{Read, Write};
use std::thread;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use gale_core::{
    AuthMethod, FtpSecurity, Protocol, SerialParity, SessionProfile, SessionStore, SshAuth,
};
use gale_net::{TcpSession, TcpSessionConfig, TelnetSession, TelnetSessionConfig};
use gale_serial::{list_ports, SerialSession, SerialSessionConfig};
use gale_ssh::{
    default_local_download_path, default_local_scp_path, parse_bind_addr, parse_host_port,
    parse_proxy_url, run_forward, ForwardSpec, HostKeyPolicy, ProxyKind, ScpClient, SftpClient,
    SshSession, SshSessionConfig,
};
use gale_term::{LocalPty, PtySize};

#[derive(Debug, Parser)]
#[command(
    name = "gale",
    version,
    about = "GaleTerm — WindTerm-class terminal client"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// List saved sessions
    List,
    /// Add a session profile
    Add {
        /// Session display name
        name: String,
        /// Protocol: local | ssh | telnet | tcp | serial | ftp | ftps
        #[arg(long, default_value = "ssh")]
        protocol: String,
        #[arg(long)]
        host: Option<String>,
        /// Port (ssh default 22, telnet default 23; required for tcp)
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        user: Option<String>,
        /// Auth: password | public_key | agent (agent uses SSH_AUTH_SOCK / Pageant)
        #[arg(long, default_value = "public_key")]
        auth: String,
        #[arg(long)]
        identity: Option<String>,
        #[arg(long)]
        group: Option<String>,
        /// ProxyJump: id or name of an existing SSH session used as bastion
        #[arg(long)]
        jump: Option<String>,
        /// Outbound proxy URL, e.g. socks5://127.0.0.1:1080 or http://user:pass@host:8080
        #[arg(long)]
        proxy: Option<String>,
        /// Serial device path, e.g. /dev/tty.usbserial or COM3
        #[arg(long)]
        device: Option<String>,
        /// Serial baud rate (default 115200)
        #[arg(long, default_value = "115200")]
        baud: u32,
        /// Serial data bits 5–8 (default 8)
        #[arg(long, default_value = "8")]
        data_bits: u8,
        /// Serial stop bits 1|2 (default 1)
        #[arg(long, default_value = "1")]
        stop_bits: u8,
        /// Serial parity: none | even | odd
        #[arg(long, default_value = "none")]
        parity: String,
    },
    /// Remove a session by id or name
    Remove {
        id_or_name: String,
    },
    /// Open a saved session (ssh) or spawn local shell
    Connect {
        /// Session id or name. Use "local" for an ephemeral local shell.
        target: String,
        #[arg(long, default_value = "80")]
        cols: u16,
        #[arg(long, default_value = "24")]
        rows: u16,
        /// Host key policy: ask | accept-new | strict (default: ask / TOFU)
        #[arg(long, default_value = "ask")]
        host_key: String,
        /// Override known_hosts path (default: ~/.ssh/known_hosts)
        #[arg(long)]
        known_hosts: Option<String>,
        /// Override ProxyJump for this connect (id or name); empty string clears
        #[arg(long)]
        jump: Option<String>,
        /// Override outbound proxy URL for this connect
        #[arg(long)]
        proxy: Option<String>,
    },
    /// SSH port forwarding (local / remote / dynamic SOCKS5)
    Forward {
        #[command(subcommand)]
        mode: ForwardCommands,
    },
    /// SFTP file transfer (ls / get / put / rm / mkdir / mv)
    Sftp {
        #[command(subcommand)]
        action: SftpCommands,
    },
    /// SCP file transfer (get / put)
    Scp {
        #[command(subcommand)]
        action: ScpCommands,
    },
    /// List serial ports on this machine
    Ports,
    /// Show store path and version info
    Info,
}

#[derive(Debug, Subcommand)]
enum ForwardCommands {
    /// Local forward (-L): listen locally, tunnel to remote host:port
    Local {
        /// Saved SSH session id or name
        session: String,
        /// Local bind, e.g. 127.0.0.1:8080 or just 8080
        #[arg(long)]
        bind: String,
        /// Destination host:port as seen from the SSH server
        #[arg(long)]
        dest: String,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Remote forward (-R): listen on the SSH server, tunnel to local dest
    Remote {
        session: String,
        /// Remote bind on the server, e.g. 0.0.0.0:8080 or 8080
        #[arg(long)]
        bind: String,
        /// Local destination host:port
        #[arg(long)]
        dest: String,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Dynamic SOCKS5 forward (-D)
    Dynamic {
        session: String,
        /// Local SOCKS5 bind, e.g. 127.0.0.1:1080 or 1080
        #[arg(long)]
        bind: String,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum SftpCommands {
    /// List remote directory
    Ls {
        session: String,
        /// Remote path (default: .)
        #[arg(default_value = ".")]
        path: String,
        /// Long listing
        #[arg(short = 'l', long)]
        long: bool,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Print remote working directory
    Pwd {
        session: String,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Download remote file or directory
    Get {
        session: String,
        /// Remote path
        remote: String,
        /// Local path (file or directory); defaults to basename of remote
        local: Option<String>,
        /// Recurse into directories
        #[arg(short = 'r', long)]
        recursive: bool,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Upload local file or directory
    Put {
        session: String,
        /// Local path
        local: String,
        /// Remote path
        remote: String,
        /// Recurse into directories
        #[arg(short = 'r', long)]
        recursive: bool,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Remove remote file or directory
    Rm {
        session: String,
        path: String,
        /// Recurse into directories
        #[arg(short = 'r', long)]
        recursive: bool,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Create remote directory
    Mkdir {
        session: String,
        path: String,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Rename / move remote path
    Mv {
        session: String,
        from: String,
        to: String,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ScpCommands {
    /// Download remote file or directory via SCP
    Get {
        session: String,
        remote: String,
        local: Option<String>,
        #[arg(short = 'r', long)]
        recursive: bool,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Upload local file or directory via SCP
    Put {
        session: String,
        local: String,
        remote: String,
        #[arg(short = 'r', long)]
        recursive: bool,
        #[arg(long, default_value = "ask")]
        host_key: String,
        #[arg(long)]
        known_hosts: Option<String>,
        #[arg(long)]
        jump: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::List => cmd_list()?,
        Commands::Add {
            name,
            protocol,
            host,
            port,
            user,
            auth,
            identity,
            group,
            jump,
            proxy,
            device,
            baud,
            data_bits,
            stop_bits,
            parity,
        } => cmd_add(
            name, protocol, host, port, user, auth, identity, group, jump, proxy, device, baud,
            data_bits, stop_bits, parity,
        )?,
        Commands::Remove { id_or_name } => cmd_remove(id_or_name)?,
        Commands::Connect {
            target,
            cols,
            rows,
            host_key,
            known_hosts,
            jump,
            proxy,
        } => cmd_connect(target, cols, rows, host_key, known_hosts, jump, proxy).await?,
        Commands::Forward { mode } => cmd_forward(mode).await?,
        Commands::Sftp { action } => cmd_sftp(action).await?,
        Commands::Scp { action } => cmd_scp(action).await?,
        Commands::Ports => cmd_ports()?,
        Commands::Info => cmd_info()?,
    }
    Ok(())
}

fn cmd_list() -> Result<()> {
    let store = SessionStore::open_default()?;
    if store.list().is_empty() {
        println!("(no sessions)  tip: gale add <name> --host <ip> --user <user>");
        return Ok(());
    }
    println!(
        "{:<36} {:<16} {:<8} {}",
        "ID", "NAME", "PROTO", "TARGET"
    );
    for s in store.list() {
        let target = match s.protocol {
            Protocol::Ssh => {
                let base = s
                    .ssh
                    .as_ref()
                    .map(|c| format!("{}@{}:{}", c.username, c.host, c.port))
                    .unwrap_or_default();
                match s.ssh.as_ref().and_then(|c| c.jump_session_id.as_ref()) {
                    Some(jid) => {
                        let jump_name = store
                            .get(jid.as_str())
                            .map(|j| j.name.as_str())
                            .unwrap_or(jid.as_str());
                        format!("{base} via {jump_name}")
                    }
                    None => base,
                }
            }
            Protocol::Local => "local-shell".into(),
            Protocol::Telnet => s
                .telnet
                .as_ref()
                .map(|c| format!("{}:{}", c.host, c.port))
                .unwrap_or_default(),
            Protocol::Tcp => s
                .tcp
                .as_ref()
                .map(|c| format!("{}:{}", c.host, c.port))
                .unwrap_or_default(),
            Protocol::Serial => s
                .serial
                .as_ref()
                .map(|c| format!("{} @ {}", c.device, c.baud_rate))
                .unwrap_or_default(),
            other => other.as_str().into(),
        };
        println!(
            "{:<36} {:<16} {:<8} {}",
            s.id.as_str(),
            s.name,
            s.protocol.as_str(),
            target
        );
    }
    Ok(())
}

fn cmd_add(
    name: String,
    protocol: String,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    auth: String,
    identity: Option<String>,
    group: Option<String>,
    jump: Option<String>,
    proxy: Option<String>,
    device: Option<String>,
    baud: u32,
    data_bits: u8,
    stop_bits: u8,
    parity: String,
) -> Result<()> {
    let proto: Protocol = protocol.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let mut store = SessionStore::open_default()?;
    let mut profile = match proto {
        Protocol::Local => SessionProfile::new_local(name),
        Protocol::Ssh => {
            let host = host.context("--host is required for ssh sessions")?;
            let user = user.context("--user is required for ssh sessions")?;
            let port = port.unwrap_or(22);
            let method = parse_auth(&auth)?;
            let mut profile = SessionProfile::new_ssh(
                name,
                host,
                user,
                port,
                SshAuth {
                    method,
                    password: None,
                    private_key_path: identity,
                    passphrase: None,
                },
            );
            if let Some(jump_ref) = jump {
                let jump_profile = store.get(&jump_ref).with_context(|| {
                    format!("jump session not found: {jump_ref}")
                })?;
                if jump_profile.protocol != Protocol::Ssh {
                    bail!("jump session `{}` is not ssh", jump_ref);
                }
                profile.ssh.as_mut().unwrap().jump_session_id =
                    Some(jump_profile.id.clone());
            }
            if let Some(proxy_url) = proxy {
                profile.ssh.as_mut().unwrap().proxy =
                    Some(parse_proxy_url(&proxy_url).map_err(|e| anyhow::anyhow!(e))?);
            }
            profile
        }
        Protocol::Telnet => {
            let host = host.context("--host is required for telnet sessions")?;
            SessionProfile::new_telnet(name, host, port.unwrap_or(23))
        }
        Protocol::Tcp => {
            let host = host.context("--host is required for tcp sessions")?;
            let port = port.context("--port is required for tcp sessions")?;
            SessionProfile::new_tcp(name, host, port)
        }
        Protocol::Serial => {
            let device = device.context("--device is required for serial sessions")?;
            let mut profile = SessionProfile::new_serial(name, device, baud);
            if let Some(serial) = profile.serial.as_mut() {
                serial.data_bits = data_bits;
                serial.stop_bits = stop_bits;
                serial.parity = parse_parity(&parity)?;
            }
            profile
        }
        Protocol::Sftp => bail!("use an ssh session with `gale sftp` / `gale scp`"),
        Protocol::Ftp => {
            let host = host.context("--host is required for ftp sessions")?;
            let security = if protocol.eq_ignore_ascii_case("ftps") {
                FtpSecurity::Explicit
            } else {
                FtpSecurity::Auto
            };
            let user = user.unwrap_or_else(|| "anonymous".into());
            SessionProfile::new_ftp(name, host, user, port.unwrap_or(21), security)
        }
    };
    profile.group = group;

    let id = profile.id.as_str().to_string();
    store.upsert(profile)?;
    println!("saved session {id}");
    Ok(())
}

fn cmd_remove(id_or_name: String) -> Result<()> {
    let mut store = SessionStore::open_default()?;
    if store.remove(&id_or_name)? {
        println!("removed {id_or_name}");
    } else {
        bail!("session not found: {id_or_name}");
    }
    Ok(())
}

fn cmd_info() -> Result<()> {
    let path = SessionStore::default_path()?;
    println!("gale {}", env!("CARGO_PKG_VERSION"));
    println!("sessions: {}", path.display());
    println!("phase: 2.3 (SSH + SFTP/SCP + telnet/tcp + serial)");
    Ok(())
}

fn cmd_ports() -> Result<()> {
    let ports = list_ports().map_err(|e| anyhow::anyhow!(e))?;
    if ports.is_empty() {
        println!("(no serial ports found)");
        return Ok(());
    }
    println!("{:<24} {}", "DEVICE", "TYPE");
    for p in ports {
        println!("{:<24} {}", p.name, p.port_type);
    }
    Ok(())
}

async fn cmd_scp(action: ScpCommands) -> Result<()> {
    let (session, host_key, known_hosts, jump, proxy) = match &action {
        ScpCommands::Get {
            session,
            host_key,
            known_hosts,
            jump,
            proxy,
            ..
        }
        | ScpCommands::Put {
            session,
            host_key,
            known_hosts,
            jump,
            proxy,
            ..
        } => (
            session.clone(),
            host_key.clone(),
            known_hosts.clone(),
            jump.clone(),
            proxy.clone(),
        ),
    };

    let (cfg, label) =
        build_ssh_config(&session, 80, 24, &host_key, known_hosts, jump, proxy)?;
    eprintln!("scp via {label} ...");
    let client = ScpClient::connect(cfg)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    match action {
        ScpCommands::Get {
            remote,
            local,
            recursive,
            ..
        } => {
            let dest = default_local_scp_path(&remote, local.as_deref());
            let n = if recursive {
                client
                    .download_dir(&remote, &dest)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?
            } else {
                client
                    .download(&remote, &dest)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?
            };
            println!("downloaded {n} bytes -> {}", dest.display());
        }
        ScpCommands::Put {
            local,
            remote,
            recursive,
            ..
        } => {
            let n = if recursive {
                client
                    .upload_dir(&local, &remote)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?
            } else {
                client
                    .upload(&local, &remote)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?
            };
            println!("uploaded {n} bytes -> {remote}");
        }
    }
    Ok(())
}

async fn cmd_sftp(action: SftpCommands) -> Result<()> {
    let (session, host_key, known_hosts, jump, proxy) = match &action {
        SftpCommands::Ls {
            session,
            host_key,
            known_hosts,
            jump,
            proxy,
            ..
        }
        | SftpCommands::Pwd {
            session,
            host_key,
            known_hosts,
            jump,
            proxy,
        }
        | SftpCommands::Get {
            session,
            host_key,
            known_hosts,
            jump,
            proxy,
            ..
        }
        | SftpCommands::Put {
            session,
            host_key,
            known_hosts,
            jump,
            proxy,
            ..
        }
        | SftpCommands::Rm {
            session,
            host_key,
            known_hosts,
            jump,
            proxy,
            ..
        }
        | SftpCommands::Mkdir {
            session,
            host_key,
            known_hosts,
            jump,
            proxy,
            ..
        }
        | SftpCommands::Mv {
            session,
            host_key,
            known_hosts,
            jump,
            proxy,
            ..
        } => (
            session.clone(),
            host_key.clone(),
            known_hosts.clone(),
            jump.clone(),
            proxy.clone(),
        ),
    };

    let (cfg, label) =
        build_ssh_config(&session, 80, 24, &host_key, known_hosts, jump, proxy)?;
    eprintln!("sftp via {label} ...");
    let client = SftpClient::connect(cfg)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    match action {
        SftpCommands::Ls { path, long, .. } => {
            let entries = client.list(&path).await.map_err(|e| anyhow::anyhow!(e))?;
            for e in entries {
                if e.name == "." || e.name == ".." {
                    continue;
                }
                if long {
                    println!("{}", e.format_long());
                } else {
                    let marker = if e.is_dir { "/" } else { "" };
                    println!("{}{marker}", e.name);
                }
            }
        }
        SftpCommands::Pwd { .. } => {
            let pwd = client.pwd().await.map_err(|e| anyhow::anyhow!(e))?;
            println!("{pwd}");
        }
        SftpCommands::Get {
            remote,
            local,
            recursive,
            ..
        } => {
            let dest = default_local_download_path(&remote, local.as_deref());
            let n = if recursive {
                client
                    .download_dir(&remote, &dest)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?
            } else {
                client
                    .download(&remote, &dest)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?
            };
            println!("downloaded {n} bytes -> {}", dest.display());
        }
        SftpCommands::Put {
            local,
            remote,
            recursive,
            ..
        } => {
            let n = if recursive {
                client
                    .upload_dir(&local, &remote)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?
            } else {
                client
                    .upload(&local, &remote)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?
            };
            println!("uploaded {n} bytes -> {remote}");
        }
        SftpCommands::Rm {
            path, recursive, ..
        } => {
            client
                .rm(&path, recursive)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("removed {path}");
        }
        SftpCommands::Mkdir { path, .. } => {
            client
                .mkdir(&path)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("mkdir {path}");
        }
        SftpCommands::Mv { from, to, .. } => {
            client
                .rename(&from, &to)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("moved {from} -> {to}");
        }
    }
    Ok(())
}

async fn cmd_forward(mode: ForwardCommands) -> Result<()> {
    let (session, host_key, known_hosts, jump, proxy, spec) = match mode {
        ForwardCommands::Local {
            session,
            bind,
            dest,
            host_key,
            known_hosts,
            jump,
            proxy,
        } => {
            let bind = parse_bind_addr(&bind).map_err(|e| anyhow::anyhow!(e))?;
            let (dest_host, dest_port) = parse_host_port(&dest).map_err(|e| anyhow::anyhow!(e))?;
            (
                session,
                host_key,
                known_hosts,
                jump,
                proxy,
                ForwardSpec::local(bind, dest_host, dest_port),
            )
        }
        ForwardCommands::Remote {
            session,
            bind,
            dest,
            host_key,
            known_hosts,
            jump,
            proxy,
        } => {
            let bind = parse_bind_addr(&bind).map_err(|e| anyhow::anyhow!(e))?;
            let (dest_host, dest_port) = parse_host_port(&dest).map_err(|e| anyhow::anyhow!(e))?;
            (
                session,
                host_key,
                known_hosts,
                jump,
                proxy,
                ForwardSpec::remote(bind, dest_host, dest_port),
            )
        }
        ForwardCommands::Dynamic {
            session,
            bind,
            host_key,
            known_hosts,
            jump,
            proxy,
        } => {
            let bind = parse_bind_addr(&bind).map_err(|e| anyhow::anyhow!(e))?;
            (
                session,
                host_key,
                known_hosts,
                jump,
                proxy,
                ForwardSpec::dynamic(bind),
            )
        }
    };

    let (cfg, label) =
        build_ssh_config(&session, 80, 24, &host_key, known_hosts, jump, proxy)?;
    println!("forwarding via {label} ...");
    run_forward(cfg, spec)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(())
}

async fn cmd_connect(
    target: String,
    cols: u16,
    rows: u16,
    host_key: String,
    known_hosts: Option<String>,
    jump_override: Option<String>,
    proxy_override: Option<String>,
) -> Result<()> {
    if target == "local" || target == "." {
        return connect_local(cols, rows);
    }

    let store = SessionStore::open_default()?;
    let profile = store
        .get(&target)
        .cloned()
        .with_context(|| format!("session not found: {target}"))?;

    match profile.protocol {
        Protocol::Local => connect_local(cols, rows),
        Protocol::Ssh => {
            let (cfg, label) = build_ssh_config(
                &target,
                cols,
                rows,
                &host_key,
                known_hosts,
                jump_override,
                proxy_override,
            )?;
            println!("connecting to {label} ...");
            let mut session = SshSession::connect(cfg).await?;
            session.relay_stdio().await?;
            Ok(())
        }
        Protocol::Telnet => {
            let mut cfg = TelnetSessionConfig::from_profile(&profile, cols, rows)
                .map_err(|e| anyhow::anyhow!(e))?;
            cfg.cols = cols;
            cfg.rows = rows;
            println!("telnet {}:{} ...", cfg.host, cfg.port);
            let session = TelnetSession::connect(cfg)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            session.relay_stdio().await.map_err(|e| anyhow::anyhow!(e))?;
            Ok(())
        }
        Protocol::Tcp => {
            let cfg =
                TcpSessionConfig::from_profile(&profile).map_err(|e| anyhow::anyhow!(e))?;
            println!("tcp {}:{} ...", cfg.host, cfg.port);
            let session = TcpSession::connect(cfg)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            session.relay_stdio().await.map_err(|e| anyhow::anyhow!(e))?;
            Ok(())
        }
        Protocol::Serial => {
            let cfg =
                SerialSessionConfig::from_profile(&profile).map_err(|e| anyhow::anyhow!(e))?;
            println!("serial {} @ {} ...", cfg.device, cfg.baud_rate);
            let session = SerialSession::open(cfg)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            session.relay_stdio().await.map_err(|e| anyhow::anyhow!(e))?;
            Ok(())
        }
        Protocol::Sftp => bail!("use `gale sftp` / `gale scp` with an ssh session"),
        Protocol::Ftp => bail!("ftp sessions are browsed in the desktop file manager"),
    }
}

fn build_ssh_config(
    target: &str,
    cols: u16,
    rows: u16,
    host_key: &str,
    known_hosts: Option<String>,
    jump_override: Option<String>,
    proxy_override: Option<String>,
) -> Result<(SshSessionConfig, String)> {
    let store = SessionStore::open_default()?;
    let mut profile = store
        .get(target)
        .cloned()
        .with_context(|| format!("session not found: {target}"))?;
    if profile.protocol != Protocol::Ssh {
        bail!("session `{target}` is not ssh");
    }

    if let Some(jump_ref) = &jump_override {
        let ssh = profile
            .ssh
            .as_mut()
            .context("session is missing ssh config")?;
        if jump_ref.is_empty() {
            ssh.jump_session_id = None;
        } else {
            let jump_profile = store
                .get(jump_ref)
                .with_context(|| format!("jump session not found: {jump_ref}"))?;
            if jump_profile.protocol != Protocol::Ssh {
                bail!("jump session `{jump_ref}` is not ssh");
            }
            ssh.jump_session_id = Some(jump_profile.id.clone());
        }
    }

    let chain = if jump_override.is_some() {
        build_chain_from_profile(&store, &profile)?
    } else {
        store
            .resolve_jump_chain(profile.id.as_str())
            .map_err(|e| anyhow::anyhow!(e))?
    };

    let policy = parse_host_key_policy(host_key)?;
    let kh = known_hosts.map(expand_tilde_path);

    let mut hop_cfgs: Vec<SshSessionConfig> = chain
        .iter()
        .map(|p| {
            let mut cfg = SshSessionConfig::from_profile(p, cols as u32, rows as u32)?;
            cfg.host_key_policy = policy;
            cfg.known_hosts_path = kh.clone();
            Ok(cfg)
        })
        .collect::<Result<Vec<_>>>()?;

    // Outbound proxy applies to the outermost TCP dial only.
    if let Some(url) = proxy_override {
        let proxy = parse_proxy_url(&url).map_err(|e| anyhow::anyhow!(e))?;
        hop_cfgs
            .first_mut()
            .context("empty jump chain")?
            .proxy = Some(proxy);
    }

    let target_cfg = hop_cfgs.pop().context("empty jump chain")?;
    let jump_names: Vec<_> = chain[..chain.len().saturating_sub(1)]
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    let proxy_note = hop_cfgs
        .first()
        .and_then(|h| h.proxy.as_ref())
        .or(target_cfg.proxy.as_ref())
        .map(|p| format!(" via {}://{}:{}", proxy_kind_str(p.kind), p.host, p.port))
        .unwrap_or_default();
    let label = if jump_names.is_empty() {
        format!(
            "{}@{}:{}{proxy_note}",
            target_cfg.username, target_cfg.host, target_cfg.port
        )
    } else {
        format!(
            "{}@{}:{} via {}{proxy_note}",
            target_cfg.username,
            target_cfg.host,
            target_cfg.port,
            jump_names.join(" -> ")
        )
    };

    Ok((target_cfg.with_jumps(hop_cfgs), label))
}

fn build_chain_from_profile(
    store: &SessionStore,
    profile: &gale_core::SessionProfile,
) -> Result<Vec<gale_core::SessionProfile>> {
    // Temporarily present as if saved: walk jump_session_id using store lookups.
    let mut chain_rev = vec![profile.clone()];
    let mut seen = vec![profile.id.as_str().to_string()];
    let mut current = profile.clone();
    while let Some(jid) = current
        .ssh
        .as_ref()
        .and_then(|s| s.jump_session_id.as_ref())
    {
        if seen.iter().any(|s| s == jid.as_str()) {
            bail!("proxy jump cycle involving `{jid}`");
        }
        let jump = store
            .get(jid.as_str())
            .cloned()
            .with_context(|| format!("jump session not found: {jid}"))?;
        seen.push(jid.as_str().into());
        chain_rev.push(jump.clone());
        current = jump;
    }
    chain_rev.reverse();
    Ok(chain_rev)
}

fn proxy_kind_str(kind: ProxyKind) -> &'static str {
    match kind {
        ProxyKind::Http => "http",
        ProxyKind::Socks5 => "socks5",
    }
}

fn parse_host_key_policy(s: &str) -> Result<HostKeyPolicy> {
    match s.to_ascii_lowercase().as_str() {
        "ask" | "tofu" => Ok(HostKeyPolicy::Ask),
        "accept-new" | "accept_new" | "acceptnew" => Ok(HostKeyPolicy::AcceptNew),
        "strict" | "yes" => Ok(HostKeyPolicy::Strict),
        other => bail!("unknown --host-key policy: {other} (ask|accept-new|strict)"),
    }
}

fn expand_tilde_path(path: String) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

fn connect_local(cols: u16, rows: u16) -> Result<()> {
    let size = PtySize { cols, rows };
    let mut pty = LocalPty::spawn_shell(size).context("failed to spawn local shell")?;
    let mut reader = pty.try_clone_reader().context("failed to clone pty reader")?;

    // Raw-ish stdin: line-buffered is OK for Phase 1 MVP; desktop UI will replace this.
    let writer_pty = thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if pty.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
            if !pty.is_alive() {
                break;
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                stdout.write_all(&buf[..n])?;
                stdout.flush()?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    let _ = writer_pty.join();
    Ok(())
}

fn parse_auth(s: &str) -> Result<AuthMethod> {
    match s.to_ascii_lowercase().as_str() {
        "password" | "pw" => Ok(AuthMethod::Password),
        "public_key" | "key" | "pubkey" => Ok(AuthMethod::PublicKey),
        "agent" => Ok(AuthMethod::Agent),
        "keyboard" | "keyboard_interactive" => Ok(AuthMethod::KeyboardInteractive),
        other => bail!("unknown auth method: {other}"),
    }
}

fn parse_parity(s: &str) -> Result<SerialParity> {
    match s.to_ascii_lowercase().as_str() {
        "none" | "n" => Ok(SerialParity::None),
        "even" | "e" => Ok(SerialParity::Even),
        "odd" | "o" => Ok(SerialParity::Odd),
        other => bail!("unknown parity: {other} (none|even|odd)"),
    }
}
