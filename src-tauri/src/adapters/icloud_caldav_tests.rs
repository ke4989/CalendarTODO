use crate::adapters::icloud_caldav::*;
use crate::adapters::{
    caldav::{AdapterAccountContext, ExecutingCalDavAdapter, discover_icloud},
    caldav_transport::*,
    persistence::{Database, DiscoveredCalendarRecord},
};
use crate::domain::{
    account::{ProviderKind, RegisterAccountProfile},
    calendar_sync::IncrementalSyncRequest,
};
use crate::ports::caldav::ReadOnlyCalendarSource;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

struct Step {
    method: DavMethod,
    url: String,
    body: Vec<u8>,
    depth: DavDepth,
    response: RawDavResponse,
}
struct Recording {
    name: &'static str,
    secret: u8,
    steps: Mutex<VecDeque<Step>>,
    calls: Mutex<usize>,
}
impl Recording {
    fn new(name: &'static str, secret: u8, steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            name,
            secret,
            steps: Mutex::new(steps.into()),
            calls: Mutex::new(0),
        })
    }
    fn done(&self) {
        assert!(self.steps.lock().unwrap().is_empty());
    }
}
impl DavExecutor for Recording {
    fn execute(
        &self,
        r: &DavRequest,
        a: &DavAuthentication,
    ) -> Result<RawDavResponse, DavExecutionError> {
        *self.calls.lock().unwrap() += 1;
        assert!(a.username() == self.name && a.password_bytes() == [self.secret]);
        let s = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected exchange");
        assert_eq!(
            (r.method(), r.url().as_str(), r.body(), r.depth()),
            (s.method, s.url.as_str(), s.body.as_slice(), s.depth)
        );
        Ok(s.response)
    }
}
fn step(
    method: DavMethod,
    url: &str,
    body: &[u8],
    depth: DavDepth,
    status: u16,
    location: Option<&str>,
    response: &[u8],
) -> Step {
    Step {
        method,
        url: url.into(),
        body: body.into(),
        depth,
        response: RawDavResponse::new(
            status,
            location.map(str::to_owned),
            Some(
                if method == DavMethod::Get {
                    "text/calendar"
                } else {
                    "application/xml"
                }
                .into(),
            ),
            response.into(),
        ),
    }
}
fn xml(properties: &str) -> Vec<u8> {
    format!("<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/</d:href><d:propstat><d:prop>{properties}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>").into_bytes()
}
fn calendars(home: &str) -> Vec<u8> {
    format!("<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>{home}calendar/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype><c:supported-calendar-component-set><c:comp name=\"VEVENT\"/></c:supported-calendar-component-set><d:displayname>Synthetic calendar</d:displayname><d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><d:write-content/></d:privilege></d:current-user-privilege-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>").into_bytes()
}
fn binding(shard: &str) -> ICloudBinding {
    ICloudBinding {
        bootstrap_url: BOOTSTRAP.into(),
        principal_url: format!("https://{shard}/principal/"),
        home_url: format!("https://{shard}/home/"),
        generation: 0,
    }
}

