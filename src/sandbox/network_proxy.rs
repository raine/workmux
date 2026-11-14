//! HTTP CONNECT proxy for domain-based network restrictions.
//!
//! Runs as a host-resident proxy alongside the RPC server. Containers set
//! `HTTPS_PROXY` / `HTTP_PROXY` env vars to route all outbound HTTPS through
//! this proxy. The proxy verifies auth, checks domain allowlist, resolves DNS
//! on the host side (rejecting private IPs), and tunnels traffic.
//!
//! Combined with iptables inside the container (default-deny egress, only
//! allow proxy and RPC ports), this prevents the sandbox from accessing
//! unapproved destinations even if it ignores the proxy env vars.

use anyhow::{Context, Result};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::config::AllowedDomainRule;
use crate::sandbox::constant_time::constant_time_eq;
use crate::sandbox::rpc::generate_token;

/// Maximum size of the CONNECT request (line + headers).
/// Prevents memory exhaustion from oversized requests.
const MAX_REQUEST_SIZE: usize = 8 * 1024;

/// Deadline for reading the initial CONNECT request.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

/// Time allowed for an overloaded client to receive the proxy error.
const REJECT_LINGER_TIMEOUT: Duration = Duration::from_millis(100);

/// Timeout for connecting to the upstream target.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The only destination port allowed via CONNECT.
const ALLOWED_PORT: u16 = 443;

/// HTTP CONNECT proxy server with domain allowlist and private IP rejection.
pub struct NetworkProxy {
    listener: TcpListener,
    port: u16,
    token: String,
    allowed_domains: Vec<AllowedDomainRule>,
    max_connections: usize,
}

/// Handle to a running proxy server thread.
pub struct ProxyHandle {
    _handle: thread::JoinHandle<()>,
    _limit: Arc<ConnectionLimit>,
}

struct ConnectionLimit {
    active: AtomicUsize,
    max: usize,
}

struct ConnectionPermit {
    limit: Arc<ConnectionLimit>,
}

impl ConnectionLimit {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            max,
        })
    }

    fn try_acquire(self: &Arc<Self>) -> Result<ConnectionPermit, usize> {
        let mut active = self.active.load(Ordering::Relaxed);
        loop {
            if active >= self.max {
                return Err(active);
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(ConnectionPermit {
                        limit: Arc::clone(self),
                    });
                }
                Err(current) => active = current,
            }
        }
    }

    #[cfg(test)]
    fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.limit.active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl NetworkProxy {
    /// Bind to a random port on all interfaces (same as RPC server).
    pub fn bind(allowed_domains: &[AllowedDomainRule], max_connections: usize) -> Result<Self> {
        Self::bind_to(
            std::net::Ipv4Addr::UNSPECIFIED.into(),
            allowed_domains,
            max_connections,
        )
    }

    /// Listen on an ephemeral port of `ip` only.
    pub fn bind_to(
        ip: std::net::IpAddr,
        allowed_domains: &[AllowedDomainRule],
        max_connections: usize,
    ) -> Result<Self> {
        let listener = TcpListener::bind((ip, 0))
            .with_context(|| format!("Failed to bind network proxy listener on {ip}"))?;
        let port = listener.local_addr()?.port();
        let token = generate_token();
        debug!(port, "network proxy bound");
        Ok(Self {
            listener,
            port,
            token,
            allowed_domains: allowed_domains.to_vec(),
            max_connections,
        })
    }

    /// Get the port the proxy is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the auth token for this proxy session.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Spawn the proxy accept loop in a background thread.
    pub fn spawn(self) -> ProxyHandle {
        let ctx = Arc::new(ProxyContext {
            token: self.token,
            allowed_domains: self.allowed_domains,
        });
        let limit = ConnectionLimit::new(self.max_connections);
        let accept_limit = Arc::clone(&limit);

        let handle = thread::spawn(move || {
            for stream in self.listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let permit = match accept_limit.try_acquire() {
                            Ok(permit) => permit,
                            Err(current) => {
                                warn!(current, "proxy connection limit reached, rejecting");
                                reject_over_capacity(stream);
                                continue;
                            }
                        };
                        let ctx = Arc::clone(&ctx);
                        thread::spawn(move || {
                            let _permit = permit;
                            if let Err(e) = handle_proxy_connection(stream, &ctx) {
                                debug!(error = %e, "proxy connection ended");
                            }
                        });
                    }
                    Err(e) => {
                        debug!(error = %e, "proxy accept error, shutting down");
                        break;
                    }
                }
            }
        });

        ProxyHandle {
            _handle: handle,
            _limit: limit,
        }
    }
}

