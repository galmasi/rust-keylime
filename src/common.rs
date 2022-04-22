// SPDX-License-Identifier: Apache-2.0
// Copyright 2021 Keylime Authors

use crate::algorithms::{EncryptionAlgorithm, HashAlgorithm, SignAlgorithm};
use crate::error::{Error, Result};
use crate::permissions;
use ini::Ini;
use log::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::convert::TryFrom;
use std::env;
use std::ffi::CString;
use std::fmt::Debug;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tss_esapi::{structures::PcrSlot, utils::TpmsContext};
use uuid::Uuid;

/*
 * Constants and static variables
 */
pub const API_VERSION: &str = "v2.0";
pub const STUB_VTPM: bool = false;
pub const STUB_IMA: bool = true;
pub const TPM_DATA_PCR: usize = 16;
pub const IMA_PCR: usize = 10;
pub static DEFAULT_CONFIG: &str = "/etc/keylime.conf";
pub static RSA_PUBLICKEY_EXPORTABLE: &str = "rsa placeholder";
pub static TPM_TOOLS_PATH: &str = "/usr/local/bin/";
pub static IMA_ML: &str =
    "/sys/kernel/security/ima/ascii_runtime_measurements";
pub static MEASUREDBOOT_ML: &str =
    "/sys/kernel/security/tpm0/binary_bios_measurements";
// The DEFAULT_CA_PATH is relative from WORK_DIR
pub static DEFAULT_CA_PATH: &str = "cv_ca/cacert.crt";
pub static KEY: &str = "secret";
pub const MTLS_ENABLED: bool = true;
pub static WORK_DIR: &str = "/var/lib/keylime";
pub static TPM_DATA: &str = "tpmdata.json";
// Note: The revocation certificate name is generated inside the Python tenant and the
// certificate(s) can be generated by running the tenant with the --cert flag. For more
// information, check the README: https://github.com/keylime/keylime/#using-keylime-ca
pub static REV_CERT: &str = "RevocationNotifier-cert.crt";
pub static REV_ACTIONS_DIR: &str = "/usr/libexec/keylime";
pub static REV_ACTIONS: &str = "";
pub static ALLOW_PAYLOAD_REV_ACTIONS: bool = true;
pub static ALLOW_INSECURE_PAYLOAD: bool = false;

pub const AGENT_UUID_LEN: usize = 36;
pub const AUTH_TAG_LEN: usize = 96;
pub const AES_128_KEY_LEN: usize = 16;
pub const AES_256_KEY_LEN: usize = 32;
pub const AES_BLOCK_SIZE: usize = 16;

cfg_if::cfg_if! {
    if #[cfg(any(test, feature = "testing"))] {
        // Secure mount of tpmfs (False is generally used for development environments)
        pub static MOUNT_SECURE: bool = false;

        pub(crate) fn ima_ml_path_get() -> PathBuf {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-data")
                .join("ima")
                .join("ascii_runtime_measurements")
        }
    } else {
        pub static MOUNT_SECURE: bool = true;

        pub(crate) fn ima_ml_path_get() -> PathBuf {
            Path::new(IMA_ML).to_path_buf()
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct APIVersion {
    major: u32,
    minor: u32,
}

impl std::fmt::Display for APIVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}.{}", self.major, self.minor)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct JsonWrapper<A> {
    pub code: u16,
    pub status: String,
    pub results: A,
}

impl JsonWrapper<Value> {
    pub(crate) fn error(
        code: u16,
        status: impl ToString,
    ) -> JsonWrapper<Value> {
        JsonWrapper {
            code,
            status: status.to_string(),
            results: json!({}),
        }
    }
}

impl<'de, A> JsonWrapper<A>
where
    A: Deserialize<'de> + Serialize + Debug,
{
    pub(crate) fn success(results: A) -> JsonWrapper<A> {
        JsonWrapper {
            code: 200,
            status: String::from("Success"),
            results,
        }
    }
}

// a vector holding keys
pub type KeySet = Vec<SymmKey>;

