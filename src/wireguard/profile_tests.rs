use super::*;

// Synthetic material only. Each key's bytes exercise a different base64 quartet.
const SECRET: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
const PUBLIC: &str = "CQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const PSK: &str = "//////////////////////////////////////////8=";

fn fixture() -> String {
    format!(
        "[Interface]\nPrivateKey = {SECRET}\nAddress = 10.23.0.2/24\nDNS = 10.23.0.1, 1.1.1.1\nMTU = 1280\n[Peer]\nPublicKey = {PUBLIC}\nPresharedKey = {PSK}\nAllowedIPs = 0.0.0.0/0\nEndpoint = 192.0.2.10:51820\nPersistentKeepalive = 25\n"
    )
}

fn failure(input: &str) -> ProfileError {
    match parse(input) {
        Ok(_) => panic!("hostile profile was accepted"),
        Err(error) => error,
    }
}

#[test]
fn converts_the_complete_supported_subset_without_network_work() {
    let profile = parse(&fixture()).unwrap();
    assert_eq!(profile.address_prefix(), 24);
    let config = profile.into_tunnel_config();
    assert_eq!(config.secret, [1; 32]);
    let mut public = [0; 32];
    public[0] = 9;
    assert_eq!(config.peer_public, public);
    assert_eq!(config.psk, Some([255; 32]));
    assert_eq!(config.ip.local(), Ipv4Addr::new(10, 23, 0, 2));
    assert_eq!(config.mtu, 1280);
    assert_eq!(config.tcp.ip, config.ip);
    assert_eq!(config.tcp.mtu, config.mtu);
    let dns = config.dns.unwrap();
    assert_eq!(dns.ip, config.ip);
    assert_eq!(dns.mtu, config.mtu);
    assert_eq!(
        dns.resolvers,
        [Ipv4Addr::new(10, 23, 0, 1), Ipv4Addr::new(1, 1, 1, 1)]
    );
    assert_eq!(
        config.endpoint,
        Endpoint::Address("192.0.2.10:51820".parse().unwrap())
    );
    assert_eq!(config.keepalive, Some(Duration::from_secs(25)));
    assert!(config.initiate);
}

#[test]
fn comments_whitespace_crlf_and_missing_final_newline_are_supported() {
    let input = fixture()
        .replace(" = ", "\t=\t")
        .replace('\n', " # comment\r\n");
    assert!(parse(&format!("# header\r\n\r\n{input}")).is_ok());
    assert!(parse(fixture().trim_end()).is_ok());
}

#[test]
fn defaults_and_keepalive_bounds_match_wireguard_semantics() {
    let minimal = fixture()
        .replace("MTU = 1280\n", "")
        .replace(&format!("PresharedKey = {PSK}\n"), "")
        .replace("PersistentKeepalive = 25\n", "");
    let config = parse(&minimal).unwrap().into_tunnel_config();
    assert_eq!(config.mtu, MAX_MTU);
    assert_eq!(config.tcp.mtu, MAX_MTU);
    assert_eq!(config.dns.unwrap().mtu, MAX_MTU);
    assert_eq!(config.keepalive, None);
    assert_eq!(config.psk, None);
    for seconds in [0, 1, 65535] {
        let config = parse(&fixture().replace("Keepalive = 25", &format!("Keepalive = {seconds}")))
            .unwrap()
            .into_tunnel_config();
        assert_eq!(
            config.keepalive,
            (seconds != 0).then(|| Duration::from_secs(seconds))
        );
    }
    for mtu in [MIN_MTU, MAX_MTU] {
        let config = parse(&fixture().replace("MTU = 1280", &format!("MTU = {mtu}")))
            .unwrap()
            .into_tunnel_config();
        assert_eq!(config.mtu, mtu);
    }
    for prefix in [0, 32] {
        assert_eq!(
            parse(&fixture().replace("/24", &format!("/{prefix}")))
                .unwrap()
                .address_prefix(),
            prefix
        );
    }
}

