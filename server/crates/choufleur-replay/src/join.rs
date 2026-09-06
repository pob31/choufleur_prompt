//! The addresses an operator is given, and the code that carries one.
//!
//! Shared, because both servers now need it and for different reasons. The library
//! server prints these at startup — it is the only thing on screen before anything is
//! open. The show server hands them to the page, which is where an operator actually
//! is when somebody asks how to join: a show is open, the window is on the script, and
//! walking back to the library screen to read out an address is the whole problem.

use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

/// How an address was found, which is the only thing that says who can resolve it.
///
/// An Android browser does not do mDNS: `choufleur.local` is a name it will not look up,
/// and the number is the only entry on this list a phone can reach the machine by. An
/// iPad and a Mac resolve both. Something has to choose which of the two goes in front
/// of a camera, and the shape of the string is the wrong place to ask — the two are
/// distinguishable by eye today and by accident on the first day something adds a third
/// kind of name.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum AddrKind {
    /// A Bonjour name. Outlives the lease the number does not; Android will not look it up.
    Name,
    /// A number. Reaches anything on the network, until the lease changes.
    Ip,
}

/// One way in, ready to be read out or drawn.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Operator {
    pub kind: AddrKind,
    /// The host alone, for building another path onto this machine.
    pub host: String,
    /// The whole address, exactly as it goes on a screen and out of a mouth.
    pub url: String,
}

/// The addresses an operator can reach this machine on, best first.
///
/// The Bonjour name comes first on purpose. It survives the DHCP lease that the address
/// does not — the number can change between the get-in and the half — and an iPad
/// resolves `.local` with nothing configured.
pub fn lan_addresses(port: u16) -> Vec<Operator> {
    // The address is assembled here and only here. It used to be written out at every
    // call site, which is several copies of one string with nothing keeping them equal.
    let at = |kind, host: String| Operator {
        url: format!("http://{host}:{port}/"),
        host,
        kind,
    };
    let mut out = Vec::new();
    if let Ok(o) = std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
    {
        let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !name.is_empty() {
            out.push(at(AddrKind::Name, format!("{name}.local")));
        }
    }
    // Which interface a tablet would arrive on, asked of the routing table rather than
    // guessed from a list. A "connected" UDP socket sends no packet — it only picks the
    // route — so this costs nothing and touches the network not at all. The address is
    // in the documentation range, which is routed nowhere by design.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("192.0.2.1:9").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if ip != "0.0.0.0" {
                    out.push(at(AddrKind::Ip, ip));
                }
            }
        }
    }
    out
}

/// The library server's port, when this process was started by one.
///
/// `None` for a `serve` run straight from a terminal, where there is no library server
/// and therefore no `/list/<id>` to point anybody at. The difference decides which
/// address goes in a code, so it is answered here rather than guessed by the page.
pub fn ui_port() -> Option<u16> {
    std::env::var("CHOUFLEUR_UI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
}

/// What the page needs to draw a way in for every cue list it is showing.
///
/// `durable` is the whole reason this is a server's answer rather than a page's guess.
/// With a library server behind us the address to hand out is `/list/<id>` on *its*
/// port, which never moves; without one there is no such thing, and the only address
/// that works is this server's own — which is chosen at open time and slides upward, so
/// it is worth having, and not worth bookmarking. The page says which it is holding.
pub fn where_to_join(own_port: u16) -> serde_json::Value {
    let ui = ui_port();
    let port = ui.unwrap_or(own_port);
    serde_json::json!({
        "durable": ui.is_some(),
        "addresses": lan_addresses(port),
    })
}

/// How much of an address this will draw.
///
/// The payload is `http://192.168.1.40:8080/list/conduite-son` and its longest plausible
/// relative is not twice that. The cap is not a resource limit — encoding is
/// microseconds — it is a statement about what this endpoint is for: anything longer is
/// not an address, and a symbol that dense is unreadable off a screen anyway.
const QR_MAX: usize = 512;

#[derive(Deserialize)]
pub struct QrQuery {
    pub d: String,
}

/// `/qr?d=<url>` — one QR code, as SVG.
///
/// Knows nothing about shows, lists or addresses, and that is the point: the page builds
/// the address it is about to print under the code and hands that same string to this,
/// so the picture and the caption cannot disagree. They could, if each end composed its
/// own.
///
/// Black on white, always, and never from the request. Two reasons, and the second is
/// the serious one. A camera in a dark wing wants the contrast the format was designed
/// for, and the palette everything else is written in would cost a get-in to discover.
/// And `svg::Color` is interpolated verbatim into a `fill="…"` attribute: a colour taken
/// from a query string is one quote away from being a `<script>` on this origin. The
/// payload itself never reaches the markup — it becomes path geometry.
///
/// Takes no state, so it serves from either router unchanged.
pub async fn qr(axum::extract::Query(q): axum::extract::Query<QrQuery>) -> axum::response::Response {
    if q.d.is_empty() || q.d.len() > QR_MAX {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!(
                "a join code carries an address; {} bytes is not one",
                q.d.len()
            ),
        )
            .into_response();
    }
    let code = match qrcode::QrCode::new(q.d.as_bytes()) {
        Ok(c) => c,
        Err(e) => return (axum::http::StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let svg = code
        .render()
        // The quiet zone is four modules and it is *inside* the picture — the renderer
        // fills the whole canvas with the light colour first — so the margin a scanner
        // needs survives being dropped into any layout, including a dark one.
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build();
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "image/svg+xml; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        svg,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The border is what a scanner locks on to, and it is not drawn — it is the light
    /// fill showing through around the modules. So if that fill ever goes dark to match
    /// the rest of the screen, the code stops working and nothing else about it changes.
    #[test]
    fn a_join_code_keeps_its_light_border() {
        let code = qrcode::QrCode::new(b"http://192.168.1.40:8080/list/conduite-son").unwrap();
        // Every QR version is 4n+17 modules across.
        assert_eq!((code.width() - 17) % 4, 0);
        let svg = code
            .render()
            .dark_color(qrcode::render::svg::Color("#000000"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .build();
        assert!(svg.contains(r##"fill="#ffffff""##), "{svg}");
        // Four modules of quiet zone on every side, which is what `render` asks for.
        //
        // Checked in modules rather than in pixels: the renderer picks its own scale, so
        // the numbers in the markup are module counts times something this test has no
        // business knowing. Derive that factor, then assert the first dark module starts
        // four modules in.
        let across = code.width() + 8;
        let side: usize = svg
            .split(r#"viewBox="0 0 "#)
            .nth(1)
            .and_then(|t| t.split(' ').next())
            .and_then(|n| n.parse().ok())
            .expect("a viewBox");
        assert_eq!(side % across, 0, "{side} is not a whole number of modules");
        let quiet = 4 * (side / across);
        assert!(svg.contains(&format!(r#"d="M{quiet} {quiet}h"#)), "{svg}");
    }

    /// Without a library server there is no `/list/<id>`, and a page that assumed one
    /// would draw a code leading nowhere.
    #[test]
    fn a_show_server_on_its_own_says_its_links_are_not_durable() {
        // The variable is process-wide, so this asserts the shape rather than setting it.
        let v = where_to_join(8081);
        assert_eq!(v["durable"], serde_json::json!(ui_port().is_some()));
        assert!(v["addresses"].is_array());
    }
}
