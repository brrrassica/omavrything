//! polkit authorization for the D-Bus control methods.
//!
//! Control methods (`Rescan`, `SetConfig`, `MountNtfs`) are gated by the
//! `tech.alatcerdas.omavrything.control` action. Root is always allowed; other
//! callers are checked against the polkit authority, and denied if it is
//! unavailable.

use std::collections::HashMap;

use zbus::Connection;
use zbus::zvariant::Value;

/// The polkit action that gates control methods.
pub const ACTION: &str = "tech.alatcerdas.omavrything.control";

/// Whether `uid` is authorized for the control action.
pub async fn is_authorized(connection: &Connection, uid: u32) -> bool {
    if uid == 0 {
        return true;
    }
    check_polkit(connection, uid).await.unwrap_or(false)
}

async fn check_polkit(connection: &Connection, uid: u32) -> zbus::Result<bool> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.PolicyKit1",
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
    )
    .await?;

    let mut subject_details: HashMap<String, Value<'_>> = HashMap::new();
    subject_details.insert("uid".to_string(), Value::U32(uid));
    let subject = ("unix-user", subject_details);
    let details: HashMap<String, String> = HashMap::new();
    // AllowUserInteraction.
    let flags: u32 = 1;

    let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = proxy
        .call("CheckAuthorization", &(ACTION, subject, details, flags))
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
