use super::*;

fn engine(list: &str) -> FilterEngine {
    let mut engine = FilterEngine::new();
    engine.add_list(Category::Ads, list).unwrap();
    engine
}

fn req(url: &str) -> FilterRequest<'_> {
    FilterRequest { url, kind: RequestType::Image, page_host: Some("news.example"), third_party: true }
}

fn blocked(engine: &FilterEngine, request: &FilterRequest) -> bool {
    matches!(engine.check(request), Verdict::Block { .. })
}

#[test]
fn domain_anchors_match_the_host_and_its_subdomains_only() {
    let e = engine("||tracker.example^");
    for url in ["https://tracker.example/", "https://a.b.tracker.example/x?y", "http://tracker.example:8080/p", "https://user@tracker.example/"] {
        assert!(blocked(&e, &req(url)), "{url}");
    }
    for url in ["https://nottracker.example/", "https://tracker.example.evil.net/", "https://example.com/?u=tracker.example", "https://tracker.examples/"] {
        assert!(!blocked(&e, &req(url)), "{url}");
    }
}

#[test]
fn hosts_file_lines_are_domain_rules() {
    let e = engine("# a hosts file\n0.0.0.0 ads.example\n127.0.0.1 more.example # trailing comment\n0.0.0.0 localhost\n::1 ip6.example\n");
    assert_eq!(e.len(), 3);
    assert!(blocked(&e, &req("https://ads.example/x")));
    assert!(blocked(&e, &req("https://cdn.more.example/")));
    assert!(blocked(&e, &req("https://ip6.example/")));
    assert!(!blocked(&e, &req("http://localhost/")));
}

#[test]
fn patterns_wildcards_separators_and_anchors() {
    let e = engine("/banner/*/ad_\n|https://start.example/only\nend.js|\n||cdn.example/ads^\n");
    assert!(blocked(&e, &req("https://x.example/banner/300x250/ad_1.png")));
    assert!(!blocked(&e, &req("https://x.example/banner/ad_1.png")), "`*` needs its own slashes around it here");
    assert!(blocked(&e, &req("https://start.example/only/this")));
    assert!(!blocked(&e, &req("https://x.example/?u=https://start.example/only")), "`|` anchors at the start");
    assert!(blocked(&e, &req("https://x.example/file/end.js")));
    assert!(!blocked(&e, &req("https://x.example/file/end.js?v=1")), "`|` anchors at the end");
    assert!(blocked(&e, &req("https://cdn.example/ads/x.png")));
    assert!(blocked(&e, &req("https://cdn.example/ads")), "`^` matches the end of the URL");
    assert!(!blocked(&e, &req("https://cdn.example/adsx/")), "`^` is not a letter");
}

#[test]
fn matching_ignores_case() {
    let e = engine("||Tracker.Example^\n/AdServer/*");
    assert!(blocked(&e, &req("https://TRACKER.example/")));
    assert!(blocked(&e, &req("https://x.example/adserver/p")));
}

#[test]
fn exceptions_lift_blocks_but_not_important_ones() {
    let e = engine("||ads.example^\n@@||ads.example/allowed/\n||must.example^$important\n@@||must.example^");
    assert!(blocked(&e, &req("https://ads.example/x")));
    assert!(!blocked(&e, &req("https://ads.example/allowed/x")));
    assert!(blocked(&e, &req("https://must.example/x")), "$important wins over an exception");
}

#[test]
fn third_party_and_domain_options() {
    let e = engine("||cdn.example^$third-party\n||own.example^$~third-party\n/promo/$domain=news.example|~sports.news.example");
    let mut r = req("https://cdn.example/x");
    assert!(blocked(&e, &r));
    r.third_party = false;
    assert!(!blocked(&e, &r), "first-party requests to cdn.example are fine");
    let mut own = req("https://own.example/x");
    assert!(!blocked(&e, &own));
    own.third_party = false;
    assert!(blocked(&e, &own));

    let promo = |page: &'static str| FilterRequest { page_host: Some(page), ..req("https://x.example/promo/1") };
    assert!(blocked(&e, &promo("news.example")));
    assert!(blocked(&e, &promo("www.news.example")));
    assert!(!blocked(&e, &promo("sports.news.example")));
    assert!(!blocked(&e, &promo("other.example")));
    assert!(!blocked(&e, &FilterRequest { page_host: None, ..promo("x") }), "no page: a domain= rule does not apply");
}

#[test]
fn request_types_and_documents() {
    let e = engine("||scripts.example^$script\n||noimg.example^$~image\n||page.example^\n||nav.example^$document");
    let typed = |url: &'static str, kind: RequestType| FilterRequest { kind, ..req(url) };
    assert!(blocked(&e, &typed("https://scripts.example/a.js", RequestType::Script)));
    assert!(!blocked(&e, &typed("https://scripts.example/a.png", RequestType::Image)));
    assert!(blocked(&e, &typed("https://noimg.example/a.js", RequestType::Script)));
    assert!(!blocked(&e, &typed("https://noimg.example/a.png", RequestType::Image)));
    assert!(!blocked(&e, &typed("https://page.example/", RequestType::Document)), "navigations need $document");
    assert!(blocked(&e, &typed("https://page.example/x", RequestType::XmlHttpRequest)));
    assert!(blocked(&e, &typed("https://nav.example/", RequestType::Document)));
}

#[test]
fn what_cannot_be_evaluated_is_skipped_and_what_is_malformed_is_rejected() {
    let mut e = FilterEngine::new();
    let report = e
        .add_list(
            Category::Ads,
            "[Adblock Plus 2.0]\n! comment\n##.ad-banner\nexample.com##.ad\n/ads?[0-9]+/\n||x.example^$csp=script-src 'none'\n||y.example^$redirect=noop.js\n||z.example^$subdocument\n\
             *\n$third-party\n||bad domain.example^\n|a|b|\n||ok.example^\n",
        )
        .unwrap();
    assert_eq!(report, ListReport { rules: 1, skipped: 6, rejected: 4 });
}