// a key of len AES_128_KEY_LEN or AES_256_KEY_LEN
#[derive(Debug, Clone)]
pub struct SymmKey {
    bytes: Vec<u8>,
}

impl SymmKey {
    pub(crate) fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    pub(crate) fn xor(&self, other: &Self) -> Result<Self> {
        if self.bytes().len() != other.bytes().len() {
            return Err(Error::Other(
                "cannot xor differing length slices".to_string(),
            ));
        }
        let mut outbuf = vec![0u8; self.bytes().len()];
        for (out, (x, y)) in outbuf
            .iter_mut()
            .zip(self.bytes().iter().zip(other.bytes()))
        {
            *out = x ^ y;
        }
        Ok(Self { bytes: outbuf })
    }
}

impl TryFrom<&[u8]> for SymmKey {
    type Error = String;

    fn try_from(v: &[u8]) -> std::result::Result<Self, Self::Error> {
        match v.len() {
            AES_128_KEY_LEN | AES_256_KEY_LEN => {
                Ok(SymmKey { bytes: v.to_vec() })
            }
            other => Err(format!(
                "key length {} does not correspond to valid GCM cipher",
                other
            )),
        }
    }
}

// TPM data that can be persisted and loaded on agent startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TpmData {
    pub ak_hash_alg: HashAlgorithm,
    pub ak_sign_alg: SignAlgorithm,
    pub ak_context: TpmsContext,
}

impl TpmData {
    pub(crate) fn load(path: &Path) -> Result<TpmData> {
        let file = File::open(path)?;
        let data: TpmData = serde_json::from_reader(file)?;
        Ok(data)
    }

    pub(crate) fn store(&self, path: &Path) -> Result<()> {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }

    pub(crate) fn valid(
        &self,
        hash_alg: HashAlgorithm,
        sign_alg: SignAlgorithm,
    ) -> bool {
        hash_alg == self.ak_hash_alg && sign_alg == self.ak_sign_alg
    }
}

#[derive(Clone, Debug)]
pub(crate) struct KeylimeConfig {
    pub agent_ip: String,
    pub agent_port: String,
    pub registrar_ip: String,
    pub registrar_port: String,
    pub agent_uuid: String,
    pub agent_contact_ip: Option<String>,
    pub agent_contact_port: Option<u32>,
    pub hash_alg: HashAlgorithm,
    pub enc_alg: EncryptionAlgorithm,
    pub sign_alg: SignAlgorithm,
    pub tpm_data: Option<TpmData>,
    pub tpm_data_path: String,
    pub run_revocation: bool,
    pub revocation_cert: String,
    pub revocation_ip: String,
    pub revocation_port: String,
    pub secure_size: String,
    pub payload_script: String,
    pub dec_payload_filename: String,
    pub key_filename: String,
    pub extract_payload_zip: bool,
    pub keylime_ca_path: String,
    pub revocation_actions: String,
    pub revocation_actions_dir: String,
    pub allow_payload_revocation_actions: bool,
    pub work_dir: String,
    pub mtls_enabled: bool,
    pub enable_insecure_payload: bool,
    pub run_as: Option<String>,
}