impl ProxyHandle {
    #[cfg(test)]
    fn active_connections(&self) -> usize {
        self._limit.active()
    }
}

/// Shared context for proxy connection handlers.
struct ProxyContext {
    token: String,
    allowed_domains: Vec<AllowedDomainRule>,
}

/// Check if a domain matches a pattern (case-insensitive).
///
/// Supports exact match and wildcard prefix (`*.example.com` matches
/// `foo.example.com` but not `example.com` itself).
fn domain_matches(domain: &str, pattern: &str) -> bool {
    let domain = domain.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix('*') {
        // suffix is ".example.com"
        domain.ends_with(&suffix)
    } else {
        domain == pattern
    }
}

fn matching_rule<'a>(
    domain: &str,
    rules: &'a [AllowedDomainRule],
) -> Option<&'a AllowedDomainRule> {
    rules
        .iter()
        .filter(|rule| domain_matches(domain, &rule.host))
        .max_by_key(|rule| match rule.host.strip_prefix("*.") {
            Some(suffix) => suffix.len(),
            None => usize::MAX,
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddressClass {
    Public,
    PrivateRoutable,
    AlwaysBlocked,
}

fn classify_ip(addr: &IpAddr) -> AddressClass {
    match addr {
        IpAddr::V4(ip) => {
            if ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_unspecified()
                || ip.is_multicast()
            {
                AddressClass::AlwaysBlocked
            } else if ip.is_private()
                // CGNAT range 100.64.0.0/10
                || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xC0) == 64)
            {
                AddressClass::PrivateRoutable
            } else {
                AddressClass::Public
            }
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return classify_ip(&IpAddr::V4(mapped));
            }
            if ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                // Link-local fe80::/10
                || (ip.segments()[0] & 0xffc0) == 0xfe80
            {
                AddressClass::AlwaysBlocked
            } else if (ip.segments()[0] & 0xfe00) == 0xfc00 {
                // ULA fc00::/7
                AddressClass::PrivateRoutable
            } else {
                AddressClass::Public
            }
        }
    }
}

/// Check if an IP address is private/reserved and should be blocked.
#[cfg(test)]
fn is_private_ip(addr: &IpAddr) -> bool {
    classify_ip(addr) != AddressClass::Public
}

fn allowed_target_addrs(addrs: &[SocketAddr], allow_private_ips: bool) -> Vec<&SocketAddr> {
    addrs
        .iter()
        .filter(|addr| match classify_ip(&addr.ip()) {
            AddressClass::Public => true,
            AddressClass::PrivateRoutable => allow_private_ips,
            AddressClass::AlwaysBlocked => false,
        })
        .collect()
}

fn read_bounded_request_line(
    reader: &mut BufReader<TcpStream>,
    output: &mut String,
    total_read: &mut usize,
    deadline: Instant,
) -> io::Result<usize> {
    let start_len = output.len();

    loop {
        if *total_read >= MAX_REQUEST_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "proxy request exceeds size limit",
            ));
        }

        let remaining_time = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "proxy request timed out"))?;
        reader
            .get_ref()
            .set_read_timeout(Some(remaining_time.max(Duration::from_millis(1))))?;

        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(output.len() - start_len);
        }

        let line_end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if *total_read + line_end > MAX_REQUEST_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "proxy request exceeds size limit",
            ));
        }

        let text = std::str::from_utf8(&available[..line_end]).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "proxy request is not UTF-8")
        })?;
        let has_newline = text.ends_with('\n');
        output.push_str(text);
        reader.consume(line_end);
        *total_read += line_end;

        if has_newline {
            return Ok(output.len() - start_len);
        }
    }
}