#[test]
fn lists_beyond_the_limits_are_refused_whole() {
    let mut e = FilterEngine::new();
    assert_eq!(e.add_list(Category::Ads, &"x".repeat(MAX_LIST_BYTES + 1)), Err(ListError::TooLarge));
    let long = format!("||{}.example^\n||ok.example^", "a".repeat(MAX_RULE_BYTES));
    assert_eq!(e.add_list(Category::Ads, &long).unwrap(), ListReport { rules: 1, skipped: 0, rejected: 1 });
    let many: String = (0..MAX_RULES + 1).map(|i| format!("||d{i}.example^\n")).collect();
    let before = e.len();
    assert_eq!(e.add_list(Category::Trackers, &many), Err(ListError::TooManyRules));
    assert_eq!(e.len(), before, "nothing of a refused list is added");
}

#[test]
fn the_block_names_its_category_and_rule() {
    let mut e = FilterEngine::new();
    e.add_list(Category::AiCrawlers, "||ai.example^").unwrap();
    assert_eq!(e.check(&req("https://ai.example/x")), Verdict::Block { category: Category::AiCrawlers, rule: "||ai.example^".into() });
}

#[test]
fn the_built_in_list_blocks_third_party_trackers_only() {
    let e = FilterEngine::with_built_in();
    assert!(e.len() > 10);
    assert!(blocked(&e, &req("https://www.google-analytics.com/analytics.js")));
    let first_party = FilterRequest { third_party: false, page_host: Some("google-analytics.com"), ..req("https://google-analytics.com/") };
    assert!(!blocked(&e, &first_party));
    assert!(!blocked(&e, &req("https://example.com/")));
}

#[test]
fn rules_are_indexed_not_scanned() {
    let mut list = String::new();
    for i in 0..20_000 {
        list.push_str(&format!("||d{i}.example^\n/path{i}/segment\n"));
    }
    list.push_str("*ad_\n"); // no whole token: examined for every request
    let e = engine(&list);
    assert_eq!(e.len(), 40_001);
    assert_eq!(e.unindexed(), 1);
}

/// The golden corpus: URLs known to be bad (blocked) and known to be good
/// (allowed) against a list of typical rule shapes.
#[test]
fn the_golden_corpus() {
    let list = "! typical shapes\n||doubleclick.example^\n||adserver.example^$third-party\n/pagead/js/*\n/ads/banner_\n\
                ||analytics.example/collect^\n@@||adserver.example/healthy^\n0.0.0.0 telemetry.example\n||cdn.example/*/ad-*.js|$script\n";
    let e = engine(list);
    let bad = [
        ("https://doubleclick.example/x", RequestType::Image),
        ("https://g.doubleclick.example/pagead/id", RequestType::Script),
        ("https://adserver.example/serve?id=1", RequestType::Script),
        ("https://site.example/pagead/js/adsbygoogle.js", RequestType::Script),
        ("https://site.example/ads/banner_728.png", RequestType::Image),
        ("https://analytics.example/collect?v=1", RequestType::XmlHttpRequest),
        ("https://eu.telemetry.example/v1/events", RequestType::XmlHttpRequest),
        ("https://cdn.example/lib/ad-loader.js", RequestType::Script),
    ];
    let good = [
        ("https://news.example/article", RequestType::Document),
        ("https://adserver.example/healthy", RequestType::Script),
        ("https://site.example/pagead/", RequestType::Script),
        ("https://site.example/downloads/banner_728.png", RequestType::Image),
        ("https://analytics.example/collected", RequestType::XmlHttpRequest),
        ("https://cdn.example/lib/ad-loader.js?v=2", RequestType::Script),
        ("https://cdn.example/lib/ad-loader.js", RequestType::Image),
        ("https://mytelemetry.example/", RequestType::Image),
    ];
    for (url, kind) in bad {
        assert!(blocked(&e, &FilterRequest { kind, ..req(url) }), "should block {url}");
    }
    for (url, kind) in good {
        assert!(!blocked(&e, &FilterRequest { kind, ..req(url) }), "should allow {url}");
    }
}

/// The per-request budget (ADR 019): 100,000 rules of mixed shapes, a long
/// hostile URL, and a pattern built to make wildcard matching work hard.
#[test]
fn matching_stays_within_its_budget() {
    let mut list = String::new();
    for i in 0..50_000 {
        list.push_str(&format!("||d{i}.example^\n/p{i}/*/q{i}^\n"));
    }
    list.push_str("*a*a*a*a*a*a*a*a*a*a*b\n");
    let e = engine(&list);
    let hostile = format!("https://{}.example/{}", "sub.".repeat(60), "a/".repeat(2000));
    let urls = [hostile.as_str(), "https://www.example.com/p123/x/y/q123/", "https://d49999.example/", "https://plain.example/index.html"];
    let started = std::time::Instant::now();
    let mut checks = 0;
    for _ in 0..50 {
        for url in urls {
            e.check(&req(url));
            checks += 1;
        }
    }
    let per_check = started.elapsed() / checks;
    eprintln!("filter: {per_check:?} per request over {} rules ({} unindexed)", e.len(), e.unindexed());
    // Debug builds are far slower; the release figure is in the milestone file.
    let budget = if cfg!(debug_assertions) { std::time::Duration::from_millis(20) } else { std::time::Duration::from_millis(2) };
    assert!(per_check < budget, "{per_check:?} per request");
}