impl KeylimeConfig {
    pub fn build() -> Result<Self> {
        let agent_ip =
            config_get_env("cloud_agent", "cloudagent_ip", "CLOUDAGENT_IP")?;
        let agent_port = config_get_env(
            "cloud_agent",
            "cloudagent_port",
            "CLOUDAGENT_PORT",
        )?;
        let registrar_ip =
            config_get_env("cloud_agent", "registrar_ip", "REGISTRAR_IP")?;
        let registrar_port = config_get_env(
            "cloud_agent",
            "registrar_port",
            "REGISTRAR_PORT",
        )?;
        let agent_uuid_config = config_get("cloud_agent", "agent_uuid")?;
        let agent_uuid = get_uuid(&agent_uuid_config);
        let agent_contact_ip = cloudagent_contact_ip_get();
        let agent_contact_port = cloudagent_contact_port_get()?;
        let hash_alg = HashAlgorithm::try_from(
            config_get("cloud_agent", "tpm_hash_alg")?.as_str(),
        )?;
        let enc_alg = EncryptionAlgorithm::try_from(
            config_get("cloud_agent", "tpm_encryption_alg")?.as_str(),
        )?;
        let sign_alg = SignAlgorithm::try_from(
            config_get("cloud_agent", "tpm_signing_alg")?.as_str(),
        )?;
        // There was a typo in Python Keylime and this accounts for having a version
        // of Keylime installed that still has this typo. TODO: Remove later
        let run_revocation = bool::from_str(
            &config_get("cloud_agent", "listen_notifications")
                .or_else(|_| {
                    config_get("cloud_agent", "listen_notfications")
                })?
                .to_lowercase(),
        )?;
        let revocation_cert = config_get("cloud_agent", "revocation_cert")?;
        let revocation_ip = config_get("general", "receive_revocation_ip")?;
        let revocation_port =
            config_get("general", "receive_revocation_port")?;

        let secure_size = config_get("cloud_agent", "secure_size")?;
        let payload_script = config_get("cloud_agent", "payload_script")?;
        let dec_payload_filename =
            config_get("cloud_agent", "dec_payload_file")?;
        let key_filename = config_get("cloud_agent", "enc_keyname")?;
        let extract_payload_zip = bool::from_str(
            &config_get("cloud_agent", "extract_payload_zip")?.to_lowercase(),
        )?;

        let work_dir =
            config_get_env("cloud_agent", "keylime_dir", "KEYLIME_DIR")
                .or_else::<Error, _>(|_| Ok(String::from(WORK_DIR)))?;

        let tpm_data_path = PathBuf::from(&work_dir).join(TPM_DATA);
        let tpm_data = if tpm_data_path.exists() {
            match TpmData::load(&tpm_data_path) {
                Ok(data) => Some(data),
                Err(e) => {
                    warn!("Could not load TPM data");
                    None
                }
            }
        } else {
            warn!(
                "TPM2 event log not available: {}",
                tpm_data_path.display()
            );
            None
        };

        let mut keylime_ca_path = config_get("cloud_agent", "keylime_ca")?;
        if keylime_ca_path == "default" {
            keylime_ca_path = Path::new(&work_dir)
                .join(DEFAULT_CA_PATH)
                .display()
                .to_string();
        }
        let revocation_actions =
            config_get("cloud_agent", "revocation_actions")
                .or_else::<Error, _>(|_| Ok(String::from(REV_ACTIONS)))?;
        let revocation_actions_dir =
            config_get("cloud_agent", "revocation_actions_dir")
                .or_else::<Error, _>(|_| Ok(String::from(REV_ACTIONS_DIR)))?;
        let allow_payload_revocation_actions = match config_get(
            "cloud_agent",
            "allow_payload_revocation_actions",
        ) {
            Ok(s) => bool::from_str(&s.to_lowercase())?,
            Err(_) => ALLOW_PAYLOAD_REV_ACTIONS,
        };
        let run_as = if permissions::get_euid() == 0 {
            match config_get("cloud_agent", "run_as") {
                Ok(user_group) => Some(user_group),
                Err(_) => {
                    warn!("Cannot drop privileges since 'run_as' is empty or missing in 'cloud_agent' section of keylime.conf.");
                    None
                }
            }
        } else {
            None
        };

        let mtls_enabled =
            match config_get("cloud_agent", "mtls_cert_enabled") {
                Ok(enabled) => bool::from_str(&enabled.to_lowercase())
                    .or::<Error>(Ok(MTLS_ENABLED))?,
                Err(_) => true,
            };

        let enable_insecure_payload =
            match config_get("cloud_agent", "enable_insecure_payload") {
                Ok(allowed) => bool::from_str(&allowed.to_lowercase())
                    .or::<Error>(Ok(ALLOW_INSECURE_PAYLOAD))?,
                Err(_) => false,
            };

        Ok(KeylimeConfig {
            agent_ip,
            agent_port,
            registrar_ip,
            registrar_port,
            agent_uuid,
            agent_contact_ip,
            agent_contact_port,
            hash_alg,
            enc_alg,
            sign_alg,
            tpm_data,
            tpm_data_path: tpm_data_path.display().to_string(),
            run_revocation,
            revocation_cert,
            revocation_ip,
            revocation_port,
            secure_size,
            payload_script,
            dec_payload_filename,
            key_filename,
            extract_payload_zip,
            keylime_ca_path,
            revocation_actions,
            revocation_actions_dir,
            allow_payload_revocation_actions,
            work_dir,
            mtls_enabled,
            enable_insecure_payload,
            run_as,
        })
    }
}