fn write_request_read_error(writer: &mut impl Write, error: io::Error) -> Result<()> {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            write_error(writer, 408, "Proxy request timed out")
        }
        io::ErrorKind::InvalidData => write_error(writer, 400, "Request too large or invalid"),
        _ => Err(error.into()),
    }
}

fn reject_over_capacity(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(REJECT_LINGER_TIMEOUT));
    let _ = write_error(
        &mut stream,
        503,
        "workmux network proxy connection limit reached",
    );
    let _ = stream.shutdown(Shutdown::Write);

    let _ = stream.set_read_timeout(Some(REJECT_LINGER_TIMEOUT));
    let mut drained = 0usize;
    let mut buffer = [0u8; 1024];
    while drained < MAX_REQUEST_SIZE {
        let remaining = MAX_REQUEST_SIZE - drained;
        let read_size = remaining.min(buffer.len());
        match stream.read(&mut buffer[..read_size]) {
            Ok(0) | Err(_) => break,
            Ok(count) => drained += count,
        }
    }
}

/// Parse and handle a single proxy connection.
fn handle_proxy_connection(stream: TcpStream, ctx: &ProxyContext) -> Result<()> {
    let peer = stream.peer_addr().ok();
    debug!(?peer, "proxy connection accepted");

    let deadline = Instant::now() + AUTH_TIMEOUT;
    let mut reader = BufReader::new(stream.try_clone().context("Failed to clone proxy stream")?);
    let mut writer = &stream;

    let mut total_read = 0usize;
    let mut request_line = String::new();
    let mut proxy_auth: Option<String> = None;

    let n = match read_bounded_request_line(
        &mut reader,
        &mut request_line,
        &mut total_read,
        deadline,
    ) {
        Ok(n) => n,
        Err(error) => {
            write_request_read_error(&mut writer, error)?;
            return Ok(());
        }
    };
    debug!(
        ?peer,
        request_line = request_line.trim(),
        bytes = n,
        "proxy request line"
    );

    loop {
        let mut header_line = String::new();
        if let Err(error) =
            read_bounded_request_line(&mut reader, &mut header_line, &mut total_read, deadline)
        {
            write_request_read_error(&mut writer, error)?;
            return Ok(());
        }

        let trimmed = header_line.trim();
        if trimmed.is_empty() {
            break;
        }

        // Parse Proxy-Authorization header (case-insensitive per HTTP spec)
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("Proxy-Authorization")
        {
            proxy_auth = Some(value.trim().to_string());
        }
    }

    // Parse CONNECT request line: "CONNECT host:port HTTP/1.1\r\n"
    let request_line = request_line.trim();
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 || parts[0] != "CONNECT" {
        write_error(&mut writer, 400, "Expected CONNECT method")?;
        return Ok(());
    }

    let target = parts[1];
    let (hostname, port) = parse_host_port(target)?;

    debug!(hostname, port, "CONNECT request");

    // Verify auth token
    let expected = format!("Basic {}", base64_encode(&format!("workmux:{}", ctx.token)));
    match proxy_auth {
        None => {
            debug!(hostname, "proxy auth missing");
            write_error(&mut writer, 407, "Proxy authentication required")?;
            return Ok(());
        }
        Some(ref auth) if !constant_time_eq(auth.as_bytes(), expected.as_bytes()) => {
            debug!(hostname, "proxy auth failed");
            write_error(&mut writer, 407, "Invalid proxy credentials")?;
            return Ok(());
        }
        _ => {}
    }

    // Clear read timeout for authenticated tunneling
    stream.set_read_timeout(None)?;

    // Reject non-443 ports
    if port != ALLOWED_PORT {
        warn!(hostname, port, "rejected: non-443 port");
        write_error(&mut writer, 403, "Only port 443 is allowed")?;
        return Ok(());
    }

    // Normalize hostname
    let hostname = hostname.to_ascii_lowercase();
    let hostname = hostname.strip_suffix('.').unwrap_or(&hostname);

    // Reject IP literal hostnames
    if hostname.parse::<IpAddr>().is_ok() {
        warn!(hostname, "rejected: IP literal hostname");
        write_error(&mut writer, 403, "IP literal hostnames not allowed")?;
        return Ok(());
    }

    // Check domain allowlist
    let Some(rule) = matching_rule(hostname, &ctx.allowed_domains) else {
        warn!(hostname, "rejected: domain not in allowlist");
        write_error(&mut writer, 403, "Domain not allowed")?;
        return Ok(());
    };

    // Resolve DNS on host side
    let addrs: Vec<SocketAddr> = match format!("{}:{}", hostname, port).to_socket_addrs() {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            warn!(hostname, error = %e, "DNS resolution failed");
            write_error(&mut writer, 502, "DNS resolution failed")?;
            return Ok(());
        }
    };

    // Filter out disallowed IPs
    let allowed_addrs = allowed_target_addrs(&addrs, rule.allow_private_ips);

    if allowed_addrs.is_empty() {
        warn!(hostname, "rejected: no resolved IPs are allowed");
        write_error(&mut writer, 403, "No resolved IPs are allowed")?;
        return Ok(());
    }

    // Connect to first allowed IP (TOCTOU-safe: use validated SocketAddr directly)
    let target_addr = *allowed_addrs[0];
    let mut target_stream = match TcpStream::connect_timeout(&target_addr, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            debug!(hostname, addr = %target_addr, error = %e, "connect failed");
            write_error(&mut writer, 502, "Connection to target failed")?;
            return Ok(());
        }
    };

    debug!(hostname, addr = %target_addr, "tunnel established");

    // Send 200 Connection Established
    writer.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    writer.flush()?;

    // Drain any bytes the BufReader consumed from the socket but hasn't yielded
    // (e.g. a TLS ClientHello pipelined in the same TCP segment as the CONNECT).
    let buffered = reader.buffer();
    if !buffered.is_empty() {
        target_stream
            .write_all(buffered)
            .context("Failed to forward buffered data to target")?;
    }
    drop(reader);

    // Bidirectional tunnel
    tunnel(stream, target_stream)?;

    Ok(())
}

