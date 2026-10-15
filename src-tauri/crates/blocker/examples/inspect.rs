//! Descarga las listas reales y muestra qué haría el bloqueador en una URL.
//!
//!   cargo run -p blocker --example inspect --features content-blocking -- https://music.youtube.com/

use std::time::Instant;

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or("https://music.youtube.com/".into());
    let cache = std::env::temp_dir().join("pulpo-filter-lists");

    let t = Instant::now();
    let lists = blocker::Lists::load(&cache).expect("listas");
    println!("listas: {:?} ({})", t.elapsed(), lists.fingerprint());

    let t = Instant::now();
    let b = blocker::Blocker::new(&lists).expect("motor");
    println!("motor: {:?}", t.elapsed());

    #[cfg(feature = "content-blocking")]
    {
        let t = Instant::now();
        let json = lists.content_blocking_json().expect("content blocking");
        let n = json.matches("\"trigger\"").count();
        println!(
            "reglas WebKit: {n} ({} KB) en {:?}",
            json.len() / 1024,
            t.elapsed()
        );
    }

    let script = b.page_script(&url);
    println!("script para {url}: {} KB", script.len() / 1024);
    for line in script
        .lines()
        .filter(|l| l.starts_with("try {") || l.contains("(\""))
    {
        if line.len() < 200 && !line.starts_with("try") {
            println!("  {line}");
        }
    }
    for (u, t) in [
        (
            "https://googleads.g.doubleclick.net/pagead/id",
            "xmlhttprequest",
        ),
        (
            "https://www.youtube.com/api/stats/ads?x=1",
            "xmlhttprequest",
        ),
        (
            "https://music.youtube.com/youtubei/v1/player",
            "xmlhttprequest",
        ),
    ] {
        println!("bloquear {u}: {}", b.should_block(u, &url, t));
    }
}