// Default test configuration. This should match the defaults in keylime.conf
#[cfg(any(test, feature = "testing"))]
impl Default for KeylimeConfig {
    fn default() -> Self {
        // In case the tests are executed by privileged user
        let run_as = if permissions::get_euid() == 0 {
            Some("keylime:tss".to_string())
        } else {
            None
        };

        KeylimeConfig {
            agent_ip: "127.0.0.1".to_string(),
            agent_port: "9002".to_string(),
            registrar_ip: "127.0.0.1".to_string(),
            registrar_port: "8890".to_string(),
            agent_uuid: "d432fbb3-d2f1-4a97-9ef7-75bd81c00000".to_string(),
            agent_contact_ip: Some("127.0.0.1".to_string()),
            agent_contact_port: Some(9002),
            hash_alg: HashAlgorithm::Sha256,
            enc_alg: EncryptionAlgorithm::Rsa,
            sign_alg: SignAlgorithm::RsaSsa,
            tpm_data: None,
            tpm_data_path: Path::new(WORK_DIR)
                .join(TPM_DATA)
                .display()
                .to_string(),
            run_revocation: true,
            revocation_cert: "default".to_string(),
            revocation_ip: "127.0.0.1".to_string(),
            revocation_port: "8992".to_string(),
            secure_size: "1m".to_string(),
            payload_script: "autorun.sh".to_string(),
            dec_payload_filename: "decrypted_payload".to_string(),
            key_filename: "derived_tci_key".to_string(),
            extract_payload_zip: true,
            keylime_ca_path: DEFAULT_CA_PATH.to_string(),
            revocation_actions: "".to_string(),
            revocation_actions_dir: "/usr/libexec/keylime".to_string(),
            allow_payload_revocation_actions: true,
            work_dir: WORK_DIR.to_string(),
            mtls_enabled: true,
            enable_insecure_payload: false,
            run_as,
        }
    }
}

fn get_uuid(agent_uuid_config: &str) -> String {
    match agent_uuid_config {
        "openstack" => {
            info!("Openstack placeholder...");
            "openstack".into()
        }
        "hash_ek" => {
            info!("hash_ek placeholder...");
            "hash_ek".into()
        }
        "generate" => {
            let agent_uuid = Uuid::new_v4();
            info!("Generated a new UUID: {}", &agent_uuid);
            agent_uuid.to_string()
        }
        uuid_config => match Uuid::parse_str(uuid_config) {
            Ok(uuid_config) => uuid_config.to_string(),
            Err(_) => {
                info!("Misformatted UUID: {}", &uuid_config);
                let agent_uuid = Uuid::new_v4();
                agent_uuid.to_string()
            }
        },
    }
}

/*
 * Return: Returns the configuration file provided in the environment variable
 * KEYLIME_CONFIG or defaults to /etc/keylime.conf
 *
 * Example call:
 * let config = config_file_get();
 */
fn config_file_get() -> String {
    match env::var("KEYLIME_CONFIG") {
        Ok(cfg) => {
            // The variable length must be larger than 0 to accept
            if !cfg.is_empty() {
                cfg
            } else {
                String::from(DEFAULT_CONFIG)
            }
        }
        _ => String::from(DEFAULT_CONFIG),
    }
}

/// Returns revocation ip from keylime.conf if env var not present
fn revocation_ip_get() -> Result<String> {
    config_get_env("general", "receive_revocation_ip", "REVOCATION_IP")
}

/// Returns revocation port from keylime.conf if env var not present
fn revocation_port_get() -> Result<String> {
    config_get_env("general", "receive_revocation_port", "REVOCATION_PORT")
}

