//! ABI 7.7: the `[identity]`, `[identity.publisher]` and `[remote_config]`
//! settings of a node config (gateway identity,
//! `docs/_internal/web_gateway/gateway-identity.md`).
//!
//! Like the 7.3 network and 7.4 metrics settings they travel beside
//! `aster_config_t` in its library-owned `storage`, so `aster_config_t`'s
//! layout does not change:
//!
//! - `aster_config_from_env` / `aster_config_from_toml` read
//!   `ASTER_IDENTITY_*`, `ASTER_REMOTE_CONFIG_*`, `[identity]`,
//!   `[[identity.trusted_issuers]]`, `[identity.publisher]` and
//!   `[remote_config]` into `storage`, with where each overridable setting
//!   came from (`aster.ops.NodeConfig` `get` reports it).
//! - `aster_config_identity` reads them back; `aster_config_set_identity`
//!   copies a caller's settings in.
//! - `aster_node_start` / `aster_node_start_enrolled` keep them with the
//!   node; `aster_server_builder_new` / `aster_server_start` on that node
//!   validate and apply them as `AsterServer` does, and a client node runs
//!   `[identity.publisher]` once it enrolls with a server.
//!
//! The two structured values are JSON, the shape their environment variables
//! already take: `trusted_issuers_json` is `ASTER_IDENTITY_TRUSTED_ISSUERS`
//! (an array of `{iss, audiences, roles_claim, role_map,
//! max_token_lifetime_secs}`), `publisher_json` is `ASTER_IDENTITY_PUBLISHER`
//! (`{issuers, jwks_url, refresh_secs, min_refresh_secs, ca_certs, ca_only}`).

use std::collections::BTreeMap;
use std::slice;

use aster::{
    AsterConfig, ConfigSource, IdentityPublisherConfig, TrustedIssuer, SETTING_ACCEPT_ON_BEHALF_OF,
    SETTING_CLOCK_LEEWAY, SETTING_EDGE_TOKEN_MAX_LIFETIME, SETTING_MAX_KEY_SET_AGE,
    SETTING_TRUSTED_ISSUERS,
};

use crate::node::{aster_config_t, ConfigStorage};
use crate::{aster_bytes_list_t, aster_bytes_t, aster_status_t, set_last_error};

/// `aster_identity_config_t.set`: `edge_token_max_lifetime_secs` is set
/// (clear: the default, 300).
pub const ASTER_IDENTITY_SET_EDGE_TOKEN_MAX_LIFETIME: u32 = 0x1;
/// `aster_identity_config_t.set`: `clock_leeway_secs` is set (clear: 60).
pub const ASTER_IDENTITY_SET_CLOCK_LEEWAY: u32 = 0x2;
/// `aster_identity_config_t.set`: `max_key_set_age_secs` is set (clear:
/// 172800).
pub const ASTER_IDENTITY_SET_MAX_KEY_SET_AGE: u32 = 0x4;

/// Index into `aster_identity_config_t.sources`: `accept_on_behalf_of`.
pub const ASTER_IDENTITY_SETTING_ACCEPT_ON_BEHALF_OF: u32 = 0;
/// Index into `aster_identity_config_t.sources`: `trusted_issuers`.
pub const ASTER_IDENTITY_SETTING_TRUSTED_ISSUERS: u32 = 1;
/// Index into `aster_identity_config_t.sources`:
/// `edge_token_max_lifetime_secs`.
pub const ASTER_IDENTITY_SETTING_EDGE_TOKEN_MAX_LIFETIME: u32 = 2;
/// Index into `aster_identity_config_t.sources`: `clock_leeway_secs`.
pub const ASTER_IDENTITY_SETTING_CLOCK_LEEWAY: u32 = 3;
/// Index into `aster_identity_config_t.sources`: `max_key_set_age_secs`.
pub const ASTER_IDENTITY_SETTING_MAX_KEY_SET_AGE: u32 = 4;

