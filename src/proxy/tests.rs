use super::*;

fn list(proxies: &[&str]) -> ProxyList {
    ProxyList::new(proxies).unwrap()
}

#[test]
fn round_robin_cycles_through_all_proxies() {
    let list = list(&["http://a", "http://b", "http://c"]);
    assert_eq!(list.pick(), Some("http://a/"));
    assert_eq!(list.pick(), Some("http://b/"));
    assert_eq!(list.pick(), Some("http://c/"));
    assert_eq!(list.pick(), Some("http://a/"));
}

#[test]
fn pick_index_wraps_around_skipping_a_cooled_down_proxy() {
    let list = list(&["http://a", "http://b", "http://c"]);
    list.mark_bad("http://b", Duration::from_secs(60));
    // More picks than proxies, over two full laps and one more: each
    // lap must skip "b" every time it comes up, not just the first.
    let picks: Vec<&str> = (0..7).map(|_| list.pick().unwrap()).collect();
    assert_eq!(
        picks,
        [
            "http://a/",
            "http://c/",
            "http://a/",
            "http://c/",
            "http://a/",
            "http://c/",
            "http://a/",
        ]
    );
}

#[test]
fn pick_index_cursor_wraps_at_len_from_every_start() {
    for start in 0..3 {
        let list = list(&["http://a", "http://b", "http://c"]);
        list.lock().next_index = start;
        assert!(list.pick_index().is_some());
        assert_eq!(list.lock().next_index, (start + 1) % 3);
    }
}

#[test]
fn pick_index_cursor_wraps_when_every_proxy_is_cooling_down() {
    // "c" is due back first. Picking it must wrap the cursor to 0.
    let list = list(&["http://a", "http://b", "http://c"]);
    let now = Instant::now();
    {
        let mut state = list.lock();
        state.bad_until[0] = Some(now + Duration::from_secs(60));
        state.bad_until[1] = Some(now + Duration::from_secs(60));
        state.bad_until[2] = Some(now + Duration::from_secs(10));
    }
    assert_eq!(list.pick_index(), Some(2));
    assert_eq!(list.lock().next_index, 0);
}

#[test]
fn pick_index_avoiding_cursor_wraps_at_len() {
    let list = list(&["http://a", "http://b", "http://c"]);
    list.lock().next_index = 2;
    // Cursor on "c", "b" avoided: "c" it is, and the cursor wraps to 0.
    assert_eq!(list.pick_index_avoiding(&[1]), Some(2));
    assert_eq!(list.lock().next_index, 0);
}

#[test]
fn pick_index_ties_prefer_rotation_order() {
    let list = list(&["http://a", "http://b"]);
    // Exact tie: rotation order decides.
    let until = Instant::now().checked_add(Duration::from_secs(60)).unwrap();
    {
        let mut state = list.lock();
        state.bad_until[0] = Some(until);
        state.bad_until[1] = Some(until);
    }
    assert_eq!(list.pick(), Some("http://a/"));
}

#[test]
fn empty_list_has_no_pick() {
    let list = ProxyList::new(Vec::<String>::new()).unwrap();
    assert!(list.is_empty());
    assert_eq!(list.len(), 0);
    assert_eq!(list.pick(), None);
}

#[test]
fn is_empty_is_false_for_a_nonempty_list() {
    assert!(!list(&["http://a"]).is_empty());
}

#[test]
fn rejects_blank_proxy_entries() {
    let err = ProxyList::new(["  "]).unwrap_err();
    assert!(matches!(err, Error::InvalidProxy { .. }));
}

#[test]
fn vendor_colon_format_is_explained_and_redacted() {
    let err = ProxyList::new(["1.2.3.4:8080:user:s3cret"]).unwrap_err();
    let message = err.to_string();
    let reason = std::error::Error::source(&err).unwrap().to_string();
    assert!(!message.contains("s3cret"), "{message}");
    assert!(!message.contains("user"), "{message}");
    assert!(!reason.contains("s3cret"), "{reason}");
    assert!(reason.contains("http://user:pass@host:port"), "{reason}");

    let at_spelling = ProxyList::new(["1.2.3.4:8080@user:s3cret"])
        .unwrap_err()
        .to_string();
    assert!(!at_spelling.contains("s3cret"), "{at_spelling}");
}

