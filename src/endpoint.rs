//! Default pool endpoint.
//!
//! cairn-miner is the open-pool counterpart to cairn-pool: the default endpoint
//! below is a plain, greppable constant — no obfuscation — and the `--pool`
//! flag overrides it freely. Your miner, your choice of pool.

/// Default Stratum v1 endpoint (`host:port`) used when no `--pool` flag and no
/// `pool =` config value is given: the public cairn pool.
pub const DEFAULT_POOL: &str = "cairn-pool.com:3333";

/// Return the default pool endpoint as an owned string.
pub fn pool_endpoint() -> String {
    DEFAULT_POOL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoint_is_host_port() {
        let ep = pool_endpoint();
        let (host, port) = ep.rsplit_once(':').expect("host:port");
        assert!(!host.is_empty());
        assert!(port.parse::<u16>().is_ok(), "port must be numeric: {port}");
    }
}