#[test]
fn icloud_exact_host_and_raw_url_normalization() {
    for suffix in ["icloud.com", "icloud.com.cn"] {
        for number in 0..1000 {
            let url = format!("https://p{number:03}-caldav.{suffix}/");
            assert!(parse(&url).is_ok());
        }
        // Apple also issues short zero-padded and unpadded shard numbers.
        for number in 0..100 {
            assert!(parse(&format!("https://p{number:02}-caldav.{suffix}/")).is_ok());
        }
        for number in 0..10 {
            assert!(parse(&format!("https://p{number}-caldav.{suffix}/")).is_ok());
        }
    }
    // A scheme-default port spelling is the same destination, canonicalized.
    assert_eq!(
        parse("https://p216-caldav.icloud.com.cn:443/home/")
            .unwrap()
            .as_str(),
        "https://p216-caldav.icloud.com.cn/home/"
    );
    assert_eq!(
        resolve(
            &parse(BOOTSTRAP).unwrap(),
            "//p216-caldav.icloud.com.cn:443/home/"
        )
        .unwrap()
        .as_str(),
        "https://p216-caldav.icloud.com.cn/home/"
    );
    for host in [
        "caldav.icloud.com",
        "caldav.icloud.com.cn",
        "p000-caldav.icloud.com",
        "p216-caldav.icloud.com.cn",
        "p235-caldav.icloud.com.cn",
        "p999-caldav.icloud.com",
    ] {
        assert!(parse(&format!("https://{host}/")).is_ok());
    }
    assert_eq!(
        parse("https://P216-CALDAV.ICLOUD.COM.CN/")
            .unwrap()
            .host_str(),
        Some("p216-caldav.icloud.com.cn")
    );
    for bad in [
        "https://p216-caldav.icloud.com.cn.evil.com/",
        "https://evil-p216-caldav.icloud.com.cn/",
        "https://caldav.icloud.com.evil.com/",
        "https://caldav.icloud.com@evil.com/",
        "http://p216-caldav.icloud.com.cn/",
        "https://p216-caldav.icloud.com.cn:8443/",
        "https://p216-caldav.icloud.com.cn:0443/",
        "https://p216-caldav.icloud.com.cn:443x/",
        "https://p216-caldav.icloud.com.cn:/",
        "https://p216-caldav.icloud.com.cn:443:443/",
        "https://p216-caldav.icloud.com.cn:4430/",
        "https://evil.example:443/",
        "https://user@p216-caldav.icloud.com.cn:443/",
        "https://[::1]:443/",
        "https://127.0.0.1/",
        "https://[::1]/",
        "https://p-caldav.icloud.com/",
        "https://p0216-caldav.icloud.com/",
        "https://p2160-caldav.icloud.com/",
        "https://p２16-caldav.icloud.com/",
        "https://p216-caldav.icloud.com.cn./",
        "https://%70%32%31%36-caldav.icloud.com.cn/",
        "https://ｐ216-caldav.icloud.com.cn/",
        "https://p216-caldav.icloud.com.cn/%2fescape",
        "https://p216-caldav.icloud.com.cn/%252fescape",
        "https:////p216-caldav.icloud.com.cn/",
        "https://p216-caldav.icloud.com.cn/a/../b",
        "https://p216-caldav.icloud.com.cn/?x=1",
        "https://p216-caldav.icloud.com.cn/#x",
        "https://p216-caldav.icloud.com.cn\\@evil.com/",
    ] {
        assert!(parse(bad).is_err(), "accepted unsafe spelling");
    }
}

#[test]
fn icloud_read_redirects_preserve_method_body_and_account_auth() {
    for method in [DavMethod::Get, DavMethod::Propfind, DavMethod::Report] {
        for status in [301, 302, 307, 308] {
            let first = parse(BOOTSTRAP).unwrap();
            let next = "https://p216-caldav.icloud.com.cn/entry/";
            // The server may spell the same destination with the HTTPS default port.
            let location = "https://p216-caldav.icloud.com.cn:443/entry/";
            let depth = if method == DavMethod::Get {
                DavDepth::Zero
            } else {
                DavDepth::One
            };
            let e = Recording::new(
                "first",
                1,
                vec![
                    step(
                        method,
                        BOOTSTRAP,
                        b"body",
                        depth,
                        status,
                        Some(location),
                        b"",
                    ),
                    step(method, next, b"body", depth, 207, None, b""),
                ],
            );
            let p = DavTransportPolicy::icloud_discovery(first.clone()).unwrap();
            let response = p
                .execute(
                    e.as_ref(),
                    DavRequest::new(method, first, b"body".to_vec(), depth, 1234).unwrap(),
                    &DavAuthentication::synthetic("first", &[1]),
                )
                .unwrap();
            assert_eq!(response.final_url().as_str(), next);
            e.done();
        }
    }
}

