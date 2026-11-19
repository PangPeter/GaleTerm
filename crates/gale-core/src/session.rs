use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for a saved session profile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Connection protocol. Phase 1 implements Local + Ssh; others are reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Local,
    Ssh,
    Telnet,
    Serial,
    Tcp,
    Sftp,
    Ftp,
    /// Remote Desktop Protocol (Phase 3.5).
    Rdp,
    /// VNC / RFB (Phase 3.5).
    Vnc,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ssh => "ssh",
            Self::Telnet => "telnet",
            Self::Serial => "serial",
            Self::Tcp => "tcp",
            Self::Sftp => "sftp",
            Self::Ftp => "ftp",
            Self::Rdp => "rdp",
            Self::Vnc => "vnc",
        }
    }
}

impl std::str::FromStr for Protocol {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "local" | "shell" => Ok(Self::Local),
            "ssh" => Ok(Self::Ssh),
            "telnet" => Ok(Self::Telnet),
            "serial" => Ok(Self::Serial),
            "tcp" | "raw" => Ok(Self::Tcp),
            "sftp" => Ok(Self::Sftp),
            "ftp" | "ftps" => Ok(Self::Ftp),
            "rdp" => Ok(Self::Rdp),
            "vnc" | "rfb" => Ok(Self::Vnc),
            other => Err(format!("unknown protocol: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Password,
    PublicKey,
    Agent,
    KeyboardInteractive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshAuth {
    pub method: AuthMethod,
    /// Password is stored only when user explicitly opts in (Phase 1: never by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

impl Default for SshAuth {
    fn default() -> Self {
        Self {
            method: AuthMethod::PublicKey,
            password: None,
            private_key_path: None,
            passphrase: None,
        }
    }
}

/// Outbound proxy used when dialing the SSH host (not ProxyJump).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKind {
    Http,
    Socks5,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub auth: SshAuth,
    /// Jump host session id (ProxyJump): connect via that session first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_session_id: Option<SessionId>,
    /// Outbound HTTP/SOCKS5 proxy for the TCP dial to this host (or outermost jump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyConfig>,
}

fn default_ssh_port() -> u16 {
    22
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialConfig {
    pub device: String,
    #[serde(default = "default_baud")]
    pub baud_rate: u32,
    /// Data bits: 5–8 (default 8).
    #[serde(default = "default_data_bits")]
    pub data_bits: u8,
    /// Stop bits: 1 or 2 (default 1).
    #[serde(default = "default_stop_bits")]
    pub stop_bits: u8,
    /// Parity: none | even | odd (default none).
    #[serde(default)]
    pub parity: SerialParity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialParity {
    #[default]
    None,
    Even,
    Odd,
}

fn default_baud() -> u32 {
    115_200
}

fn default_data_bits() -> u8 {
    8
}

fn default_stop_bits() -> u8 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelnetConfig {
    pub host: String,
    #[serde(default = "default_telnet_port")]
    pub port: u16,
}

fn default_telnet_port() -> u16 {
    23
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TcpConfig {
    pub host: String,
    pub port: u16,
}

/// How an FTP session is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FtpSecurity {
    /// `AUTH TLS` when the server offers it, otherwise plain FTP.
    #[default]
    Auto,
    /// Explicit FTPS: fail unless `AUTH TLS` succeeds.
    Explicit,
    /// Implicit FTPS (TLS from the first byte, usually port 990).
    Implicit,
    /// Plain FTP, credentials in clear text.
    Plain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FtpConfig {
    pub host: String,
    #[serde(default = "default_ftp_port")]
    pub port: u16,
    #[serde(default = "default_ftp_user")]
    pub username: String,
    /// Stored only when the user explicitly opts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default)]
    pub security: FtpSecurity,
}

fn default_ftp_port() -> u16 {
    21
}

fn default_ftp_user() -> String {
    "anonymous".into()
}

/// VNC / RFB connection parameters (Phase 3.5 implements the client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VncConfig {
    pub host: String,
    #[serde(default = "default_vnc_port")]
    pub port: u16,
    /// Stored only when the user explicitly opts in; prefer the system vault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// SSH session used as a tunnel (`LocalForward`) before dialing VNC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_session_id: Option<SessionId>,
}

fn default_vnc_port() -> u16 {
    5900
}

/// RDP connection parameters (Phase 3.5 implements the client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdpConfig {
    pub host: String,
    #[serde(default = "default_rdp_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// Windows domain / workgroup, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Stored only when the user explicitly opts in; prefer the system vault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// SSH session used as a tunnel (`LocalForward`) before dialing RDP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_session_id: Option<SessionId>,
}

fn default_rdp_port() -> u16 {
    3389
}

/// Saved session profile (WindTerm-style session entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProfile {
    pub id: SessionId,
    pub name: String,
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<SerialConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telnet: Option<TelnetConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<TcpConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ftp: Option<FtpConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vnc: Option<VncConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rdp: Option<RdpConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SessionProfile {
    pub fn new_local(name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            name: name.into(),
            protocol: Protocol::Local,
            group: None,
            ssh: None,
            serial: None,
            telnet: None,
            tcp: None,
            ftp: None,
            vnc: None,
            rdp: None,
            notes: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn new_ssh(
        name: impl Into<String>,
        host: impl Into<String>,
        username: impl Into<String>,
        port: u16,
        auth: SshAuth,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            name: name.into(),
            protocol: Protocol::Ssh,
            group: None,
            ssh: Some(SshConfig {
                host: host.into(),
                port,
                username: username.into(),
                auth,
                jump_session_id: None,
                proxy: None,
            }),
            serial: None,
            telnet: None,
            tcp: None,
            ftp: None,
            vnc: None,
            rdp: None,
            notes: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn new_telnet(name: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            name: name.into(),
            protocol: Protocol::Telnet,
            group: None,
            ssh: None,
            serial: None,
            telnet: Some(TelnetConfig {
                host: host.into(),
                port,
            }),
            tcp: None,
            ftp: None,
            vnc: None,
            rdp: None,
            notes: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn new_tcp(name: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            name: name.into(),
            protocol: Protocol::Tcp,
            group: None,
            ssh: None,
            serial: None,
            telnet: None,
            tcp: Some(TcpConfig {
                host: host.into(),
                port,
            }),
            ftp: None,
            vnc: None,
            rdp: None,
            notes: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn new_ftp(
        name: impl Into<String>,
        host: impl Into<String>,
        username: impl Into<String>,
        port: u16,
        security: FtpSecurity,
    ) -> Self {
        let mut profile = Self::new_local(name);
        profile.protocol = Protocol::Ftp;
        profile.ftp = Some(FtpConfig {
            host: host.into(),
            port,
            username: username.into(),
            password: None,
            security,
        });
        profile
    }

    pub fn new_vnc(name: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        let mut profile = Self::new_local(name);
        profile.protocol = Protocol::Vnc;
        profile.vnc = Some(VncConfig {
            host: host.into(),
            port,
            password: None,
            jump_session_id: None,
        });
        profile
    }

    pub fn new_rdp(
        name: impl Into<String>,
        host: impl Into<String>,
        username: impl Into<String>,
        port: u16,
    ) -> Self {
        let mut profile = Self::new_local(name);
        profile.protocol = Protocol::Rdp;
        profile.rdp = Some(RdpConfig {
            host: host.into(),
            port,
            username: username.into(),
            domain: None,
            password: None,
            jump_session_id: None,
        });
        profile
    }

    pub fn new_serial(
        name: impl Into<String>,
        device: impl Into<String>,
        baud_rate: u32,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            name: name.into(),
            protocol: Protocol::Serial,
            group: None,
            ssh: None,
            serial: Some(SerialConfig {
                device: device.into(),
                baud_rate,
                data_bits: default_data_bits(),
                stop_bits: default_stop_bits(),
                parity: SerialParity::None,
            }),
            telnet: None,
            tcp: None,
            ftp: None,
            vnc: None,
            rdp: None,
            notes: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ssh_profile_json() {
        let profile = SessionProfile::new_ssh(
            "prod-web",
            "10.0.0.1",
            "ubuntu",
            22,
            SshAuth {
                method: AuthMethod::PublicKey,
                password: None,
                private_key_path: Some("~/.ssh/id_ed25519".into()),
                passphrase: None,
            },
        );
        let json = serde_json::to_string_pretty(&profile).unwrap();
        let back: SessionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "prod-web");
        assert_eq!(back.protocol, Protocol::Ssh);
        assert_eq!(back.ssh.as_ref().unwrap().host, "10.0.0.1");
    }

    #[test]
    fn roundtrip_telnet_serial_tcp() {
        let t = SessionProfile::new_telnet("bbs", "bbs.example", 23);
        let s = SessionProfile::new_serial("uart", "/dev/ttyUSB0", 9600);
        let c = SessionProfile::new_tcp("raw", "127.0.0.1", 9000);
        for p in [&t, &s, &c] {
            let json = serde_json::to_string(p).unwrap();
            let back: SessionProfile = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, p);
        }
    }

    #[test]
    fn roundtrip_vnc_and_rdp() {
        let mut vnc = SessionProfile::new_vnc("lab-display", "10.0.0.5", 5901);
        vnc.vnc.as_mut().unwrap().jump_session_id = Some(SessionId::new());
        let mut rdp = SessionProfile::new_rdp("win-box", "desk.lan", "Administrator", 3389);
        rdp.rdp.as_mut().unwrap().domain = Some("CORP".into());
        for p in [&vnc, &rdp] {
            let json = serde_json::to_string_pretty(p).unwrap();
            assert!(!json.contains("password"));
            let back: SessionProfile = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, p);
        }
        assert_eq!("rdp".parse::<Protocol>().unwrap(), Protocol::Rdp);
        assert_eq!("rfb".parse::<Protocol>().unwrap(), Protocol::Vnc);
    }
}