/// Returns the contact ip for the agent if set
fn cloudagent_contact_ip_get() -> Option<String> {
    match config_get_env(
        "cloud_agent",
        "agent_contact_ip",
        "KEYLIME_AGENT_CONTACT_IP",
    ) {
        Ok(ip) => Some(ip),
        Err(_) => None, // Ignore errors because this option might not be set
    }
}

/// Returns the contact ip for the agent if set
fn cloudagent_contact_port_get() -> Result<Option<u32>> {
    match config_get_env(
        "cloud_agent",
        "agent_contact_port",
        "KEYLIME_AGENT_CONTACT_PORT",
    ) {
        Ok(port_str) => match port_str.parse::<u32>() {
            Ok(port) => Ok(Some(port)),
            _ => Err(Error::Configuration(format!(
                "Parse {} to a port number.",
                port_str
            ))),
        },
        _ => Ok(None), // Ignore errors because this option might not be set
    }
}

/*
 * Input: [section] and key
 * Return: Returns the matched key
 *
 * Example call:
 * let port = common::config_get("general","cloudagent_port");
 */
fn config_get(section: &str, key: &str) -> Result<String> {
    let conf_name = config_file_get();
    let conf = Ini::load_from_file(&conf_name)?;
    let section = match conf.section(Some(section.to_owned())) {
        Some(section) => section,
        None =>
        // TODO: Make Error::Configuration an alternative with data instead of string
        {
            return Err(Error::Configuration(format!(
                "Cannot find section called {} in file {}",
                section, conf_name
            )))
        }
    };
    let value = match section.get(key) {
        Some(value) => value,
        None =>
        // TODO: Make Error::Configuration an alternative with data instead of string
        {
            return Err(Error::Configuration(format!(
                "Cannot find key {} in file {}",
                key, conf_name
            )))
        }
    };

    Ok(value.to_string())
}

/*
 * Input: [section] and key and environment variable
 * Return: Returns the matched key
 *
 * Example call:
 * let port = common::config_get_env("general","cloudagent_port", "CLOUDAGENT_PORT");
 */
fn config_get_env(section: &str, key: &str, env: &str) -> Result<String> {
    match env::var(env) {
        Ok(ip) => {
            // The variable length must be larger than 0 to accept
            if !ip.is_empty() {
                Ok(ip)
            } else {
                config_get(section, key)
            }
        }
        _ => config_get(section, key),
    }
}

// Unit Testing
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_get_parameters_exist() {
        //let result = config_get("keylime.conf", "general", "cloudagent_port");
        //assert_eq!(result, "9002");
    }

    #[test]
    fn test_config_file_get() {
        let conf_orig = option_env!("KEYLIME_CONFIG").or(Some("")).unwrap(); //#[allow_ci]

        // Test with no environment variable
        env::set_var("KEYLIME_CONFIG", "");
        assert_eq!(config_file_get(), String::from("/etc/keylime.conf"));

        // Test with an environment variable
        env::set_var("KEYLIME_CONFIG", "/tmp/testing.conf");
        assert_eq!(config_file_get(), String::from("/tmp/testing.conf"));
        // Reset environment
        env::set_var("KEYLIME_CONFIG", conf_orig);
    }

    #[test]
    fn test_get_uuid() {
        assert_eq!(get_uuid("openstack"), "openstack");
        assert_eq!(get_uuid("hash_ek"), "hash_ek");
        let _ = Uuid::parse_str(&get_uuid("generate")).unwrap(); //#[allow_ci]
        assert_eq!(
            get_uuid("D432FBB3-D2F1-4A97-9EF7-75BD81C00000"),
            "d432fbb3-d2f1-4a97-9ef7-75bd81c00000"
        );
        assert_ne!(
            get_uuid("D432FBB3-D2F1-4A97-9EF7-75BD81C0000X"),
            "d432fbb3-d2f1-4a97-9ef7-75bd81c0000X"
        );
        let _ = Uuid::parse_str(&get_uuid(
            "D432FBB3-D2F1-4A97-9EF7-75BD81C0000X",
        ))
        .unwrap(); //#[allow_ci]
    }
}