#[test]
fn icloud_unsafe_redirects_and_loops_never_receive_credentials() {
    for location in [
        "http://p216-caldav.icloud.com.cn/",
        "https://p216-caldav.icloud.com.cn.evil.com/",
        "https://127.0.0.1/",
        "https://user@p216-caldav.icloud.com.cn/",
        "https://p216-caldav.icloud.com.cn:8443/",
        "https://%70%32%31%36-caldav.icloud.com.cn/",
    ] {
        let e = Recording::new(
            "first",
            1,
            vec![step(
                DavMethod::Propfind,
                BOOTSTRAP,
                b"x",
                DavDepth::Zero,
                307,
                Some(location),
                b"",
            )],
        );
        assert!(
            DavTransportPolicy::icloud_discovery(parse(BOOTSTRAP).unwrap())
                .unwrap()
                .execute(
                    e.as_ref(),
                    DavRequest::new(
                        DavMethod::Propfind,
                        parse(BOOTSTRAP).unwrap(),
                        b"x".to_vec(),
                        DavDepth::Zero,
                        1000
                    )
                    .unwrap(),
                    &DavAuthentication::synthetic("first", &[1])
                )
                .is_err()
        );
        e.done();
        assert_eq!(*e.calls.lock().unwrap(), 1);
    }
    for status in [303, 307] {
        let e = Recording::new(
            "first",
            1,
            vec![step(
                DavMethod::Propfind,
                BOOTSTRAP,
                b"x",
                DavDepth::Zero,
                status,
                Some(BOOTSTRAP),
                b"",
            )],
        );
        assert!(
            DavTransportPolicy::icloud_discovery(parse(BOOTSTRAP).unwrap())
                .unwrap()
                .execute(
                    e.as_ref(),
                    DavRequest::new(
                        DavMethod::Propfind,
                        parse(BOOTSTRAP).unwrap(),
                        b"x".to_vec(),
                        DavDepth::Zero,
                        1000
                    )
                    .unwrap(),
                    &DavAuthentication::synthetic("first", &[1])
                )
                .is_err()
        );
        e.done();
        assert_eq!(*e.calls.lock().unwrap(), 1);
    }
}

fn discover(
    entry: &str,
    shard: &str,
    name: &'static str,
    secret: u8,
) -> crate::adapters::caldav::icloud::ICloudDiscovery {
    discover_spelled(entry, shard, name, secret, "")
}

/// `port` is how the server spells the authority in its response bodies; the
/// requests the client sends must always use the canonical portless URL.
fn discover_spelled(
    entry: &str,
    shard: &str,
    name: &'static str,
    secret: u8,
    port: &str,
) -> crate::adapters::caldav::icloud::ICloudDiscovery {
    let principal = "https://p007-caldav.icloud.com.cn/principal/";
    let home = format!("https://{shard}/home/");
    let listed = format!("https://{shard}{port}/home/");
    let first = xml(&format!(
        "<d:current-user-principal><d:href>https://p007-caldav.icloud.com.cn{port}/principal/</d:href></d:current-user-principal>"
    ));
    let second = xml(&format!(
        "<c:calendar-home-set><d:href>https://{shard}{port}/home/</d:href></c:calendar-home-set>"
    ));
    // Exact request bodies remain the real production discovery bodies.
    let body = crate::adapters::caldav::DISCOVERY_BODY;
    let e = Recording::new(
        name,
        secret,
        vec![
            step(
                DavMethod::Propfind,
                entry,
                body,
                DavDepth::Zero,
                207,
                None,
                &first,
            ),
            step(
                DavMethod::Propfind,
                principal,
                body,
                DavDepth::Zero,
                207,
                None,
                &second,
            ),
            step(
                DavMethod::Propfind,
                &home,
                crate::adapters::caldav::CALENDARS_BODY,
                DavDepth::One,
                207,
                None,
                &calendars(if port.is_empty() { "/home/" } else { &listed }),
            ),
        ],
    );
    let found = discover_icloud(
        entry,
        DavAuthentication::synthetic(name, &[secret]),
        e.clone(),
    )
    .unwrap();
    e.done();
    found
}

/// Real iCloud answers `calendar-home-set` with an absolute URL that spells the
/// scheme-default port (`https://pNNN-caldav.icloud.com[.cn]:443/<dsid>/calendars/`).
/// Discovery must canonicalize it, send only portless requests and store
/// portless binding and collection URLs.
#[test]
fn icloud_discovery_accepts_default_port_spelling_and_stores_canonical_urls() {
    for (entry, shard) in [
        (BOOTSTRAP, "p235-caldav.icloud.com.cn"),
        ("https://caldav.icloud.com.cn/", "p216-caldav.icloud.com.cn"),
        (BOOTSTRAP, "p34-caldav.icloud.com"),
        (BOOTSTRAP, "p07-caldav.icloud.com"),
    ] {
        let found = discover_spelled(entry, shard, "first", 1, ":443");
        assert_eq!(found.binding.home_url, format!("https://{shard}/home/"));
        assert!(!found.binding.principal_url.contains(":443"));
        assert!(
            found
                .calendars
                .iter()
                .all(|c| c.href == format!("https://{shard}/home/calendar/"))
        );
    }
}