#[test]
fn every_required_field_is_mandatory_and_every_field_is_unique() {
    for (index, name) in FIELDS.iter().enumerate() {
        let original = fixture();
        let row = original
            .lines()
            .find(|line| line.starts_with(name))
            .unwrap();
        if ![3, 5, 8].contains(&index) {
            let input = original.replace(&format!("{row}\n"), "");
            assert_eq!(failure(&input).kind, ErrorKind::MissingField(name));
        }
        let input = original.replace(row, &format!("{row}\n{row}"));
        let error = failure(&input);
        assert_eq!(error.kind, ErrorKind::DuplicateField(name));
        assert!(error.line > 1);
    }
}

#[test]
fn rejects_hooks_unknown_fields_multiple_peers_and_misplaced_fields() {
    for directive in [
        "PreUp",
        "PostUp",
        "PreDown",
        "PostDown",
        "ListenPort",
        "SaveConfig",
        "Table",
        "FwMark",
        "SecretProvider",
    ] {
        let input = fixture().replace("[Peer]", &format!("{directive} = do-not-run\n[Peer]"));
        assert_eq!(failure(&input).kind, ErrorKind::UnsupportedDirective);
    }
    for section in ["[Interface]", "[Peer]"] {
        assert_eq!(
            failure(&format!("{}{section}\n", fixture())).kind,
            ErrorKind::DuplicateSection
        );
    }
    for input in [
        fixture().replace("[Interface]", "[Unknown]"),
        fixture().replace("[Interface]", "[Peer]"),
    ] {
        assert_eq!(failure(&input).kind, ErrorKind::UnsupportedSection);
    }
    for input in [
        fixture().replace("[Interface]\n", ""),
        fixture().replace("[Peer]\n", ""),
        fixture().replace("[Peer]", "[Peer]\nMTU = 1280"),
        fixture().replace("[Peer]", &format!("PublicKey = {PUBLIC}\n[Peer]")),
    ] {
        assert_eq!(failure(&input).kind, ErrorKind::UnsupportedDirective);
    }
}

#[test]
fn rejects_unsupported_addresses_routes_resolvers_and_endpoints() {
    let cases = [
        (
            "Address",
            "10.23.0.2/24",
            vec![
                "10.0.0.1",
                "10.0.0.1/33",
                "10.0.0.1/+1",
                "10.0.0.1/24,10.0.0.2/24",
                "::1/128",
                "fd00::2/128",
                "10.0.0.1/24,fd00::2/129",
                "10.0.0.1/24,fd00::2/128,fd00::3/128",
                "10.0.0.1/24,ff02::1/128",
                "0.0.0.1/8",
                "127.0.0.1/8",
                "224.0.0.1/8",
                "255.255.255.255/32",
            ],
        ),
        (
            "DNS",
            "10.23.0.1, 1.1.1.1",
            vec![
                "",
                "resolver.example",
                "::1",
                "fd00::1",
                "1.1.1.1,fd00::1,fd00::1",
                "127.0.0.1",
                "0.0.0.0",
                "224.0.0.1",
                "1.1.1.1,1.1.1.1",
                "1.1.1.1,",
                ",1.1.1.1",
                "1.1.1.1,2.2.2.2,3.3.3.3,4.4.4.4",
            ],
        ),
        (
            "AllowedIPs",
            "0.0.0.0/0",
            vec![
                "0.0.0.0/1",
                "10.0.0.0/8",
                "0.0.0.0/0,::/1",
                "0.0.0.0/0,::/0,::/0",
                "::/0",
                "0.0.0.1/0",
                "0.0.0.0/0,0.0.0.0/0",
            ],
        ),
        (
            "Endpoint",
            "192.0.2.10:51820",
            vec![
                "vpn.example:51820",
                "localhost:51820",
                "[::1]:51820",
                "192.0.2.10:0",
                "192.0.2.10:65536",
                "192.0.2.10",
                "127.0.0.1:123",
                "0.0.0.0:123",
                "224.0.0.1:123",
                "255.255.255.255:123",
                "https://192.0.2.10:123",
                "user@192.0.2.10:123",
            ],
        ),
    ];
    for (field, original, values) in cases {
        for value in values {
            assert_eq!(
                failure(&fixture().replace(original, value)).kind,
                ErrorKind::InvalidValue(field)
            );
        }
    }
    for dns in ["1.1.1.1", "1.1.1.1,2.2.2.2,3.3.3.3"] {
        assert!(parse(&fixture().replace("10.23.0.1, 1.1.1.1", dns)).is_ok());
    }
    assert!(parse(&fixture().replace(":51820", ":65535")).is_ok());
}

