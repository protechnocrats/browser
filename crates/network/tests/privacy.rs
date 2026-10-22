//! Milestone 37 against real local servers: the filter blocks before
//! sending (redirect hops too), routes fail closed, a proxy route really
//! goes through the proxy, the VPN gate holds, and DNS over HTTPS resolves
//! strictly (ADR 011, 012, 019).

use network::{
    Destination, DohConfig, FilterCategory, FilterEngine, Initiator, NetworkConfig, NetworkContext, NetworkError, ProxyConfig, RequestContext,
    RouteRules,
};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A server that answers every request with a small page — `/hop` with a
/// redirect to `/pixel/2.gif` — and records each request line.
fn serve() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                }
                log.lock().unwrap().push(line.trim().to_string());
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                if path == "/hop" {
                    let _ = write!(stream, "HTTP/1.1 302 Found\r\nLocation: /pixel/2.gif\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                } else {
                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                }
            });
        }
    });
    (port, seen)
}

/// A minimal HTTP proxy — `CONNECT` tunnels and absolute-form requests —
/// that counts the connections it carried.
fn connect_proxy() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let carried = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&carried);
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { continue };
            let count = Arc::clone(&count);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(client.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push_str(&line);
                    if line.trim().is_empty() {
                        break;
                    }
                }
                let mut words = head.split_whitespace();
                let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
                let authority = if method == "CONNECT" {
                    target.to_string()
                } else {
                    target.trim_start_matches("http://").split('/').next().unwrap_or("").to_string()
                };
                let Ok(mut server) = TcpStream::connect(&authority) else { return };
                count.fetch_add(1, Ordering::SeqCst);
                let mut client = client;
                if method == "CONNECT" {
                    let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
                } else {
                    let _ = server.write_all(head.as_bytes());
                }
                let (mut to_client, mut from_server) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut from_server, &mut to_client);
                    let _ = to_client.shutdown(std::net::Shutdown::Both);
                });
                let _ = std::io::copy(&mut reader, &mut server);
            });
        }
    });
    (port, carried)
}

fn page_request(page: &str) -> RequestContext {
    RequestContext::subresource_by(Initiator::for_url(page)).with_destination(Destination::Image)
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[test]
fn the_filter_blocks_before_anything_is_sent_including_redirect_hops() {
    let (port, seen) = serve();
    let network = NetworkContext::new();
    let mut filter = FilterEngine::new();
    filter.add_list(FilterCategory::Trackers, "/pixel/*\n").unwrap();
    network.set_filter(filter);
    let base = format!("http://127.0.0.1:{port}");
    let ctx = page_request("https://news.example/");

    match network.fetch_with(&format!("{base}/pixel/1.gif"), &ctx) {
        Err(NetworkError::Blocked(why)) => assert!(why.starts_with("trackers"), "{why}"),
        other => panic!("expected a block, got {other:?}"),
    }
    assert!(seen.lock().unwrap().is_empty(), "the server never saw it");

    // A redirect to a blocked URL: the first hop goes out, the second never.
    let result = network.fetch_with(&format!("{base}/hop"), &ctx);
    assert!(matches!(result, Err(NetworkError::Blocked(_))), "{result:?}");
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "{requests:?}");

    // An ordinary request still works.
    assert_eq!(network.fetch_with(&format!("{base}/image.png"), &ctx).unwrap().status, 200);
}

#[test]
fn an_unreachable_proxy_fails_hard_and_never_connects_directly() {
    let (port, seen) = serve();
    for proxy in [format!("http://127.0.0.1:{}", closed_port()), format!("socks5://127.0.0.1:{}", closed_port())] {
        let config = NetworkConfig {
            proxies: vec![("proxy".into(), ProxyConfig { url: proxy.clone() })],
            rules: RouteRules { profile_default: Some("proxy".into()), ..RouteRules::default() },
            ..NetworkConfig::default()
        };
        let (network, _) = NetworkContext::with_config(&config).unwrap();
        let before = diagnostics::metrics::snapshot();
        let result = network.fetch_with(&format!("http://127.0.0.1:{port}/"), &RequestContext::typed_navigation());
        let Err(NetworkError::Transport(why)) = result else { panic!("{proxy}: expected a failure, got {result:?}") };
        assert!(why.contains("no fallback"), "{why}");
        assert!(diagnostics::metrics::snapshot().delta(&before, "guard.route_failed_closed") >= 1);
    }
    assert!(seen.lock().unwrap().is_empty(), "nothing reached the server directly");
}