#[test]
fn icloud_two_accounts_discover_different_homes_concurrently() {
    let a = std::thread::spawn(|| discover(BOOTSTRAP, "p235-caldav.icloud.com.cn", "first", 1));
    let b = std::thread::spawn(|| {
        discover(
            "https://caldav.icloud.com.cn/",
            "p216-caldav.icloud.com.cn",
            "second",
            2,
        )
    });
    let (a, b) = (a.join().unwrap(), b.join().unwrap());
    assert!(
        a.calendars[0]
            .href
            .starts_with("https://p235-caldav.icloud.com.cn/")
    );
    assert!(
        b.calendars[0]
            .href
            .starts_with("https://p216-caldav.icloud.com.cn/")
    );
    assert_ne!(a.binding.home_url, b.binding.home_url);
}

#[test]
fn icloud_bound_scope_rejects_other_account_home_and_collection() {
    let b = binding("p216-caldav.icloud.com.cn");
    let home = parse(&b.home_url).unwrap();
    let p = DavTransportPolicy::icloud_bound(&b, ["/home/calendar/".into()]).unwrap();
    assert!(
        p.resolve_exact_mutation_target(&home, "/home/calendar/", "/home/calendar/one.ics")
            .is_ok()
    );
    for route in [
        "https://p235-caldav.icloud.com.cn/home/calendar/",
        "/home/other/",
        "/home/",
    ] {
        assert!(
            p.resolve_exact_mutation_target(&home, route, "/home/other/one.ics")
                .is_err()
        );
    }
    assert!(
        b.collection("https://p235-caldav.icloud.com.cn/home/calendar/")
            .is_err()
    );
    assert!(b.collection("/outside/calendar/").is_err());
    let never = Recording::new("first", 1, vec![]);
    let wrong = parse("https://p235-caldav.icloud.com.cn/home/calendar/").unwrap();
    assert!(
        p.execute(
            never.as_ref(),
            DavRequest::new(DavMethod::Report, wrong, vec![], DavDepth::One, 1000).unwrap(),
            &DavAuthentication::synthetic("first", &[1])
        )
        .is_err()
    );
    assert_eq!(*never.calls.lock().unwrap(), 0);
}