#[test]
fn rejects_malformed_numbers_and_out_of_range_mtu_or_keepalive() {
    for (field, old, values) in [
        (
            "MTU",
            "1280",
            vec![
                "",
                "0",
                "575",
                "1425",
                "+1280",
                "1_280",
                "0x500",
                "1280junk",
                "4294967296",
            ],
        ),
        (
            "PersistentKeepalive",
            "25",
            vec!["", "-1", "65536", "+25", "2.5", "25 seconds", "4294967296"],
        ),
    ] {
        for value in values {
            assert_eq!(
                failure(
                    &fixture().replace(&format!("{field} = {old}"), &format!("{field} = {value}"))
                )
                .kind,
                ErrorKind::InvalidValue(field)
            );
        }
    }
}

#[test]
fn keys_are_exact_canonical_base64_with_no_low_order_peer() {
    let zero = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    for (field, original) in [
        ("PrivateKey", SECRET),
        ("PublicKey", PUBLIC),
        ("PresharedKey", PSK),
    ] {
        for malformed in [
            "".into(),
            "key-file".into(),
            original[..43].into(),
            format!("{original}="),
            original.replace('=', "A"),
            original.replace('=', "=="),
            format!(" {} x", &original[..40]),
            original.replacen(original.chars().next().unwrap(), "-", 1),
        ] {
            assert_eq!(
                failure(&fixture().replace(original, &malformed)).kind,
                ErrorKind::InvalidValue(field)
            );
        }
        // Same decoded bytes with nonzero unused padding bits must be rejected.
        let mut noncanonical = original.to_owned();
        noncanonical.replace_range(
            42..43,
            if field == "PresharedKey" {
                "/"
            } else if field == "PrivateKey" {
                "F"
            } else {
                "B"
            },
        );
        assert_eq!(
            failure(&fixture().replace(original, &noncanonical)).kind,
            ErrorKind::InvalidValue(field)
        );
    }
    for peer in [zero, "AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="] {
        assert_eq!(
            failure(&fixture().replace(PUBLIC, peer)).kind,
            ErrorKind::InvalidValue("PublicKey")
        );
    }
    assert_eq!(
        failure(&fixture().replace(SECRET, zero)).kind,
        ErrorKind::InvalidValue("PrivateKey")
    );
    assert_eq!(
        parse(&fixture().replace(PSK, zero))
            .unwrap()
            .into_tunnel_config()
            .psk,
        Some([0; 32])
    );
}

#[test]
fn exact_resource_limits_apply_to_comments_and_whitespace_too() {
    let original = fixture();
    let exact_line = format!("#{}\n{original}", "x".repeat(MAX_LINE_BYTES - 1));
    assert!(parse(&exact_line).is_ok());
    assert_eq!(
        failure(&format!("#{exact_line}")).kind,
        ErrorKind::LineLimit
    );
    let lines = original.lines().count();
    let exact_lines = format!("{original}{}", "\n".repeat(MAX_PROFILE_LINES - lines));
    assert!(parse(&exact_lines).is_ok());
    assert_eq!(
        failure(&format!("{exact_lines}\n")).kind,
        ErrorKind::ProfileLimit
    );
    let mut exact_bytes = original;
    while exact_bytes.len() < MAX_PROFILE_BYTES {
        let length = (MAX_PROFILE_BYTES - exact_bytes.len()).min(MAX_LINE_BYTES + 1);
        exact_bytes.push_str(&" ".repeat(length - 1));
        exact_bytes.push('\n');
    }
    assert!(parse(&exact_bytes).is_ok());
    assert_eq!(
        failure(&format!("{exact_bytes} ")).kind,
        ErrorKind::ProfileLimit
    );
}

#[test]
fn syntax_and_diagnostics_never_echo_hostile_or_secret_input() {
    for value in ["\0", "\u{1b}", "\u{7f}", "\r", "é", "\u{feff}"] {
        assert_eq!(
            failure(&format!("# x{value}y\n{}", fixture())).kind,
            ErrorKind::Syntax
        );
    }
    assert_eq!(
        failure(&fixture().replace("DNS =", "DNS :")).kind,
        ErrorKind::Syntax
    );
    for input in [
        fixture().replace("[Interface]", &format!("[{SECRET}]")),
        fixture().replace("[Peer]", &format!("{SECRET} = {PSK}\n[Peer]")),
        fixture().replace(SECRET, &format!("{SECRET}bad")),
        fixture().replace("192.0.2.10:51820", "private-provider.example:51820"),
    ] {
        let error = failure(&input);
        for output in [error.to_string(), format!("{error:?}")] {
            for secret in [SECRET, PSK, "private-provider.example", "192.0.2.10"] {
                assert!(!output.contains(secret));
            }
        }
    }
}

