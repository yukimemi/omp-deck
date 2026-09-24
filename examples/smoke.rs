//! `examples/smoke.rs` — release-time smoke target.
//!
//! Runs the JSON parsing and HTML rendering paths of the produced build on a
//! fixture. No network and no subprocess: omp-deck's only outbound action is
//! spawning `omp`, which a release runner does not have.

use omp_deck::bind::choose_bind;
use omp_deck::model::parse_hosts;
use omp_deck::view::{render_page, render_table};

const HOSTS: &str = include_str!("../tests/fixtures/hosts.json");
const EMPTY: &str = include_str!("../tests/fixtures/empty.json");

fn main() {
    let hosts = parse_hosts(HOSTS).expect("fixture parses");
    assert_eq!(hosts.len(), 2, "fixture host count");
    assert_eq!(hosts[0].display_name(), "omp-deck", "cwd leaf fallback");

    let html = render_page(&hosts, 1_700_000_300_000);
    assert_eq!(html.matches("<article").count(), 2, "one card per host");
    assert!(!html.contains("<script>alert(1)"), "sessionName is escaped");
    assert!(html.contains("&lt;script&gt;"), "escaped form is present");

    let empty = parse_hosts(EMPTY).expect("empty fixture parses");
    assert!(render_page(&empty, 0).contains("no live omp sessions"));
    assert!(render_table(&hosts, 1_700_000_300_000).contains("omp-deck"));

    let bind = choose_bind(None, None).expect("default bind");
    assert!(!bind.addr.ip().is_unspecified(), "never binds 0.0.0.0");

    println!("smoke: ok ({} cards)", hosts.len());
}