#[test]
fn vendor_hint_needs_every_guard_to_agree() {
    let no_hint = |raw: &str| {
        let err = ProxyList::new([raw]).unwrap_err();
        let reason = std::error::Error::source(&err).unwrap().to_string();
        assert!(
            !reason.contains("write `host:port:user:pass`"),
            "{raw:?}: {reason}"
        );
    };
    no_hint("a:1234://p:q"); // has a scheme
    no_hint("1234:5678:9012:3456@"); // has a `@`
    no_hint("host:1234:extra"); // not exactly three colons
    no_hint("2001:db8::1"); // unbracketed IPv6: second field is not a port
    no_hint("host::extra:more"); // second field is empty, not a port
    no_hint("::1"); // bare IPv6 loopback: two colons, no port

    // A bracketed IPv6 literal is a valid host:port and never reaches
    // the hint at all.
    assert_eq!(
        ProxyList::new(["[::1]:8080"]).unwrap().as_slice(),
        ["http://[::1]:8080/"]
    );
}

#[test]
fn rejects_unparseable_and_unknown_schemes() {
    assert!(matches!(
        ProxyList::new(["not a valid proxy url"]).unwrap_err(),
        Error::InvalidProxy { .. }
    ));
    assert!(matches!(
        ProxyList::new(["ftp://proxy.example:21"]).unwrap_err(),
        Error::InvalidProxy { .. }
    ));
}

#[test]
fn scheme_less_entries_are_treated_as_http() {
    let list = list(&[
        "1.2.3.4:8080",
        "user:pass@proxy.example:3128",
        "localhost:9",
    ]);
    assert_eq!(
        list.as_slice(),
        &[
            "http://1.2.3.4:8080/",
            "http://user:pass@proxy.example:3128/",
            "http://localhost:9/",
        ]
    );
}

#[test]
fn urls_are_canonicalised() {
    let list = list(&[
        "HTTP://Proxy.Example:80",
        "https://user:pass@proxy.example:443",
        "http://proxy.example:8080/",
    ]);
    assert_eq!(
        list.as_slice(),
        &[
            "http://proxy.example/",
            "https://user:pass@proxy.example/",
            "http://proxy.example:8080/",
        ]
    );
}

#[test]
fn iter_yields_canonical_urls_in_rotation_order() {
    let list = list(&["HTTP://Proxy.Example:80", "proxy-b.example:8080"]);
    let urls: Vec<&str> = list.iter().collect();
    assert_eq!(
        urls,
        &["http://proxy.example/", "http://proxy-b.example:8080/"]
    );
    assert_eq!(list.iter().len(), list.len());

    let reversed: Vec<&str> = list.iter().rev().collect();
    assert_eq!(
        reversed,
        &["http://proxy-b.example:8080/", "http://proxy.example/"]
    );
}

#[test]
fn iter_redacted_hides_credentials_in_rotation_order() {
    let list = list(&["http://alice:s3cret@a:8080", "b:3128"]);
    let shown: Vec<String> = list.iter_redacted().collect();
    assert_eq!(shown, ["http://***@a:8080/", "http://b:3128/"]);
}

#[cfg(not(feature = "socks"))]
#[test]
fn socks_is_rejected_without_the_feature() {
    let err = ProxyList::new(["socks5://127.0.0.1:1080"]).unwrap_err();
    let Error::InvalidProxy { .. } = &err else {
        panic!("expected InvalidProxy");
    };
    assert!(err.to_string().contains("socks"), "{err}");
}

#[cfg(feature = "socks")]
#[test]
fn socks_is_accepted_with_the_feature() {
    let list = list(&["socks5://127.0.0.1:1080", "socks5h://127.0.0.1:1081"]);
    assert_eq!(list.len(), 2);
}