#[test]
fn fixed_base64_vectors_preserve_every_byte() {
    assert_eq!(
        key("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="),
        Some([
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31
        ])
    );
    assert_eq!(
        key("//79/Pv6+fj39vX08/Lx8O/u7ezr6uno5+bl5OPi4eA="),
        Some([
            255, 254, 253, 252, 251, 250, 249, 248, 247, 246, 245, 244, 243, 242, 241, 240, 239,
            238, 237, 236, 235, 234, 233, 232, 231, 230, 229, 228, 227, 226, 225, 224
        ])
    );
}

#[test]
fn local_identity_cannot_be_used_as_the_peer_identity() {
    // RFC 7748 Alice identity, also covered by crypto::curve tests.
    let input = fixture()
        .replace(SECRET, "dwdtCnMYpX08FsFyUbJmRd9ML4frwJkqsXf7pR25LCo=")
        .replace(PUBLIC, "hSDwCYkwp1R0i33ctD73Wg2/Og0mOBr066SpjqqbTmo=");
    let error = failure(&input);
    assert_eq!(
        error,
        ProfileError {
            line: 7,
            kind: ErrorKind::InvalidValue("PublicKey")
        }
    );
}

#[test]
fn bare_final_carriage_return_is_rejected() {
    assert_eq!(
        failure(&format!("{}\r", fixture().trim_end())).kind,
        ErrorKind::Syntax
    );
}

#[test]
fn ipv6_entries_put_ipv6_in_the_tunnel_and_are_kept() {
    let input = fixture()
        .replace(
            "Address = 10.23.0.2/24",
            "Address = 10.23.0.2/24, fd00:23::2/128",
        )
        .replace("DNS = 10.23.0.1, 1.1.1.1", "DNS = 10.23.0.1, fd00:23::1")
        .replace("AllowedIPs = 0.0.0.0/0", "AllowedIPs = ::/0, 0.0.0.0/0");
    let profile = parse(&input).unwrap();
    assert_eq!(profile.dns(), [Ipv4Addr::new(10, 23, 0, 1)]);
    assert!(profile.ipv6());
    let document = profile.private_document();
    assert!(document.contains("Address = 10.23.0.2/24, fd00:23::2/128\n"));
    assert!(document.contains("DNS = 10.23.0.1, fd00:23::1\n"));
    assert!(document.contains("AllowedIPs = 0.0.0.0/0, ::/0\n"));
    assert_eq!(parse(&document).unwrap().private_document(), document);
    let config = profile.into_tunnel_config();
    let local: std::net::Ipv6Addr = "fd00:23::2".parse().unwrap();
    assert_eq!(config.ip.local_ipv6(), Some(local));
    assert_eq!(config.tcp.ip, config.ip);
    let resolvers = config.dns.unwrap().resolvers;
    assert_eq!(
        resolvers,
        [
            "10.23.0.1".parse::<IpAddr>().unwrap(),
            "fd00:23::1".parse().unwrap()
        ]
    );
    // Without the IPv6 route, below the IPv6 MTU or with a link-local
    // address, the profile is kept but the tunnel stays IPv4 only.
    for edited in [
        input.replace("AllowedIPs = ::/0, 0.0.0.0/0", "AllowedIPs = 0.0.0.0/0"),
        input.replace("MTU = 1280", "MTU = 1279"),
        input.replace("fd00:23::2/128", "fe80::2/64"),
    ] {
        let profile = parse(&edited).unwrap();
        assert!(!profile.ipv6());
        let config = profile.into_tunnel_config();
        assert_eq!(config.ip.local_ipv6(), None);
        assert_eq!(
            config.dns.unwrap().resolvers,
            ["10.23.0.1".parse::<IpAddr>().unwrap()]
        );
    }
}
