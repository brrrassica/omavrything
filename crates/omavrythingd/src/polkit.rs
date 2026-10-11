//! polkit authorization for the D-Bus control methods.
//!
//! The privileged control methods (`Rescan`, `MountNtfs`) are gated by the
//! `tech.alatcerdas.omavrything.control` action. `SetConfig` uses
//! `tech.alatcerdas.omavrything.set-config` for cosmetic keys and `control` for
//! privileged ones. Root is always allowed; other callers are checked against
//! the polkit authority, and denied if it is unavailable.

use std::collections::HashMap;

use zbus::Connection;
use zbus::zvariant::Value;

/// The polkit action that gates the privileged control methods.
pub const ACTION: &str = "tech.alatcerdas.omavrything.control";

/// The polkit action that gates cosmetic configuration changes. It is
/// `allow_active=yes`, so an active local user can change the UI settings
/// without a password prompt.
pub const CONFIG_ACTION: &str = "tech.alatcerdas.omavrything.set-config";

/// Whether `key` is a cosmetic setting that does not need the privileged
/// `control` action. Only the scratchpad opacity is purely cosmetic; every other
/// settable key affects indexing or resource use.
pub fn is_cosmetic_key(key: &str) -> bool {
    matches!(key, "background_opacity")
}

/// Whether `uid` is authorized for `action`.
///
/// The subject is the caller's D-Bus name (`system-bus-name`), not a bare uid:
/// polkit needs the bus name to resolve the caller's session and show an
/// authentication prompt. A `unix-user` subject cannot be prompted for, which is
/// why `SetConfig` used to fail instantly with `AccessDenied`.
pub async fn is_authorized(connection: &Connection, uid: u32, sender: &str, action: &str) -> bool {
    if uid == 0 {
        return true;
    }
    check_polkit(connection, sender, action).await.unwrap_or(false)
}

/// Build the polkit subject for a D-Bus caller. The `system-bus-name` subject
/// lets polkit resolve the caller's session and prompt for authentication.
fn subject(sender: &str) -> (&'static str, HashMap<String, Value<'_>>) {
    let mut details: HashMap<String, Value<'_>> = HashMap::new();
    details.insert("name".to_string(), Value::Str(sender.into()));
    ("system-bus-name", details)
}

async fn check_polkit(connection: &Connection, sender: &str, action: &str) -> zbus::Result<bool> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.PolicyKit1",
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
    )
    .await?;

    let subject = subject(sender);
    let details: HashMap<String, String> = HashMap::new();
    // AllowUserInteraction.
    let flags: u32 = 1;

    let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = proxy
        .call("CheckAuthorization", &(action, subject, details, flags))
        .await?;
    Ok(authorized)
}

/// The uid of a D-Bus peer, resolved via the bus daemon.
pub async fn peer_uid(connection: &Connection, sender: &str) -> zbus::Result<u32> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    proxy.call("GetConnectionUnixUser", &(sender,)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosmetic_keys_are_only_the_ui_setting() {
        assert!(is_cosmetic_key("background_opacity"));
        for key in [
            "excludes",
            "ntfs",
            "scan_interval",
            "snapshot_interval",
            "log_level",
        ] {
            assert!(!is_cosmetic_key(key), "{key} must be privileged");
        }
    }

    #[test]
    fn subject_is_a_system_bus_name() {
        let (kind, details) = subject(":1.42");
        assert_eq!(kind, "system-bus-name");
        match details.get("name") {
            Some(Value::Str(name)) => assert_eq!(name.as_ref(), ":1.42"),
            _ => panic!("subject detail 'name' is missing or not a string"),
        }
    }
}