#[test]
fn duplicates_are_dropped_keeping_first_position() {
    let list = list(&["http://a", "http://b", "http://a/", "b", "HTTP://A:80"]);
    assert_eq!(list.as_slice(), &["http://a/", "http://b/"]);
}

#[test]
fn dedup_holds_at_scale() {
    // 2000 inputs, every second one a repeat: 1000 left, first-seen order.
    let proxies: Vec<String> = (0..2000)
        .map(|i| format!("http://10.0.0.{}:{}", i / 2 % 256, 9000 + i / 2))
        .collect();
    let list = ProxyList::new(&proxies).unwrap();
    assert_eq!(list.len(), 1000);
    assert_eq!(list.as_slice()[0], "http://10.0.0.0:9000/");
    assert_eq!(list.as_slice()[999], "http://10.0.0.231:9999/");
}

#[test]
fn mark_bad_is_skipped_until_cooldown_expires() {
    let list = list(&["http://a", "http://b"]);
    assert_eq!(list.pick(), Some("http://a/"));
    // The un-canonicalised spelling is accepted too.
    assert!(list.mark_bad("http://b", Duration::from_millis(50)));
    assert!(list.in_cooldown("http://b/"));
    assert!(list.in_cooldown("b"));
    // "b" is next in rotation but is in cooldown, so "a" is served again.
    assert_eq!(list.pick(), Some("http://a/"));
    std::thread::sleep(Duration::from_millis(80));
    assert!(!list.in_cooldown("http://b/"));
    assert_eq!(list.pick(), Some("http://b/"));
}

#[test]
fn mark_bad_on_unknown_proxy_is_a_no_op() {
    let list = list(&["http://a"]);
    assert!(!list.mark_bad("http://nope", Duration::from_secs(60)));
    assert!(!list.mark_bad("not a url at all", Duration::from_secs(60)));
    assert!(!list.in_cooldown("http://nope"));
    assert_eq!(list.pick(), Some("http://a/"));
}

#[test]
fn mark_good_index_clears_a_cooldown() {
    let list = list(&["http://a", "http://b"]);
    list.mark_bad("http://a", Duration::from_secs(60));
    assert!(list.in_cooldown("http://a"));
    list.mark_good_index(0);
    assert!(!list.in_cooldown("http://a"));
    assert_eq!(list.pick(), Some("http://a/"));
}

#[test]
fn mark_good_clears_a_cooldown_early() {
    let list = list(&["http://a", "http://b"]);
    assert_eq!(list.pick(), Some("http://a/"));
    assert!(list.mark_bad("http://b", Duration::from_secs(60)));
    assert!(list.in_cooldown("http://b/"));
    // The un-canonicalised spelling is accepted too.
    assert!(list.mark_good("b"));
    assert!(!list.in_cooldown("http://b/"));
    assert_eq!(list.pick(), Some("http://b/"));
}

#[test]
fn mark_good_on_unknown_proxy_is_a_no_op() {
    let list = list(&["http://a", "http://b"]);
    assert_eq!(list.pick(), Some("http://a/"));
    assert!(list.mark_bad("http://b", Duration::from_secs(60)));
    assert!(!list.mark_good("http://nope"));
    assert!(!list.mark_good("not a url at all"));
    // "b" is still cooling down, so "a" is served again.
    assert!(list.in_cooldown("http://b/"));
    assert_eq!(list.pick(), Some("http://a/"));
}

#[test]
fn mark_good_on_a_healthy_proxy_changes_nothing() {
    let list = list(&["http://a", "http://b", "http://c"]);
    assert!(list.mark_good("http://b"));
    assert_eq!(list.pick(), Some("http://a/"));
    assert_eq!(list.pick(), Some("http://b/"));
    assert_eq!(list.pick(), Some("http://c/"));
}

#[test]
fn all_proxies_bad_returns_the_one_recovering_first() {
    let list = list(&["http://a", "http://b", "http://c"]);
    list.mark_bad("http://a", Duration::from_secs(60));
    list.mark_bad("http://b", Duration::from_secs(10));
    list.mark_bad("http://c", Duration::from_secs(60));
    assert_eq!(list.pick(), Some("http://b/"));
    // With every proxy cooling down, consecutive picks repeat the
    // soonest-recovering one: nothing here has answered to clear it.
    assert_eq!(list.pick(), Some("http://b/"));
    // Rotation then carries on after the proxy it fell back to.
    for proxy in ["http://a", "http://b", "http://c"] {
        list.mark_good(proxy);
    }
    assert_eq!(list.pick(), Some("http://c/"));
}