#[test]
fn icloud_full_and_incremental_use_each_bound_home_and_sync_token() {
    let workers = [("p235-caldav.icloud.com.cn", "first", 1), ("p216-caldav.icloud.com.cn", "second", 2), ("p999-caldav.icloud.com", "third", 3), ("p34-caldav.icloud.com", "fourth", 4)]
        .into_iter().map(|(shard, name, secret)| std::thread::spawn(move || {
            let binding = binding(shard);
            let route = format!("{}calendar/", binding.home_url);
            let object = format!("{route}one.ics");
            let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:synthetic\r\nDTSTART:20261026T010000Z\r\nDTEND:20261026T020000Z\r\nSUMMARY:Synthetic\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
            let inventory = b"<d:multistatus xmlns:d=\"DAV:\"><d:sync-token>first-token</d:sync-token><d:response><d:href>/home/calendar/</d:href><d:propstat><d:prop/><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>/home/calendar/one.ics</d:href><d:propstat><d:prop><d:getetag>rev1</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>";
            let report = b"<d:multistatus xmlns:d=\"DAV:\"><d:sync-token>second-token</d:sync-token><d:response><d:href>/home/calendar/one.ics</d:href><d:propstat><d:prop><d:getetag>rev2</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>";
            let spelled = format!("https://{shard}:443/home/calendar/one.ics");
            let object_href=match secret {1=>"one.ics", 2=>&object, 4=>spelled.as_str(), _=>"/home/calendar/one.ics"};
            let inventory=String::from_utf8(inventory.to_vec()).unwrap().replace("/home/calendar/one.ics",object_href).into_bytes();
            let report=String::from_utf8(report.to_vec()).unwrap().replace("/home/calendar/one.ics",object_href).into_bytes();
            let e = Recording::new(name, secret, vec![
                step(DavMethod::Propfind, &binding.home_url, super::CALENDARS_BODY, DavDepth::One, 207, None, &calendars("/home/")),
                step(DavMethod::Propfind, &route, super::INVENTORY_BODY, DavDepth::One, 207, None, &inventory),
                step(DavMethod::Get, &object, b"", DavDepth::Zero, 200, None, raw),
                step(DavMethod::Report, &route, &super::sync_collection_body("first-token").unwrap(), DavDepth::One, 207, None, &report),
                step(DavMethod::Get, &object, b"", DavDepth::Zero, 200, None, raw),
            ]);
            let adapter = ExecutingCalDavAdapter::new(AdapterAccountContext {
                account_id: A.into(), provider_kind: "caldav".into(), user_label: "Synthetic".into(),
                credential_ref: format!("calendar-todo/account/{A}"), region_profile: "icloud-cn".into(),
                trusted_hosts: BTreeSet::from([shard.into()]), calendar_ids_by_href: BTreeMap::from([(route.clone(), B.into())]),
            }, &binding.home_url, DavAuthentication::synthetic(name, &[secret]), e.clone(), 1).unwrap().with_icloud_binding(binding).unwrap();
            let full = adapter.collect_full_inventory().unwrap();
            assert_eq!(full.calendars[0].collection_url, route);
            assert_eq!(full.calendars[0].sync_token.as_deref(), Some("first-token"));
            let incremental = adapter.collect_incremental(&[IncrementalSyncRequest{calendar_id:B.into(),sync_token:"first-token".into()}]).unwrap();
            assert_eq!(incremental.calendars[0].new_sync_token,"second-token");
            assert_eq!(incremental.calendars[0].changed_resources[0].etag,"rev2");
            e.done();
        })).collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn icloud_discovery_rejects_invalid_home_before_sending_and_foreign_collection_before_commit() {
    let principal = "https://p007-caldav.icloud.com.cn/principal/";
    for bad in [
        "http://p216-caldav.icloud.com.cn/home/",
        "https://evil.com/home/",
        "https://p216-caldav.icloud.com.cn:8443/home/",
    ] {
        let first = xml(&format!(
            "<d:current-user-principal><d:href>{principal}</d:href></d:current-user-principal><c:calendar-home-set><d:href>{bad}</d:href></c:calendar-home-set>"
        ));
        let e = Recording::new(
            "first",
            1,
            vec![step(
                DavMethod::Propfind,
                BOOTSTRAP,
                super::DISCOVERY_BODY,
                DavDepth::Zero,
                207,
                None,
                &first,
            )],
        );
        assert!(
            discover_icloud(
                BOOTSTRAP,
                DavAuthentication::synthetic("first", &[1]),
                e.clone()
            )
            .is_err()
        );
        e.done();
        assert_eq!(*e.calls.lock().unwrap(), 1);
    }
    let home = "https://p216-caldav.icloud.com.cn/home/";
    let first = xml(&format!(
        "<d:current-user-principal><d:href>{principal}</d:href></d:current-user-principal><c:calendar-home-set><d:href>{home}</d:href></c:calendar-home-set>"
    ));
    for bad in ["https://p235-caldav.icloud.com.cn/home/", "/outside/"] {
        let e = Recording::new(
            "first",
            1,
            vec![
                step(
                    DavMethod::Propfind,
                    BOOTSTRAP,
                    super::DISCOVERY_BODY,
                    DavDepth::Zero,
                    207,
                    None,
                    &first,
                ),
                step(
                    DavMethod::Propfind,
                    home,
                    super::CALENDARS_BODY,
                    DavDepth::One,
                    207,
                    None,
                    &calendars(bad),
                ),
            ],
        );
        assert!(
            discover_icloud(
                BOOTSTRAP,
                DavAuthentication::synthetic("first", &[1]),
                e.clone()
            )
            .is_err()
        );
        e.done();
    }
}

#[test]
fn icloud_redirect_hop_budget_and_two_node_loop_are_bounded() {
    for hops in [5, 6] {
        let urls = (0..=hops)
            .map(|n| format!("https://p216-caldav.icloud.com.cn/hop{n}/"))
            .collect::<Vec<_>>();
        let mut steps = Vec::new();
        for n in 0..hops {
            steps.push(step(
                DavMethod::Propfind,
                &urls[n],
                b"body",
                DavDepth::One,
                308,
                Some(&urls[n + 1]),
                b"",
            ));
        }
        if hops == 5 {
            steps.push(step(
                DavMethod::Propfind,
                &urls[hops],
                b"body",
                DavDepth::One,
                207,
                None,
                b"",
            ));
        }
        let e = Recording::new("first", 1, steps);
        let result = DavTransportPolicy::icloud_discovery(parse(&urls[0]).unwrap())
            .unwrap()
            .execute(
                e.as_ref(),
                DavRequest::new(
                    DavMethod::Propfind,
                    parse(&urls[0]).unwrap(),
                    b"body".to_vec(),
                    DavDepth::One,
                    1234,
                )
                .unwrap(),
                &DavAuthentication::synthetic("first", &[1]),
            );
        assert_eq!(result.is_ok(), hops == 5);
        e.done();
        assert_eq!(*e.calls.lock().unwrap(), 6);
    }
    let next = "https://p216-caldav.icloud.com.cn/";
    let e = Recording::new(
        "first",
        1,
        vec![
            step(
                DavMethod::Propfind,
                BOOTSTRAP,
                b"",
                DavDepth::Zero,
                302,
                Some(next),
                b"",
            ),
            step(
                DavMethod::Propfind,
                next,
                b"",
                DavDepth::Zero,
                301,
                Some(BOOTSTRAP),
                b"",
            ),
        ],
    );
    assert!(
        DavTransportPolicy::icloud_discovery(parse(BOOTSTRAP).unwrap())
            .unwrap()
            .execute(
                e.as_ref(),
                DavRequest::new(
                    DavMethod::Propfind,
                    parse(BOOTSTRAP).unwrap(),
                    vec![],
                    DavDepth::Zero,
                    1000
                )
                .unwrap(),
                &DavAuthentication::synthetic("first", &[1])
            )
            .is_err()
    );
    e.done();
    assert_eq!(*e.calls.lock().unwrap(), 2);
}

const A: &str = "33333333-3333-4333-8333-333333333333";
const B: &str = "44444444-4444-4444-8444-444444444444";
fn profile(db: &Database, id: &str) {
    db.upsert_account_profile(RegisterAccountProfile {
        account_id: id.into(),
        provider_kind: ProviderKind::CalDav,
        user_label: "Synthetic".into(),
        credential_ref: format!("calendar-todo/account/{id}"),
        region_profile: "icloud-cn".into(),
        enabled: true,
    })
    .unwrap();
}
fn record(id: &str, route: &str) -> DiscoveredCalendarRecord {
    DiscoveredCalendarRecord {
        calendar_id: id.into(),
        collection_url: route.into(),
        display_name: "Synthetic".into(),
        server_color: None,
        privileges: "read,write-content".into(),
    }
}
#[test]
fn icloud_persisted_bindings_restart_and_account_lease_isolation() {
    struct TestDir(std::path::PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = TestDir(
        std::env::temp_dir().join(format!("calendar-todo-icloud-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(&dir.0).unwrap();
    let path = dir.0.join("synthetic.sqlite3");
    let db = Database::open(&path).unwrap();
    profile(&db, A);
    profile(&db, B);
    assert!(db.icloud_binding(A.into()).unwrap().is_none());
    for (id, shard) in [
        (A, "p235-caldav.icloud.com.cn"),
        (B, "p216-caldav.icloud.com.cn"),
    ] {
        let b = binding(shard);
        db.replace_icloud_discovery(
            id.into(),
            vec![record(
                &format!("{}1", &id[..35]),
                &format!("{}calendar/", b.home_url),
            )],
            b,
            1,
        )
        .unwrap();
    }
    let (_, a_cancel) = db.icloud_binding_lease(A.into()).unwrap().unwrap();
    let (_, b_cancel) = db.icloud_binding_lease(B.into()).unwrap().unwrap();
    let mut next = binding("p217-caldav.icloud.com.cn");
    next.principal_url = "https://p008-caldav.icloud.com.cn/principal/".into();
    db.replace_icloud_discovery(
        B.into(),
        vec![record(
            "44444444-4444-4444-8444-444444444441",
            &format!("{}calendar/", next.home_url),
        )],
        next,
        2,
    )
    .unwrap();
    assert!(!a_cancel.load(std::sync::atomic::Ordering::SeqCst));
    assert!(b_cancel.load(std::sync::atomic::Ordering::SeqCst));
    drop(db);
    let db = Database::open(&path).unwrap();
    assert!(
        db.icloud_binding(A.into())
            .unwrap()
            .unwrap()
            .home_url
            .contains("p235-")
    );
    assert!(
        db.icloud_binding(B.into())
            .unwrap()
            .unwrap()
            .home_url
            .contains("p217-")
    );
}