#[test]
fn a_proxy_route_goes_through_the_proxy_and_site_rules_pick_it() {
    let (port, seen) = serve();
    let (proxy_port, tunnels) = connect_proxy();
    let config = NetworkConfig {
        proxies: vec![("work".into(), ProxyConfig { url: format!("http://127.0.0.1:{proxy_port}") })],
        rules: RouteRules { site_rules: vec![("intranet.example".into(), "work".into())], ..RouteRules::default() },
        ..NetworkConfig::default()
    };
    let (network, _) = NetworkContext::with_config(&config).unwrap();
    let target = format!("http://127.0.0.1:{port}/a");
    // A page of the ruled site: through the proxy.
    assert_eq!(network.fetch_with(&target, &page_request("https://wiki.intranet.example/")).unwrap().status, 200);
    assert_eq!(tunnels.load(Ordering::SeqCst), 1);
    // Another site's page: direct.
    assert_eq!(network.fetch_with(&target, &page_request("https://news.example/")).unwrap().status, 200);
    assert_eq!(tunnels.load(Ordering::SeqCst), 1, "not through the proxy");
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn private_mode_overrides_every_other_rule() {
    let (port, _) = serve();
    let (proxy_port, tunnels) = connect_proxy();
    let config = NetworkConfig {
        proxies: vec![("private".into(), ProxyConfig { url: format!("http://127.0.0.1:{proxy_port}") })],
        rules: RouteRules { private_override: Some("private".into()), ..RouteRules::default() },
        ..NetworkConfig::default()
    };
    let (network, _) = NetworkContext::with_config(&config).unwrap();
    let target = format!("http://127.0.0.1:{port}/");
    network.fetch_with(&target, &RequestContext::typed_navigation()).unwrap();
    assert_eq!(tunnels.load(Ordering::SeqCst), 0, "not private: direct");
    network.set_private_mode(true);
    network.fetch_with(&target, &RequestContext::typed_navigation()).unwrap();
    assert_eq!(tunnels.load(Ordering::SeqCst), 1, "private: through the override");
}

#[test]
fn the_system_vpn_route_connects_only_while_the_vpn_is_up() {
    let (port, seen) = serve();
    let up = Arc::new(AtomicBool::new(false));
    let probe_state = Arc::clone(&up);
    let config = NetworkConfig {
        system_vpn: Some(Arc::new(move || probe_state.load(Ordering::SeqCst))),
        rules: RouteRules { profile_default: Some(network::SYSTEM_VPN.into()), ..RouteRules::default() },
        ..NetworkConfig::default()
    };
    let (network, _) = NetworkContext::with_config(&config).unwrap();
    let target = format!("http://127.0.0.1:{port}/");
    let down = network.fetch_with(&target, &RequestContext::typed_navigation());
    assert!(matches!(&down, Err(NetworkError::Transport(why)) if why.contains("VPN is down")), "{down:?}");
    assert!(seen.lock().unwrap().is_empty());
    up.store(true, Ordering::SeqCst);
    assert_eq!(network.fetch_with(&target, &RequestContext::typed_navigation()).unwrap().status, 200);
}

/// Decodes base64url without padding (the `?dns=` parameter).
fn unbase64url(s: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' => 62,
        _ => 63,
    };
    let mut out = Vec::new();
    for chunk in s.as_bytes().chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &c)| n | (u32::from(value(c)) << (18 - 6 * i)));
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// A DoH resolver over plain HTTP on loopback: `app.test` is 127.0.0.1,
/// every other name does not exist. Counts queries.
fn fake_doh() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let queries = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&queries);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                    break;
                }
            }
            count.fetch_add(1, Ordering::SeqCst);
            let encoded = line.split("dns=").nth(1).and_then(|r| r.split([' ', '&']).next()).unwrap_or("");
            let query = unbase64url(encoded);
            let mut answer = query.clone();
            answer[2] = 0x81;
            answer[3] = 0x80;
            let qtype = u16::from_be_bytes([query[query.len() - 4], query[query.len() - 3]]);
            let name_is_app_test = query[12..].starts_with(b"\x03app\x04test\x00");
            if name_is_app_test && qtype == 1 {
                answer[7] = 1;
                answer.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1]);
            } else if !name_is_app_test {
                answer[3] = 0x83; // NXDOMAIN
            }
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", answer.len());
            let _ = stream.write_all(&answer);
        }
    });
    (port, queries)
}

#[test]
fn dns_over_https_resolves_strictly_and_caches() {
    let (port, seen) = serve();
    let (doh_port, queries) = fake_doh();
    let config = NetworkConfig {
        doh: Some(DohConfig { resolver_url: format!("http://127.0.0.1:{doh_port}/dns-query"), bootstrap: Vec::new() }),
        ..NetworkConfig::default()
    };
    let (network, _) = NetworkContext::with_config(&config).unwrap();
    // `.test` is never in system DNS: only DoH can resolve it.
    let response = network.fetch_with(&format!("http://app.test:{port}/"), &RequestContext::typed_navigation()).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(seen.lock().unwrap().len(), 1);
    let after_first = queries.load(Ordering::SeqCst);
    assert_eq!(after_first, 2, "one A and one AAAA query");
    network.fetch_with(&format!("http://app.test:{port}/again"), &RequestContext::typed_navigation()).unwrap();
    assert_eq!(queries.load(Ordering::SeqCst), after_first, "answered from the cache");

    // A name DoH cannot resolve fails; there is no fallback to system DNS.
    let before = diagnostics::metrics::snapshot();
    let missing = network.fetch_with(&format!("http://missing.test:{port}/"), &RequestContext::typed_navigation());
    assert!(matches!(&missing, Err(NetworkError::Transport(why)) if why.contains("DoH")), "{missing:?}");
    assert!(diagnostics::metrics::snapshot().delta(&before, "guard.doh_failed") >= 1);
    // `localhost` and IP literals never go to the resolver.
    let count = queries.load(Ordering::SeqCst);
    network.fetch_with(&format!("http://127.0.0.1:{port}/"), &RequestContext::typed_navigation()).unwrap();
    network.fetch_with(&format!("http://localhost:{port}/"), &RequestContext::typed_navigation()).unwrap();
    assert_eq!(queries.load(Ordering::SeqCst), count);
}

#[test]
fn a_doh_resolver_that_is_not_https_is_refused() {
    let config = NetworkConfig { doh: Some(DohConfig { resolver_url: "http://dns.example/dns-query".into(), bootstrap: Vec::new() }), ..NetworkConfig::default() };
    assert!(NetworkContext::with_config(&config).is_err());
}