/// `aster_identity_config_t.sources[i]`: where an overridable setting came
/// from (`aster::config::ConfigSource`), as `aster.ops.NodeConfig` `get`
/// reports it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_config_source_t {
    /// Not recorded: reported as `default` when the value is the default,
    /// else `code`.
    ASTER_CONFIG_SOURCE_UNSET = 0,
    ASTER_CONFIG_SOURCE_DEFAULT = 1,
    ASTER_CONFIG_SOURCE_FILE = 2,
    ASTER_CONFIG_SOURCE_ENV = 3,
    ASTER_CONFIG_SOURCE_CODE = 4,
}

/// `[identity]`, `[identity.publisher]` and `[remote_config]` of a node config
/// (ABI 7.7), next to `aster_config_t`; see `aster_config_set_identity`. 96
/// bytes (LP64). A zeroed struct with `struct_size` set is "nothing
/// configured": on-behalf-of refused, no trusted issuers, default limits, no
/// publisher, `aster.ops.NodeConfig` served with nothing locked.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_identity_config_t {
    /// `sizeof(aster_identity_config_t)`, set by the caller.
    pub struct_size: u32,
    /// `ASTER_IDENTITY_SET_*`: which of the three limits below are set.
    pub set: u32,
    /// `[identity] accept_on_behalf_of`, 0/1 (default 0).
    pub accept_on_behalf_of: u32,
    /// 1 = `[remote_config] enabled = false`: `aster.ops.NodeConfig` is not
    /// served and a stored record is ignored. 0 = served (the default).
    pub remote_config_disabled: u32,
    /// `[identity] edge_token_max_lifetime_secs`, 1..=300.
    pub edge_token_max_lifetime_secs: u64,
    /// `[identity] clock_leeway_secs`, 0..=300.
    pub clock_leeway_secs: u64,
    /// `[identity] max_key_set_age_secs`, 60..=2592000.
    pub max_key_set_age_secs: u64,
    /// `[[identity.trusted_issuers]]` as a JSON array (the
    /// `ASTER_IDENTITY_TRUSTED_ISSUERS` form); empty = none.
    pub trusted_issuers_json: aster_bytes_t,
    /// `[identity.publisher]` as a JSON object (the `ASTER_IDENTITY_PUBLISHER`
    /// form); empty = no publisher.
    pub publisher_json: aster_bytes_t,
    /// `[remote_config] lock`: section names that may not be overridden.
    pub remote_config_lock: aster_bytes_list_t,
    /// `aster_config_source_t` per `ASTER_IDENTITY_SETTING_*` index; 0 = not
    /// recorded. Filled configs carry what the file / environment set; a
    /// caller may leave them 0.
    pub sources: [u8; 8],
}

const SETTING_KEYS: [&str; 5] = [
    SETTING_ACCEPT_ON_BEHALF_OF,
    SETTING_TRUSTED_ISSUERS,
    SETTING_EDGE_TOKEN_MAX_LIFETIME,
    SETTING_CLOCK_LEEWAY,
    SETTING_MAX_KEY_SET_AGE,
];

/// The settings, owned. Default = nothing configured.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IdentityConfigSettings {
    accept_on_behalf_of: bool,
    trusted_issuers: Vec<TrustedIssuer>,
    edge_token_max_lifetime_secs: u64,
    clock_leeway_secs: u64,
    max_key_set_age_secs: u64,
    publisher: Option<IdentityPublisherConfig>,
    remote_config_enabled: bool,
    remote_config_lock: Vec<String>,
    /// Only the `identity.*` keys of `AsterConfig::setting_sources`.
    sources: BTreeMap<String, ConfigSource>,
}

impl Default for IdentityConfigSettings {
    fn default() -> Self {
        Self::from_config(&AsterConfig::default())
    }
}