/// Parse "host:port" from CONNECT target.
fn parse_host_port(target: &str) -> Result<(&str, u16)> {
    // Handle IPv6 literals like [::1]:443
    if let Some(bracket_end) = target.find(']') {
        let host = &target[..=bracket_end];
        let port_str = target[bracket_end + 1..].strip_prefix(':').unwrap_or("443");
        let port: u16 = port_str.parse().context("Invalid port")?;
        return Ok((host, port));
    }

    match target.rsplit_once(':') {
        Some((host, port_str)) => {
            let port: u16 = port_str.parse().context("Invalid port")?;
            Ok((host, port))
        }
        None => Ok((target, 443)),
    }
}

/// Write an HTTP error response.
fn write_error(writer: &mut impl Write, code: u16, message: &str) -> Result<()> {
    let status = match code {
        400 => "Bad Request",
        403 => "Forbidden",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        code,
        status,
        message.len(),
        message,
    );
    writer.write_all(response.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Simple base64 encoding (avoids adding a dependency for this one use).
fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        result.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }

    result
}

/// Bidirectional byte tunnel between two TCP streams.
fn tunnel(mut client: TcpStream, mut target: TcpStream) -> Result<()> {
    let mut target_read = target.try_clone()?;
    let mut client_write = client.try_clone()?;

    let target_to_client = thread::spawn(move || {
        let _ = std::io::copy(&mut target_read, &mut client_write);
        let _ = client_write.shutdown(Shutdown::Write);
    });

    let _ = std::io::copy(&mut client, &mut target);
    let _ = target.shutdown(Shutdown::Write);
    target_to_client.join().ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(host: &str, allow_private_ips: bool) -> AllowedDomainRule {
        AllowedDomainRule {
            host: host.to_string(),
            allow_private_ips,
        }
    }

    fn addr(s: &str) -> SocketAddr {
        format!("{}:443", s).parse().unwrap()
    }

    struct SpawnedProxy {
        port: u16,
        token: String,
        _handle: ProxyHandle,
    }

    fn spawn_test_proxy(rules: &[AllowedDomainRule]) -> SpawnedProxy {
        spawn_test_proxy_with_limit(rules, 128)
    }

    fn spawn_test_proxy_with_limit(
        rules: &[AllowedDomainRule],
        max_connections: usize,
    ) -> SpawnedProxy {
        let proxy = NetworkProxy::bind(rules, max_connections).unwrap();
        let port = proxy.port();
        let token = proxy.token().to_string();
        let handle = proxy.spawn();
        std::thread::sleep(Duration::from_millis(50));
        SpawnedProxy {
            port,
            token,
            _handle: handle,
        }
    }

    fn proxy_auth(token: &str) -> String {
        format!("Basic {}", base64_encode(&format!("workmux:{token}")))
    }

    fn proxy_connect_request(host_port: &str, auth_header: &str, auth: &str) -> String {
        format!("CONNECT {host_port} HTTP/1.1\r\n{auth_header}: {auth}\r\n\r\n")
    }

    fn proxy_request_status_line(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut response = String::new();
        let mut reader = BufReader::new(&stream);
        reader.read_line(&mut response).unwrap();
        response
    }

    // ── domain_matches tests ────────────────────────────────────────────

    #[test]
    fn domain_exact_match() {
        assert!(domain_matches("example.com", "example.com"));
        assert!(domain_matches("api.anthropic.com", "api.anthropic.com"));
    }

    #[test]
    fn domain_case_insensitive() {
        assert!(domain_matches("Example.COM", "example.com"));
        assert!(domain_matches("example.com", "Example.COM"));
    }

    #[test]
    fn domain_wildcard_match() {
        assert!(domain_matches("foo.googleapis.com", "*.googleapis.com"));
        assert!(domain_matches("bar.baz.googleapis.com", "*.googleapis.com"));
    }

    #[test]
    fn domain_wildcard_does_not_match_base() {
        // *.example.com should NOT match example.com itself (standard behavior)
        assert!(!domain_matches("example.com", "*.example.com"));
    }

    #[test]
    fn domain_no_match() {
        assert!(!domain_matches("evil.com", "example.com"));
        assert!(!domain_matches("notexample.com", "example.com"));
        assert!(!domain_matches("evil.com", "*.example.com"));
    }

    #[test]
    fn matching_rule_exact_beats_wildcard_regardless_of_order() {
        let rules = vec![
            rule("*.example.com", false),
            rule("artifactory.example.com", true),
        ];
        let matched = matching_rule("artifactory.example.com", &rules).unwrap();
        assert_eq!(matched.host, "artifactory.example.com");
        assert!(matched.allow_private_ips);
    }

    #[test]
    fn matching_rule_longest_wildcard_wins() {
        let rules = vec![
            rule("*.example.com", false),
            rule("*.internal.example.com", true),
        ];
        let matched = matching_rule("npm.internal.example.com", &rules).unwrap();
        assert_eq!(matched.host, "*.internal.example.com");
        assert!(matched.allow_private_ips);
    }

    // ── is_private_ip tests ─────────────────────────────────────────────

    #[test]
    fn private_ip_rfc1918() {
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn private_ip_loopback() {
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"::1".parse().unwrap()));
    }

    #[test]
    fn private_ip_link_local() {
        assert!(is_private_ip(&"169.254.1.1".parse().unwrap()));
    }

    #[test]
    fn private_ip_cgnat() {
        assert!(is_private_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_private_ip(&"100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn private_ip_multicast() {
        assert!(is_private_ip(&"224.0.0.1".parse().unwrap()));
    }

    #[test]
    fn private_ip_ipv6_ula() {
        assert!(is_private_ip(&"fc00::1".parse().unwrap()));
        assert!(is_private_ip(&"fd12::1".parse().unwrap()));
    }

    #[test]
    fn private_ip_ipv6_link_local() {
        assert!(is_private_ip(&"fe80::1".parse().unwrap()));
    }

    #[test]
    fn private_ip_v4_mapped_v6() {
        // ::ffff:127.0.0.1 is IPv4-mapped IPv6 for loopback
        assert!(is_private_ip(&"::ffff:127.0.0.1".parse().unwrap()));
        // ::ffff:10.0.0.1 is IPv4-mapped IPv6 for RFC1918
        assert!(is_private_ip(&"::ffff:10.0.0.1".parse().unwrap()));
        // ::ffff:8.8.8.8 is IPv4-mapped IPv6 for public IP
        assert!(!is_private_ip(&"::ffff:8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn public_ip_allowed() {
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip(&"2607:f8b0:4004:800::200e".parse().unwrap()));
    }

    #[test]
    fn not_private_ip_100_non_cgnat() {
        // 100.0.0.1 is NOT in CGNAT range (100.64.0.0/10)
        assert!(!is_private_ip(&"100.0.0.1".parse().unwrap()));
    }

    #[test]
    fn allowed_target_addrs_rejects_private_without_opt_in() {
        let addrs = vec![addr("10.0.0.1"), addr("8.8.8.8")];
        let allowed = allowed_target_addrs(&addrs, false);
        assert_eq!(allowed, vec![&addrs[1]]);
    }

    #[test]
    fn allowed_target_addrs_accepts_private_routable_with_opt_in() {
        let addrs = vec![
            addr("10.0.0.1"),
            addr("100.64.0.1"),
            "[fd12::1]:443".parse().unwrap(),
        ];
        let allowed = allowed_target_addrs(&addrs, true);
        assert_eq!(allowed, vec![&addrs[0], &addrs[1], &addrs[2]]);
    }

    #[test]
    fn allowed_target_addrs_rejects_always_blocked_with_opt_in() {
        let addrs = vec![
            addr("127.0.0.1"),
            addr("169.254.1.1"),
            addr("0.0.0.0"),
            addr("224.0.0.1"),
            addr("255.255.255.255"),
            "[::1]:443".parse().unwrap(),
            "[fe80::1]:443".parse().unwrap(),
        ];
        assert!(allowed_target_addrs(&addrs, true).is_empty());
    }

    #[test]
    fn allowed_target_addrs_classifies_ipv4_mapped_ipv6() {
        let addrs = vec![
            "[::ffff:8.8.8.8]:443".parse().unwrap(),
            "[::ffff:10.0.0.1]:443".parse().unwrap(),
            "[::ffff:127.0.0.1]:443".parse().unwrap(),
        ];
        let without_private = allowed_target_addrs(&addrs, false);
        assert_eq!(without_private, vec![&addrs[0]]);
        let with_private = allowed_target_addrs(&addrs, true);
        assert_eq!(with_private, vec![&addrs[0], &addrs[1]]);
    }

    // ── parse_host_port tests ───────────────────────────────────────────

    #[test]
    fn parse_host_port_standard() {
        let (host, port) = parse_host_port("example.com:443").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn parse_host_port_non_standard() {
        let (host, port) = parse_host_port("example.com:8443").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 8443);
    }

    #[test]
    fn parse_host_port_no_port() {
        let (host, port) = parse_host_port("example.com").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    // ── base64 encoding test ────────────────────────────────────────────

    #[test]
    fn base64_encode_basic_auth() {
        assert_eq!(base64_encode("workmux:mytoken"), "d29ya211eDpteXRva2Vu");
        assert_eq!(base64_encode(""), "");
        assert_eq!(base64_encode("a"), "YQ==");
        assert_eq!(base64_encode("ab"), "YWI=");
        assert_eq!(base64_encode("abc"), "YWJj");
    }

    // ── proxy server lifecycle tests ────────────────────────────────────

    #[test]
    fn proxy_binds_to_random_port() {
        let proxy = NetworkProxy::bind(&[rule("example.com", false)], 128).unwrap();
        assert!(proxy.port() > 0);
    }

    #[test]
    fn proxy_token_is_nonempty() {
        let proxy = NetworkProxy::bind(&[], 128).unwrap();
        assert!(!proxy.token().is_empty());
    }

    fn wait_for_active_connections(proxy: &SpawnedProxy, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while proxy._handle.active_connections() != expected {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected} active proxy connections"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn proxy_returns_503_at_connection_limit() {
        let proxy = spawn_test_proxy_with_limit(&[], 1);
        let holder = TcpStream::connect(("127.0.0.1", proxy.port)).unwrap();
        wait_for_active_connections(&proxy, 1);

        let mut overloaded = TcpStream::connect(("127.0.0.1", proxy.port)).unwrap();
        overloaded
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\nTLS")
            .unwrap();
        overloaded
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut response = String::new();
        overloaded.read_to_string(&mut response).unwrap();

        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(response.contains("Connection: close\r\n"));
        assert!(response.ends_with("workmux network proxy connection limit reached"));

        drop(holder);
        wait_for_active_connections(&proxy, 0);
    }

    #[test]
    fn proxy_admits_connection_after_slot_is_released() {
        let proxy = spawn_test_proxy_with_limit(&[], 1);
        let holder = TcpStream::connect(("127.0.0.1", proxy.port)).unwrap();
        wait_for_active_connections(&proxy, 1);
        drop(holder);
        wait_for_active_connections(&proxy, 0);

        let response =
            proxy_request_status_line(proxy.port, "CONNECT example.com:443 HTTP/1.1\r\n\r\n");
        assert!(response.contains("407"));
    }

    #[test]
    fn connection_limit_rejects_zero_capacity() {
        let limit = ConnectionLimit::new(0);
        assert!(matches!(limit.try_acquire(), Err(0)));
        assert_eq!(limit.active(), 0);
    }

    #[test]
    fn connection_limit_is_preserved_under_contention() {
        let limit = ConnectionLimit::new(4);
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let limit = Arc::clone(&limit);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let permit = limit.try_acquire().ok();
                    barrier.wait();
                    permit
                })
            })
            .collect();
        let permits: Vec<_> = handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(permits.len(), 4);
        assert_eq!(limit.active(), 4);
        drop(permits);
        assert_eq!(limit.active(), 0);
        assert!(limit.try_acquire().is_ok());
    }

    #[test]
    fn connection_permit_is_released_after_panic() {
        let limit = ConnectionLimit::new(1);
        let permit = limit.try_acquire().unwrap();
        let handle = thread::spawn(move || {
            let _permit = permit;
            panic!("test panic");
        });
        assert!(handle.join().is_err());
        assert_eq!(limit.active(), 0);
        assert!(limit.try_acquire().is_ok());
    }

    #[test]
    fn oversized_unterminated_request_line_is_rejected() {
        let proxy = spawn_test_proxy(&[]);
        let mut stream = TcpStream::connect(("127.0.0.1", proxy.port)).unwrap();
        stream.write_all(&vec![b'x'; MAX_REQUEST_SIZE + 1]).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let mut response = String::new();
        BufReader::new(&stream).read_line(&mut response).unwrap();
        assert!(response.contains("400"));
    }

    #[test]
    fn request_deadline_stops_dribbling_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            for _ in 0..10 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut output = String::new();
        let mut total_read = 0;

        let error = read_bounded_request_line(
            &mut reader,
            &mut output,
            &mut total_read,
            Instant::now() + Duration::from_millis(30),
        )
        .unwrap_err();

        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        client.join().unwrap();
    }

    #[test]
    fn proxy_rejects_missing_auth() {
        let proxy = spawn_test_proxy(&[rule("example.com", false)]);
        let response =
            proxy_request_status_line(proxy.port, "CONNECT example.com:443 HTTP/1.1\r\n\r\n");
        assert!(response.contains("407"));
    }

    #[test]
    fn proxy_rejects_wrong_auth() {
        let proxy = spawn_test_proxy(&[rule("example.com", false)]);
        let request = proxy_connect_request(
            "example.com:443",
            "Proxy-Authorization",
            &proxy_auth("wrong-token"),
        );
        let response = proxy_request_status_line(proxy.port, &request);
        assert!(response.contains("407"));
    }

    #[test]
    fn proxy_accepts_lowercase_auth_header() {
        let proxy = spawn_test_proxy(&[rule("example.com", false)]);
        let request = proxy_connect_request(
            "example.com:443",
            "proxy-authorization",
            &proxy_auth(&proxy.token),
        );
        let response = proxy_request_status_line(proxy.port, &request);
        // Should NOT be 407 -- lowercase header must be accepted
        assert!(
            !response.contains("407"),
            "lowercase proxy-authorization should be accepted, got: {}",
            response.trim()
        );
    }

    #[test]
    fn proxy_rejects_non_443_port() {
        let proxy = spawn_test_proxy(&[rule("example.com", false)]);
        let request = proxy_connect_request(
            "example.com:80",
            "Proxy-Authorization",
            &proxy_auth(&proxy.token),
        );
        let response = proxy_request_status_line(proxy.port, &request);
        assert!(response.contains("403"));
    }

    #[test]
    fn proxy_rejects_unlisted_domain() {
        let proxy = spawn_test_proxy(&[rule("allowed.com", false)]);
        let request = proxy_connect_request(
            "denied.com:443",
            "Proxy-Authorization",
            &proxy_auth(&proxy.token),
        );
        let response = proxy_request_status_line(proxy.port, &request);
        assert!(response.contains("403"));
    }

    /// Verify that bytes pipelined after CONNECT headers (e.g. a TLS
    /// ClientHello in the same TCP segment) are forwarded to the target
    /// rather than silently dropped by BufReader::into_inner().
    #[test]
    fn pipelined_data_forwarded_through_tunnel() {
        use std::io::Read;

        // "Target" server that will receive forwarded data
        let target_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_addr = target_listener.local_addr().unwrap();

        // Accept target connection in background and read the forwarded bytes
        let target_handle = thread::spawn(move || {
            let (mut conn, _) = target_listener.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = vec![0u8; 26];
            conn.read_exact(&mut buf).unwrap();
            buf
        });

        // Simulated proxy listener
        let proxy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        // Build pipelined payload: CONNECT headers + extra data in one write
        let extra_data = b"SIMULATED_TLS_CLIENT_HELLO";
        let mut pipelined = Vec::new();
        pipelined
            .extend_from_slice(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n");
        pipelined.extend_from_slice(extra_data);

        // Client sends everything at once (simulates pipelining)
        let mut client = TcpStream::connect(proxy_addr).unwrap();
        client.write_all(&pipelined).unwrap();
        client.flush().unwrap();

        // Proxy accepts and reads with BufReader (mirrors handle_proxy_connection)
        let (proxy_stream, _) = proxy_listener.accept().unwrap();
        // Ensure all data is in kernel buffer before BufReader reads
        thread::sleep(Duration::from_millis(50));
        let mut reader = BufReader::new(&proxy_stream);

        // Parse headers
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
        }

        // Connect to target and drain buffered bytes (the fix under test)
        let mut target_stream = TcpStream::connect(target_addr).unwrap();
        let buffer = reader.buffer();
        assert!(
            !buffer.is_empty(),
            "BufReader should have buffered the pipelined data"
        );
        target_stream.write_all(buffer).unwrap();
        target_stream.flush().unwrap();
        drop(target_stream);

        // Verify target received exactly the pipelined data
        let received = target_handle.join().unwrap();
        assert_eq!(received, extra_data);
    }

    #[test]
    fn proxy_rejects_ip_literal_hostname() {
        let proxy = spawn_test_proxy(&[rule("8.8.8.8", false)]);
        let request = proxy_connect_request(
            "8.8.8.8:443",
            "Proxy-Authorization",
            &proxy_auth(&proxy.token),
        );
        let response = proxy_request_status_line(proxy.port, &request);
        assert!(response.contains("403"));
    }
}
