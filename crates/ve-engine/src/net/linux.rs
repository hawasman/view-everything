//! `/proc/net/dev` on Linux: each interface's cumulative bytes received and sent.

use super::InterfaceCounters;
use std::sync::Arc;

/// The interfaces of `/proc/net/dev`, less loopback, which is no link. Names come from
/// `names`, so every sample shares one copy of each.
pub(crate) fn interface_counters(text: &str, names: &mut Vec<Arc<str>>) -> Vec<InterfaceCounters> {
    text.lines()
        .filter_map(|line| {
            // "  eth0: rx_bytes rx_packets ... (8 receive fields) tx_bytes ..."; the two header
            // lines hold no colon before their first field.
            let (name, fields) = line.split_once(':')?;
            let name = name.trim();
            if name.is_empty() || name == "lo" || name.contains('|') {
                return None;
            }
            let mut fields = fields.split_ascii_whitespace();
            let received = fields.next()?.parse().ok()?;
            let sent = fields.nth(7)?.parse().ok()?;
            let name = match names.iter().find(|n| &***n == name) {
                Some(n) => n.clone(),
                None => {
                    let n: Arc<str> = name.into();
                    names.push(n.clone());
                    n
                }
            };
            Some(InterfaceCounters { name, received, sent })
        })
        .collect()
}