impl IdentityConfigSettings {
    pub(crate) fn from_config(cfg: &AsterConfig) -> Self {
        Self {
            accept_on_behalf_of: cfg.identity_accept_on_behalf_of,
            trusted_issuers: cfg.identity_trusted_issuers.clone(),
            edge_token_max_lifetime_secs: cfg.identity_edge_token_max_lifetime_secs,
            clock_leeway_secs: cfg.identity_clock_leeway_secs,
            max_key_set_age_secs: cfg.identity_max_key_set_age_secs,
            publisher: cfg.identity_publisher.clone(),
            remote_config_enabled: cfg.remote_config_enabled,
            remote_config_lock: cfg.remote_config_lock.clone(),
            sources: cfg
                .setting_sources
                .iter()
                .filter(|(k, _)| SETTING_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        }
    }

    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub(crate) fn apply(&self, cfg: &mut AsterConfig) {
        cfg.identity_accept_on_behalf_of = self.accept_on_behalf_of;
        cfg.identity_trusted_issuers
            .clone_from(&self.trusted_issuers);
        cfg.identity_edge_token_max_lifetime_secs = self.edge_token_max_lifetime_secs;
        cfg.identity_clock_leeway_secs = self.clock_leeway_secs;
        cfg.identity_max_key_set_age_secs = self.max_key_set_age_secs;
        cfg.identity_publisher.clone_from(&self.publisher);
        cfg.remote_config_enabled = self.remote_config_enabled;
        cfg.remote_config_lock.clone_from(&self.remote_config_lock);
        cfg.setting_sources
            .retain(|k, _| !SETTING_KEYS.contains(&k.as_str()));
        cfg.setting_sources
            .extend(self.sources.iter().map(|(k, v)| (k.clone(), *v)));
    }

    /// Validate and copy a caller's struct. Value ranges are checked as a
    /// whole when a server starts (as `AsterServer` does).
    unsafe fn from_c(c: &aster_identity_config_t) -> Result<Self, String> {
        let flag = |v: u32, field: &str| match v {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(format!(
                "aster_identity_config_t.{field} must be 0 or 1, got {v}"
            )),
        };
        let known = ASTER_IDENTITY_SET_EDGE_TOKEN_MAX_LIFETIME
            | ASTER_IDENTITY_SET_CLOCK_LEEWAY
            | ASTER_IDENTITY_SET_MAX_KEY_SET_AGE;
        if c.set & !known != 0 {
            return Err(format!(
                "aster_identity_config_t.set has unknown bits {:#x}",
                c.set & !known
            ));
        }
        let defaults = AsterConfig::default();
        let pick = |bit: u32, value: u64, default: u64| {
            if c.set & bit != 0 {
                value
            } else {
                default
            }
        };
        let issuers = view(&c.trusted_issuers_json);
        let trusted_issuers = if issuers.is_empty() {
            Vec::new()
        } else {
            serde_json::from_slice(issuers).map_err(|e| {
                format!("aster_identity_config_t.trusted_issuers_json: expected a JSON array of trusted issuers: {e}")
            })?
        };
        let publisher = view(&c.publisher_json);
        let publisher = if publisher.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(publisher).map_err(|e| {
                format!("aster_identity_config_t.publisher_json: expected a JSON object of [identity.publisher] fields: {e}")
            })?)
        };
        let remote_config_lock = list_view(&c.remote_config_lock)
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let bytes = view(b);
                if bytes.is_empty() {
                    return Err(format!(
                        "aster_identity_config_t.remote_config_lock[{i}] is empty"
                    ));
                }
                std::str::from_utf8(bytes).map(str::to_string).map_err(|_| {
                    format!("aster_identity_config_t.remote_config_lock[{i}] is not UTF-8")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut sources = BTreeMap::new();
        for (i, source) in c.sources.iter().enumerate() {
            let source = match *source {
                0 => continue,
                1 => ConfigSource::Default,
                2 => ConfigSource::File,
                3 => ConfigSource::Env,
                4 => ConfigSource::Code,
                v => {
                    return Err(format!(
                        "aster_identity_config_t.sources[{i}] = {v} is not an aster_config_source_t"
                    ))
                }
            };
            let Some(key) = SETTING_KEYS.get(i) else {
                return Err(format!(
                    "aster_identity_config_t.sources[{i}] must be 0 (no such setting)"
                ));
            };
            sources.insert(key.to_string(), source);
        }
        Ok(Self {
            accept_on_behalf_of: flag(c.accept_on_behalf_of, "accept_on_behalf_of")?,
            trusted_issuers,
            edge_token_max_lifetime_secs: pick(
                ASTER_IDENTITY_SET_EDGE_TOKEN_MAX_LIFETIME,
                c.edge_token_max_lifetime_secs,
                defaults.identity_edge_token_max_lifetime_secs,
            ),
            clock_leeway_secs: pick(
                ASTER_IDENTITY_SET_CLOCK_LEEWAY,
                c.clock_leeway_secs,
                defaults.identity_clock_leeway_secs,
            ),
            max_key_set_age_secs: pick(
                ASTER_IDENTITY_SET_MAX_KEY_SET_AGE,
                c.max_key_set_age_secs,
                defaults.identity_max_key_set_age_secs,
            ),
            publisher,
            remote_config_enabled: !flag(c.remote_config_disabled, "remote_config_disabled")?,
            remote_config_lock,
            sources,
        })
    }
}

/// The settings plus the buffers their view points into.
pub(crate) struct StoredIdentity {
    pub(crate) settings: IdentityConfigSettings,
    _bufs: Vec<Vec<u8>>,
    _lists: Vec<Vec<aster_bytes_t>>,
    view: aster_identity_config_t,
}

fn bytes_view(bufs: &mut Vec<Vec<u8>>, v: Vec<u8>) -> aster_bytes_t {
    if v.is_empty() {
        return EMPTY;
    }
    let b = aster_bytes_t {
        ptr: v.as_ptr(),
        len: v.len(),
    };
    bufs.push(v);
    b
}

impl StoredIdentity {
    pub(crate) fn new(settings: IdentityConfigSettings) -> Box<Self> {
        let defaults = AsterConfig::default();
        let mut bufs = Vec::new();
        let mut lists = Vec::new();
        let trusted_issuers_json = if settings.trusted_issuers.is_empty() {
            EMPTY
        } else {
            bytes_view(
                &mut bufs,
                serde_json::to_vec(&settings.trusted_issuers).expect("JSON of plain structs"),
            )
        };
        let publisher_json = match &settings.publisher {
            None => EMPTY,
            Some(p) => bytes_view(
                &mut bufs,
                serde_json::to_vec(p).expect("JSON of a plain struct"),
            ),
        };
        let remote_config_lock = if settings.remote_config_lock.is_empty() {
            EMPTY_LIST
        } else {
            let views: Vec<aster_bytes_t> = settings
                .remote_config_lock
                .iter()
                .map(|s| bytes_view(&mut bufs, s.clone().into_bytes()))
                .collect();
            let l = aster_bytes_list_t {
                items: views.as_ptr(),
                len: views.len(),
            };
            lists.push(views);
            l
        };
        let mut set = 0;
        if settings.edge_token_max_lifetime_secs != defaults.identity_edge_token_max_lifetime_secs {
            set |= ASTER_IDENTITY_SET_EDGE_TOKEN_MAX_LIFETIME;
        }
        if settings.clock_leeway_secs != defaults.identity_clock_leeway_secs {
            set |= ASTER_IDENTITY_SET_CLOCK_LEEWAY;
        }
        if settings.max_key_set_age_secs != defaults.identity_max_key_set_age_secs {
            set |= ASTER_IDENTITY_SET_MAX_KEY_SET_AGE;
        }
        let mut sources = [0u8; 8];
        for (i, key) in SETTING_KEYS.iter().enumerate() {
            sources[i] = match settings.sources.get(*key) {
                None => 0,
                Some(ConfigSource::Default) => 1,
                Some(ConfigSource::File) => 2,
                Some(ConfigSource::Env) => 3,
                Some(ConfigSource::Code | ConfigSource::Override) => 4,
            };
        }
        let view = aster_identity_config_t {
            struct_size: std::mem::size_of::<aster_identity_config_t>() as u32,
            set,
            accept_on_behalf_of: u32::from(settings.accept_on_behalf_of),
            remote_config_disabled: u32::from(!settings.remote_config_enabled),
            edge_token_max_lifetime_secs: settings.edge_token_max_lifetime_secs,
            clock_leeway_secs: settings.clock_leeway_secs,
            max_key_set_age_secs: settings.max_key_set_age_secs,
            trusted_issuers_json,
            publisher_json,
            remote_config_lock,
            sources,
        };
        Box::new(Self {
            settings,
            _bufs: bufs,
            _lists: lists,
            view,
        })
    }
}

const EMPTY: aster_bytes_t = aster_bytes_t {
    ptr: std::ptr::null(),
    len: 0,
};
const EMPTY_LIST: aster_bytes_list_t = aster_bytes_list_t {
    items: std::ptr::null(),
    len: 0,
};

unsafe fn view(b: &aster_bytes_t) -> &[u8] {
    if b.ptr.is_null() || b.len == 0 {
        &[]
    } else {
        slice::from_raw_parts(b.ptr, b.len)
    }
}

unsafe fn list_view(l: &aster_bytes_list_t) -> &[aster_bytes_t] {
    if l.items.is_null() || l.len == 0 {
        &[]
    } else {
        slice::from_raw_parts(l.items, l.len)
    }
}

fn invalid(msg: impl Into<String>) -> i32 {
    set_last_error(msg.into());
    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
}

unsafe fn check_sizes(
    config: *const aster_config_t,
    identity: *const u32,
    func: &str,
) -> Result<(), i32> {
    let size = config.cast::<u32>().read_unaligned();
    let expected = std::mem::size_of::<aster_config_t>() as u32;
    if size != expected {
        return Err(invalid(format!(
            "{func}: aster_config_t.struct_size is {size}, expected {expected}"
        )));
    }
    if !identity.is_null() {
        let size = identity.read_unaligned();
        let expected = std::mem::size_of::<aster_identity_config_t>() as u32;
        if size != expected {
            return Err(invalid(format!(
                "{func}: aster_identity_config_t.struct_size is {size}, expected {expected}"
            )));
        }
    }
    Ok(())
}

/// Set (or, with `identity` NULL, clear) the `[identity]`,
/// `[identity.publisher]` and `[remote_config]` settings of `config` (ABI
/// 7.7). They are parsed and copied into the config's library-owned
/// `storage`, which this allocates for a caller-built config: release such a
/// config with `aster_config_release` once it has been passed to
/// `aster_node_start` (or is no longer needed). Views from an earlier
/// `aster_config_identity` on this config become invalid.
///
/// `identity->struct_size` must equal `sizeof(aster_identity_config_t)`;
/// flags are 0 or 1; `set` holds only `ASTER_IDENTITY_SET_*` bits; the JSON
/// values parse. The values are validated as a whole (issuer audiences,
/// ranges, publisher URLs) when a server starts on the node, as
/// `AsterServer` does.
///
/// # Safety
/// `config` points to a valid `aster_config_t`; `identity` is NULL or valid.
#[no_mangle]
pub unsafe extern "C" fn aster_config_set_identity(
    config: *mut aster_config_t,
    identity: *const aster_identity_config_t,
) -> i32 {
    if config.is_null() {
        return invalid("aster_config_set_identity: config is NULL");
    }
    if let Err(status) = check_sizes(config, identity.cast(), "aster_config_set_identity") {
        return status;
    }
    let settings = match identity.as_ref() {
        None => IdentityConfigSettings::default(),
        Some(i) => match IdentityConfigSettings::from_c(i) {
            Ok(s) => s,
            Err(e) => return invalid(e),
        },
    };
    let c = &mut *config;
    if c.storage.is_null() {
        if settings.is_default() {
            return aster_status_t::ASTER_STATUS_OK as i32;
        }
        c.storage = Box::into_raw(Box::<ConfigStorage>::default()).cast();
    }
    // SAFETY: a non-NULL storage is the library's ConfigStorage.
    let storage = &mut *c.storage.cast::<ConfigStorage>();
    storage.identity = (!settings.is_default()).then(|| StoredIdentity::new(settings));
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Read the `[identity]` / `[remote_config]` settings of `config` (ABI 7.7)
/// into `*out` as borrowed views, valid until `aster_config_release` or the
/// next `aster_config_set_identity` on `config`. A config without any reads
/// as the defaults (`set` 0, the default limits filled in). The caller sets
/// `out->struct_size` to `sizeof(aster_identity_config_t)` before the call.
///
/// # Safety
/// `config` points to a valid `aster_config_t`; `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_config_identity(
    config: *const aster_config_t,
    out: *mut aster_identity_config_t,
) -> i32 {
    if config.is_null() || out.is_null() {
        return invalid("aster_config_identity: config or out is NULL");
    }
    if let Err(status) = check_sizes(config, out.cast_const().cast(), "aster_config_identity") {
        return status;
    }
    let stored = (*config)
        .storage
        .cast::<ConfigStorage>()
        .as_ref()
        .and_then(|st| st.identity.as_ref());
    *out = match stored {
        Some(s) => s.view,
        // No buffers: the default view points at nothing.
        None => StoredIdentity::new(IdentityConfigSettings::default()).view,
    };
    aster_status_t::ASTER_STATUS_OK as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{aster_config_init, aster_config_release, fill_config, lower_config};

    fn view_out() -> aster_identity_config_t {
        let mut v: aster_identity_config_t = unsafe { std::mem::zeroed() };
        v.struct_size = std::mem::size_of::<aster_identity_config_t>() as u32;
        v
    }

    fn bytes(s: &'static str) -> aster_bytes_t {
        aster_bytes_t {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    const ISSUERS: &str = r#"[{"iss":"https://idp.example","audiences":["svc"],"roles_claim":"groups","role_map":{"readers":"notes.read"}}]"#;
    const PUBLISHER: &str = r#"{"issuers":["https://idp.example"],"jwks_url":{"https://idp.example":"https://idp.example/jwks"},"refresh_secs":600}"#;

    #[test]
    fn layout_is_96_bytes() {
        assert_eq!(std::mem::size_of::<aster_identity_config_t>(), 96);
    }

    #[test]
    fn set_read_and_lower_on_a_caller_built_config() {
        assert_eq!(
            crate::aster_abi_accept(crate::ASTER_ABI_VERSION_MAJOR),
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { aster_config_init(&mut c) }, 0);
        let mut back = view_out();
        assert_eq!(unsafe { aster_config_identity(&c, &mut back) }, 0);
        assert_eq!(back.set, 0);
        assert_eq!(back.edge_token_max_lifetime_secs, 300);
        assert_eq!(back.clock_leeway_secs, 60);
        assert_eq!(back.max_key_set_age_secs, 172_800);
        assert!(c.storage.is_null());

        let lock = [bytes("identity")];
        let mut i = view_out();
        i.accept_on_behalf_of = 1;
        i.set = ASTER_IDENTITY_SET_CLOCK_LEEWAY | ASTER_IDENTITY_SET_MAX_KEY_SET_AGE;
        i.clock_leeway_secs = 0;
        i.max_key_set_age_secs = 7200;
        i.trusted_issuers_json = bytes(ISSUERS);
        i.publisher_json = bytes(PUBLISHER);
        i.remote_config_lock = aster_bytes_list_t {
            items: lock.as_ptr(),
            len: 1,
        };
        i.sources[ASTER_IDENTITY_SETTING_ACCEPT_ON_BEHALF_OF as usize] =
            aster_config_source_t::ASTER_CONFIG_SOURCE_ENV as u8;
        assert_eq!(unsafe { aster_config_set_identity(&mut c, &i) }, 0);
        let cfg = unsafe { lower_config(&c) }.unwrap();
        assert!(cfg.identity_accept_on_behalf_of);
        assert_eq!(cfg.identity_clock_leeway_secs, 0);
        assert_eq!(cfg.identity_max_key_set_age_secs, 7200);
        assert_eq!(cfg.identity_edge_token_max_lifetime_secs, 300);
        assert_eq!(cfg.identity_trusted_issuers.len(), 1);
        assert_eq!(
            cfg.identity_trusted_issuers[0].role_map["readers"],
            "notes.read"
        );
        assert_eq!(cfg.identity_publisher.as_ref().unwrap().refresh_secs, 600);
        assert_eq!(cfg.remote_config_lock, vec!["identity".to_string()]);
        assert!(cfg.remote_config_enabled);
        assert_eq!(
            cfg.setting_sources.get(SETTING_ACCEPT_ON_BEHALF_OF),
            Some(&ConfigSource::Env)
        );

        let mut back = view_out();
        assert_eq!(unsafe { aster_config_identity(&c, &mut back) }, 0);
        assert_eq!(back.accept_on_behalf_of, 1);
        assert_eq!(
            back.set,
            ASTER_IDENTITY_SET_CLOCK_LEEWAY | ASTER_IDENTITY_SET_MAX_KEY_SET_AGE
        );
        let issuers: Vec<TrustedIssuer> =
            serde_json::from_slice(unsafe { view(&back.trusted_issuers_json) }).unwrap();
        assert_eq!(issuers, cfg.identity_trusted_issuers);
        assert_eq!(unsafe { list_view(&back.remote_config_lock) }.len(), 1);
        assert_eq!(back.sources[0], 3);

        assert_eq!(
            unsafe { aster_config_set_identity(&mut c, std::ptr::null()) },
            0
        );
        assert!(
            !unsafe { lower_config(&c) }
                .unwrap()
                .identity_accept_on_behalf_of
        );
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
    }

    #[test]
    fn fill_carries_identity_and_sources() {
        let mut cfg = AsterConfig::builder().accept_on_behalf_of(true).build();
        cfg.remote_config_enabled = false;
        cfg.identity_trusted_issuers = serde_json::from_str(ISSUERS).unwrap();
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        fill_config(&cfg, &mut c);
        let lowered = unsafe { lower_config(&c) }.unwrap();
        assert!(lowered.identity_accept_on_behalf_of);
        assert!(!lowered.remote_config_enabled);
        assert_eq!(
            lowered.identity_trusted_issuers,
            cfg.identity_trusted_issuers
        );
        assert_eq!(
            lowered.setting_sources.get(SETTING_ACCEPT_ON_BEHALF_OF),
            cfg.setting_sources.get(SETTING_ACCEPT_ON_BEHALF_OF)
        );
        let mut back = view_out();
        assert_eq!(unsafe { aster_config_identity(&c, &mut back) }, 0);
        assert_eq!(back.remote_config_disabled, 1);
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
    }

    #[test]
    fn rejects_bad_input() {
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        c.struct_size = std::mem::size_of::<aster_config_t>() as u32;
        let mut i = view_out();
        i.struct_size = 8;
        assert_ne!(unsafe { aster_config_set_identity(&mut c, &i) }, 0);
        let mut i = view_out();
        i.accept_on_behalf_of = 2;
        assert_ne!(unsafe { aster_config_set_identity(&mut c, &i) }, 0);
        let mut i = view_out();
        i.set = 0x8;
        assert_ne!(unsafe { aster_config_set_identity(&mut c, &i) }, 0);
        let mut i = view_out();
        i.trusted_issuers_json = bytes("{}");
        assert_ne!(unsafe { aster_config_set_identity(&mut c, &i) }, 0);
        let mut i = view_out();
        i.publisher_json = bytes("[]");
        assert_ne!(unsafe { aster_config_set_identity(&mut c, &i) }, 0);
        let mut i = view_out();
        i.sources[5] = 1;
        assert_ne!(unsafe { aster_config_set_identity(&mut c, &i) }, 0);
        let mut i = view_out();
        i.sources[0] = 9;
        assert_ne!(unsafe { aster_config_set_identity(&mut c, &i) }, 0);
        assert!(c.storage.is_null());
        let mut out = view_out();
        out.struct_size = 4;
        assert_ne!(unsafe { aster_config_identity(&c, &mut out) }, 0);
    }
}