#[test]
fn any_healthy_except_ignores_the_given_index() {
    let pool = list(&["http://a", "http://b"]);
    assert!(pool.any_healthy_except(0));
    pool.mark_bad("http://b", Duration::from_secs(60));
    assert!(!pool.any_healthy_except(0));
    assert!(pool.any_healthy_except(1));

    // Once a cooldown has actually expired (not just been set to a
    // duration too short to ever matter), the proxy it covers counts as
    // healthy again: "a" recovers while "b" (excluded above) stays bad.
    pool.mark_bad("http://a", Duration::from_millis(50));
    std::thread::sleep(Duration::from_millis(80));
    assert!(pool.any_healthy_except(1));

    let single = list(&["http://a"]);
    assert!(!single.any_healthy_except(0));
}

#[test]
fn pick_avoiding_skips_a_healthy_proxy_it_is_told_to_avoid() {
    let pool = list(&["http://a", "http://b"]);
    assert_eq!(pool.pick_index_avoiding(&[0]), Some(1));
    assert_eq!(pool.pick_index_avoiding(&[1]), Some(0));
    // Nothing else healthy: avoid is advisory, and the plain rotation
    // carries on from the cursor.
    assert_eq!(pool.pick_index_avoiding(&[0, 1]), Some(1));
    assert!(!pool.any_healthy_outside(&[0, 1]));
}

#[test]
fn huge_cooldown_does_not_panic_or_poison() {
    let list = list(&["http://a", "http://b"]);
    list.mark_bad("http://a", Duration::MAX);
    assert!(list.in_cooldown("http://a"));
    assert_eq!(list.pick(), Some("http://b/"));
}

#[test]
fn debug_output_hides_credentials() {
    let list = list(&["http://user:s3cret@a:8080", "http://b"]);
    list.mark_bad("http://b", Duration::from_secs(60));
    let debug = format!("{list:?}");
    assert!(!debug.contains("s3cret"), "{debug}");
    assert!(debug.contains("http://***@a:8080/"), "{debug}");
    assert!(debug.contains("in_cooldown: [\"http://b/\"]"), "{debug}");
}

#[test]
fn redaction_shows_only_scheme_user_host_and_port() {
    assert_eq!(redact_stored("http://a:8080/"), "http://a:8080/");
    assert_eq!(redact_stored("http://u:p@a/"), "http://***@a/");
    assert_eq!(redact_stored("socks5://u@a/"), "socks5://***@a/");
    assert_eq!(redact_stored("http://:p@a/"), "http://***@a/");
    assert_eq!(redact_stored("http://[::1]:3128/"), "http://[::1]:3128/");
    assert_eq!(redact_stored("garbage"), "garbage");
}

#[test]
fn redaction_drops_the_path_query_and_fragment() {
    assert_eq!(redact_stored("http://h/@path"), "http://h/");
    assert_eq!(redact_stored("http://u:p@h/a/b?x=@y#f"), "http://***@h/");
    // Entries 0.2.0 stores although the password sits after the host.
    assert_eq!(
        redact_stored("http://http/1.2.3.4:8080:user:secret"),
        "http://http/"
    );
    assert_eq!(
        redact_stored("http://proxy.example:8080@alice:123/#secret"),
        "http://***@alice:123/"
    );
}

#[test]
fn redaction_does_not_take_userinfo_for_a_scheme() {
    assert_eq!(redact_unparsable("user:p@a://1@1"), "***@1");
    assert_eq!(redact_unparsable("ftp://user:p@a://1@1"), "ftp://***@1");
    assert_eq!(redact_unparsable("user:p://x"), "user:***");
    assert_eq!(redact_unparsable("1http://u:p@h"), "***@h");
}

