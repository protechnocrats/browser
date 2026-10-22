//! Performance audit: a page is shown once its document and stylesheets
//! have loaded; its images appear as they arrive. (The first frame used to
//! wait for every image on the page.)

use engine::{Engine, EngineConfig, EngineEvent, ImageLoading, LoadRequest, PageCommand, PageParams, Viewport};
use rendering::DisplayItem;
use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);
const PNG: &[u8] = include_bytes!("../../images/tests/fixtures/quadrants.png");

/// Serves the page at once, and the image only once `release` fires.
fn serve(page: &'static str, release: Receiver<()>) -> String {
    let release = Arc::new(Mutex::new(release));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let _ = reader.read_line(&mut request);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let body: &[u8] = if request.contains("/img.png") {
                    let _ = release.lock().unwrap().recv_timeout(WAIT);
                    PNG
                } else {
                    page.as_bytes()
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(body);
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn images_drawn(events: &[EngineEvent]) -> Option<usize> {
    events.iter().rev().find_map(|e| match e {
        EngineEvent::Frame { scene, .. } => Some(scene.items.iter().filter(|i| matches!(i, DisplayItem::Image { .. })).count()),
        _ => None,
    })
}

#[test]
fn a_page_is_drawn_before_its_images_arrive_and_again_when_they_do() {
    let (release, gate) = channel();
    let base = serve(
        "<html><head><style>.bg { background-image: url(/img.png); width: 20px; height: 20px; }</style></head>\
         <body><p>text first</p><img src=\"/img.png\"><div class=\"bg\"></div></body></html>",
        gate,
    );
    let mut engine = Engine::new(EngineConfig { image_loading: ImageLoading::Progressive, ..EngineConfig::default() });
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    engine.send(page, PageCommand::Load(LoadRequest::Url(format!("{base}/page.html"))));

    // Committed and drawn — with the image still held back by the server.
    let deadline = Instant::now() + WAIT;
    let first = loop {
        assert!(Instant::now() < deadline, "the page was never shown while its image was loading");
        let events = engine.update_blocking(Instant::now(), WAIT);
        if events.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { .. })) {
            break images_drawn(&events).expect("the committed page is drawn at once");
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(first, 0, "no image yet");
    // Milestone 38b (ADR 020): an image in flight is not polled for; its
    // arrival wakes the page.
    assert!(engine.next_wake().is_none(), "an image in flight must not keep the page polling");

    release.send(()).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "the arrived image was never drawn");
        let events = engine.update_blocking(Instant::now(), WAIT);
        if let Some(n) = images_drawn(&events).filter(|n| *n > 0) {
            assert_eq!(n, 2, "the <img> (laid out at its image's size) and the background");
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // Everything arrived: the page goes idle.
    let _ = engine.update_blocking(Instant::now(), WAIT);
    assert!(engine.next_wake().is_none(), "nothing left to poll");
}

#[test]
fn before_first_frame_loading_waits_for_the_images() {
    let (release, gate) = channel();
    let base = serve("<html><body><img src=\"/img.png\"></body></html>", gate);
    let mut engine = Engine::new(EngineConfig { image_loading: ImageLoading::BeforeFirstFrame, ..EngineConfig::default() });
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    engine.send(page, PageCommand::Load(LoadRequest::Url(format!("{base}/page.html"))));
    let released = Instant::now() + Duration::from_millis(100);
    let mut release = Some(release);
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "the navigation never committed");
        if Instant::now() >= released {
            if let Some(r) = release.take() {
                r.send(()).unwrap();
            }
        }
        let events = engine.update_blocking(Instant::now(), WAIT);
        if events.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { .. })) {
            assert!(release.is_none(), "committed before the image was served");
            assert_eq!(images_drawn(&events), Some(1), "the first frame has the image");
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
