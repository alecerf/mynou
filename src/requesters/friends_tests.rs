//! Plex friends: parsing of the community API's answers, against the real
//! refusals and the synthetic successes of `tests/fixtures/plex`.
use super::friends::{
    MAX_FRIENDS, PlexAccount, PlexRegistry, item_id, parse_friends, parse_watchlist, reason,
    refusal,
};
use super::{Settings, plex_id, plex_user};
use crate::json::{self, Value};
use std::path::Path;

/// The body of a fixture envelope, as the bytes Plex sent.
fn body(fixture: &str) -> Vec<u8> {
    let envelope = json::parse(fixture).unwrap();
    match envelope.get("body").unwrap() {
        Value::String(text) => text.clone().into_bytes(),
        value => json::stringify(value).into_bytes(),
    }
}
fn data(fixture: &str) -> Value {
    let envelope = json::parse(fixture).unwrap();
    envelope.get("body").unwrap().get("data").unwrap().clone()
}
fn parse(text: &str) -> Value {
    json::parse(text).unwrap()
}

#[test]
fn the_friend_list_reads_what_mynou_needs_and_ignores_the_rest() {
    let friends = parse_friends(&data(include_str!(
        "../../tests/fixtures/plex/friends.json"
    )))
    .unwrap();
    let summary: Vec<_> = friends
        .iter()
        .map(|f| {
            (
                f.user_id,
                f.uuid.as_str(),
                f.username.as_str(),
                f.display_name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (7001, "0000000000000a01", "ana.synthetic", "Ana"),
            (7002, "0000000000000b02", "ben.synthetic", ""),
            (7003, "0000000000000c03", "cleo.synthetic", "Cleo"),
            (7004, "0000000000000d04", "dan.synthetic", "Dan"),
        ]
    );
    // An account number sent as text is the same number.
    let listed = parse_friends(&parse(
        r#"{"allFriendsV2":[{"user":{"id":"abc","idRaw":"12","username":"x\ny"}}]}"#,
    ))
    .unwrap();
    assert_eq!(
        (listed[0].user_id, listed[0].username.as_str()),
        (12, "x y")
    );
}

#[test]
fn a_friend_list_missing_what_mynou_needs_is_refused() {
    for (text, why) in [
        (r#"{}"#, "Plex: invalid friends response"),
        (r#"{"allFriendsV2":null}"#, "Plex: invalid friends response"),
        (
            r#"{"allFriendsV2":[{"user":null}]}"#,
            "Plex: invalid friend",
        ),
        (
            r#"{"allFriendsV2":[{"user":{"idRaw":1}}]}"#,
            "Plex: invalid friend ID",
        ),
        (
            r#"{"allFriendsV2":[{"user":{"id":"a/b","idRaw":1}}]}"#,
            "Plex: invalid friend ID",
        ),
        (
            r#"{"allFriendsV2":[{"user":{"id":"ab"}}]}"#,
            "Plex: invalid friend account number",
        ),
        (
            r#"{"allFriendsV2":[{"user":{"id":"ab","idRaw":0}}]}"#,
            "Plex: invalid friend account number",
        ),
        (
            r#"{"allFriendsV2":[{"user":{"id":"ab","idRaw":-4}}]}"#,
            "Plex: invalid friend account number",
        ),
        (
            r#"{"allFriendsV2":[{"user":{"id":"ab","idRaw":1}},{"user":{"id":"cd","idRaw":1}}]}"#,
            "Plex: a friend is listed twice",
        ),
        (
            r#"{"allFriendsV2":[{"user":{"id":"ab","idRaw":1,"username":5}}]}"#,
            "Plex: invalid friend username",
        ),
    ] {
        assert_eq!(parse_friends(&parse(text)).unwrap_err(), why, "{text}");
    }
    let many: Vec<_> = (1..=MAX_FRIENDS + 1)
        .map(|n| format!(r#"{{"user":{{"id":"u{n}","idRaw":{n}}}}}"#))
        .collect();
    let text = format!(r#"{{"allFriendsV2":[{}]}}"#, many.join(","));
    assert_eq!(
        parse_friends(&parse(&text)).unwrap_err(),
        "Plex: more than 200 friends"
    );
}

#[test]
fn a_watchlist_page_reads_its_entries_and_cursor() {
    let listing = parse_watchlist(&data(include_str!(
        "../../tests/fixtures/plex/watchlist-page.json"
    )))
    .unwrap();
    assert_eq!(listing.nodes.len(), 3);
    assert!(!listing.has_next);
    assert_eq!(listing.end.as_deref(), Some("c3ludGhldGljLWN1cnNvci0z"));
    let ids: Vec<_> = listing.nodes.iter().map(item_id).collect();
    assert_eq!(
        ids,
        [
            Some("000000000000000000000101"),
            Some("000000000000000000000102"),
            Some("000000000000000000000103"),
        ]
    );
    // Without a metadata key, the entry's own ID; never a path or nothing.
    assert_eq!(item_id(&parse(r#"{"id":"abc123"}"#)), Some("abc123"));
    assert_eq!(item_id(&parse(r#"{"id":"../x"}"#)), None);
    assert_eq!(item_id(&parse(r#"{"key":"/library/metadata/"}"#)), None);
}

#[test]
fn a_hidden_or_broken_watchlist_is_an_error_never_an_empty_list() {
    let page = |nodes: &str, info: &str| {
        parse(&format!(
            r#"{{"user":{{"watchlist":{{"nodes":[{nodes}],"pageInfo":{info}}}}}}}"#
        ))
    };
    let node = r#"{"id":"a","type":"MOVIE","title":"T"}"#;
    for (value, why) in [
        (
            parse(r#"{"user":null}"#),
            "Plex: this account is not available to you",
        ),
        (
            parse(r#"{"user":{"watchlist":null}}"#),
            "Plex: this watchlist is not shared with you",
        ),
        (parse(r#"{}"#), "Plex: invalid watchlist response"),
        (
            parse(r#"{"user":{"watchlist":{"nodes":[]}}}"#),
            "Plex: invalid watchlist response",
        ),
        (
            page(node, r#"{"hasNextPage":true}"#),
            "Plex: a watchlist page without a next cursor",
        ),
        (
            page("", r#"{"hasNextPage":true,"endCursor":"x"}"#),
            "Plex: a watchlist page without a next cursor",
        ),
        (
            page(node, r#"{"hasNextPage":false,"endCursor":"a\nb"}"#),
            "Plex: invalid watchlist response",
        ),
        (
            page(
                &vec![node; 101].join(","),
                r#"{"hasNextPage":false,"endCursor":null}"#,
            ),
            "Plex: a watchlist page holds more than 100 items",
        ),
    ] {
        assert_eq!(parse_watchlist(&value).unwrap_err(), why);
    }
    let last = parse_watchlist(&page("", r#"{"hasNextPage":false}"#)).unwrap();
    assert!(last.nodes.is_empty() && !last.has_next && last.end.is_none());
}

#[test]
fn plex_refusals_give_plex_reason_in_one_line() {
    for (fixture, why) in [
        (
            include_str!("../../tests/fixtures/plex/community-without-token.json"),
            "You must provide a token!",
        ),
        (
            include_str!("../../tests/fixtures/plex/community-invalid-token.json"),
            "Invalid token",
        ),
        (
            include_str!("../../tests/fixtures/plex/metadata-without-token.json"),
            "You must provide a token!",
        ),
        (
            include_str!("../../tests/fixtures/plex/identity-without-token.json"),
            "User could not be authenticated",
        ),
    ] {
        assert_eq!(refusal(&body(fixture), "secret").as_deref(), Some(why));
    }
    assert_eq!(refusal(b"<html><body>Bad gateway</body></html>", ""), None);
    assert_eq!(refusal(b"line one\nline two", ""), None);
    assert_eq!(refusal(&[0xff, 0xfe], ""), None);
    assert_eq!(refusal(b"{}", ""), None);
}

#[test]
fn plex_reasons_never_carry_the_owner_token() {
    let token = "owner-token-123";
    assert_eq!(
        reason(
            "Invalid token owner-token-123\r\n\tfor owner-token-123",
            token
        ),
        "Invalid token [token] for [token]"
    );
    assert_eq!(
        refusal(
            br#"{"errors":[{"message":"token owner-token-123 refused"}]}"#,
            token
        )
        .as_deref(),
        Some("token [token] refused")
    );
    assert_eq!(reason(&"x".repeat(500), token).len(), 200);
    assert_eq!(reason("plain", ""), "plain");
}

#[test]
fn plex_requester_ids_are_canonical() {
    assert_eq!(plex_id(7001), "plex-7001");
    assert_eq!(plex_user("plex-7001"), Some(7001));
    for id in [
        "plex-0",
        "plex-07001",
        "plex-",
        "plex-x",
        "plex7001",
        "7001",
    ] {
        assert_eq!(plex_user(id), None, "{id}");
    }
    assert!(super::valid_id(&plex_id(u64::MAX)));
}

#[test]
fn token_accounts_cannot_take_a_plex_friend_id() {
    let settings = parse(
        r#"{"accounts":[{"id":"plex-7001","expected_user_id":"7001","token_env":"MYNOU_SYNTHETIC"}]}"#,
    );
    assert_eq!(
        Settings::from_json(Some(&settings), Path::new("/")).unwrap_err(),
        "Requester configuration: account IDs starting with plex- are kept for Plex friends"
    );
}

#[test]
fn the_plex_registry_round_trips_and_rejects_what_it_never_writes() {
    let friend = PlexAccount {
        user_id: 7001,
        uuid: "0000000000000a01".into(),
        username: "ana.synthetic".into(),
        display_name: "Ana".into(),
        owner: false,
        present: true,
        error: Some("Plex: this watchlist is not shared with you".into()),
    };
    let owner = PlexAccount {
        user_id: 9001,
        uuid: String::new(),
        username: "owner".into(),
        display_name: String::new(),
        owner: true,
        present: true,
        error: None,
    };
    let registry = PlexRegistry {
        accounts: [
            (plex_id(7001), friend.clone()),
            (plex_id(9001), owner.clone()),
        ]
        .into_iter()
        .collect(),
        checked_at: 1_760_000_000,
        error: None,
    };
    assert_eq!(
        PlexRegistry::from_json(&registry.to_json()).unwrap(),
        registry
    );
    assert_eq!(friend.name(), "Ana");
    assert_eq!(owner.name(), "owner");

    let refused = |mutate: &dyn Fn(&mut Value)| {
        let mut value = registry.to_json();
        mutate(&mut value);
        PlexRegistry::from_json(&value).is_err()
    };
    let account = |value: &mut Value, index: usize| match value.get_mut("accounts") {
        Some(Value::Array(list)) => list[index].clone(),
        _ => unreachable!(),
    };
    let replace = |value: &mut Value, index: usize, item: Value| {
        if let Some(Value::Array(list)) = value.get_mut("accounts") {
            list[index] = item;
        }
    };
    assert!(refused(&|v| v.insert("extra", true)));
    assert!(refused(&|v| {
        let mut item = account(v, 0);
        item.insert("owner", true);
        replace(v, 0, item);
    }));
    assert!(refused(&|v| {
        let mut item = account(v, 0);
        item.insert("uuid", "");
        replace(v, 0, item);
    }));
    assert!(refused(&|v| {
        let mut item = account(v, 0);
        item.insert("display_name", "two\nlines");
        replace(v, 0, item);
    }));
    assert!(refused(&|v| {
        let item = account(v, 0);
        if let Some(Value::Array(list)) = v.get_mut("accounts") {
            list.push(item);
        }
    }));
    assert!(refused(&|v| v.insert("error", "")));
}

#[test]
fn the_real_metadata_answer_gives_the_tmdb_identity() {
    let envelope = parse(include_str!("../../tests/fixtures/plex/metadata-item.json"));
    let item = envelope
        .get("body")
        .and_then(|b| b.get("MediaContainer"))
        .and_then(|c| c.get("Metadata"))
        .and_then(Value::as_array)
        .unwrap()[0]
        .clone();
    let request = crate::integrations::watchlist_request(&item)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            request.kind.as_str(),
            request.title.as_str(),
            request.year,
            request.tmdb_id
        ),
        ("movie", "Avatar: The Way of Water", 2022, Some(76600))
    );
}