#[test]
fn redaction_reads_the_text_the_way_the_url_parser_does() {
    assert_eq!(redact_entry("ht\ntp://u:p@h"), "http://***@h/");
    assert_eq!(redact_entry("\u{1}http://u:p@h"), "http://***@h/");
}

#[test]
fn unparsable_redaction_assumes_the_worst() {
    assert_eq!(
        redact_unparsable("http://user:p?ss@host:3128"),
        "http://***@host:3128"
    );
    assert_eq!(redact_unparsable("user:p#ss@host:3128"), "***@host:3128");
    assert_eq!(redact_unparsable("a@b@c"), "***@c");
    assert_eq!(redact_unparsable("nope"), "nope");
    assert_eq!(redact_unparsable("http://"), "http://");
    // The port-shaped check catches this one too: "//x" is not a
    // port, so it is hidden the same way as a real `host:port@user:pass`.
    assert_eq!(redact_unparsable("user@host://x"), "***@host:***");
}

fn invalid_proxy_message(input: &str) -> String {
    ProxyList::new([input]).unwrap_err().to_string()
}

#[test]
fn invalid_proxy_error_redacts_credentials_of_unsupported_scheme() {
    let message = invalid_proxy_message("ftp://user:pass@host:21");

    assert!(!message.contains("pass"), "{message}");
    assert!(!message.contains("user:"), "{message}");
    assert!(message.contains("***@"), "{message}");
}

#[test]
fn invalid_proxy_error_keeps_the_callers_schemeless_spelling() {
    // A schemeless input must not have the `http://` this function
    // synthesises internally leak into the message: the caller never
    // typed it, so the message should open with their own spelling.
    let schemeless = invalid_proxy_message("not a valid proxy url");

    assert_eq!(schemeless, "invalid proxy: not a valid proxy url");
}

#[test]
fn invalid_proxy_error_redacts_credentials_of_schemeless_input() {
    let schemeless_with_credentials = invalid_proxy_message("user:s3cret@not a url");

    assert!(
        !schemeless_with_credentials.contains("s3cret"),
        "{schemeless_with_credentials}"
    );
    assert!(
        schemeless_with_credentials.starts_with("invalid proxy: ***@"),
        "{schemeless_with_credentials}"
    );
}

#[test]
fn invalid_proxy_error_redacts_at_sign_in_password_with_scheme() {
    let message = invalid_proxy_message("ftp://user:p@ss@host:21");

    assert!(!message.contains("ss@"), "{message}");
    assert!(message.contains("***@host"), "{message}");
}

#[test]
fn invalid_proxy_error_redacts_at_sign_in_password_without_scheme() {
    let message = invalid_proxy_message("user:p@ss@host:99999");

    assert!(!message.contains("ss@"), "{message}");
    assert!(message.contains("***@host"), "{message}");
}

#[test]
fn invalid_proxy_error_redacts_whole_spelling_when_password_has_question_mark() {
    // A password containing `/`, `?` or `#` makes the URL fail to
    // parse; the authority boundary is then unknown, so the whole
    // spelling up to the last `@` must be redacted, not just the part
    // the parser would have guessed at.
    let err = ProxyList::new(["http://user:p?ss@host:3128"]).unwrap_err();
    let message = err.to_string();

    assert!(!message.contains("p?ss"), "{message}");
    assert_eq!(message, "invalid proxy: http://***@host:3128");
    assert_eq!(
        std::error::Error::source(&err).unwrap().to_string(),
        PERCENT_ENCODE_HINT_SPECIAL
    );
}

#[test]
fn invalid_proxy_error_redacts_whole_spelling_when_password_has_hash() {
    let message = invalid_proxy_message("user:p#ss@host:3128");

    assert!(!message.contains("p#ss"), "{message}");
    assert!(message.contains("***@host:3128"));
}

#[test]
fn invalid_proxy_error_leaves_input_without_credentials_alone() {
    // No `@` at all: nothing to redact.
    let no_credentials = invalid_proxy_message("http://");

    assert_eq!(no_credentials, "invalid proxy: http://");
}
