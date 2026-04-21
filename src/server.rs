use crate::config::{self, SharedConfig};
use crate::totp;
use crate::updater;
use qrcode::render::svg;
use qrcode::QrCode;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct ServerState {
    pub cfg: SharedConfig,
    pub stop: Arc<AtomicBool>,
    pub pending_secret: Mutex<Option<String>>,
    pub addr: SocketAddr,
    pub update_status: updater::SharedStatus,
}

pub fn start(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    update_status: updater::SharedStatus,
) -> Arc<ServerState> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
    listener
        .set_nonblocking(false)
        .expect("listener blocking mode");
    let addr = listener.local_addr().expect("local_addr");
    let state = Arc::new(ServerState {
        cfg,
        stop: stop.clone(),
        pending_secret: Mutex::new(None),
        addr,
        update_status,
    });

    let st = state.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            if st.stop.load(Ordering::Relaxed) {
                break;
            }
            let Ok(stream) = stream else {
                continue;
            };
            let st2 = st.clone();
            thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                handle(stream, st2);
            });
        }
    });

    state
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: HashMap<String, String>,
}

fn parse_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut start = String::new();
    reader.read_line(&mut start).ok()?;
    let parts: Vec<&str> = start.trim_end().split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts[0].to_string();
    let (raw_path, raw_query) = match parts[1].split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (parts[1].to_string(), String::new()),
    };

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    let mut body_raw = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body_raw).ok()?;
    }

    Some(Request {
        method,
        path: raw_path,
        query: parse_form(&raw_query),
        body: parse_form(std::str::from_utf8(&body_raw).unwrap_or("")),
    })
}

fn parse_form(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(url_decode(k), url_decode(v));
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(mut stream: TcpStream, status: u16, content_type: &str, body: &str) {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn redirect(stream: TcpStream, location: &str) {
    let mut s = stream;
    let header = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = s.write_all(header.as_bytes());
}

fn handle(stream: TcpStream, st: Arc<ServerState>) {
    let req = match parse_request(&stream) {
        Some(r) => r,
        None => {
            respond(stream, 400, "text/plain", "bad request");
            return;
        }
    };

    let setup_complete = {
        let c = st.cfg.read().unwrap();
        c.setup_complete && c.totp_secret_b32.is_some()
    };

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => {
            if !setup_complete {
                render_setup(stream, &st, None);
            } else {
                render_panel(stream, &st, req.query.get("msg").map(|s| s.as_str()));
            }
        }
        ("POST", "/setup/verify") => {
            handle_setup_verify(stream, &st, &req.body);
        }
        ("POST", "/save") => {
            handle_save(stream, &st, &req.body);
        }
        ("POST", "/add") => {
            handle_add(stream, &st, &req.body);
        }
        ("POST", "/remove") => {
            handle_remove(stream, &st, &req.body);
        }
        ("POST", "/quit") => {
            handle_quit(stream, &st, &req.body);
        }
        ("POST", "/update") => {
            handle_update(stream, &st, &req.body);
        }
        _ => {
            respond(stream, 404, "text/plain", "not found");
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title>
<style>
  :root {{ color-scheme: dark; }}
  * {{ box-sizing: border-box; }}
  html, body {{ height: 100%; margin: 0; }}
  body {{
    font: 13px/1.45 ui-sans-serif, -apple-system, "Segoe UI Variable", "Segoe UI", system-ui, sans-serif;
    -webkit-font-smoothing: antialiased;
    -moz-osx-font-smoothing: grayscale;
    font-variant-numeric: tabular-nums;
    background: #0a0a0c;
    color: #e4e5e9;
    min-height: 100vh;
  }}

  .wrap {{
    max-width: 820px;
    margin: 0 auto;
    padding: 24px 20px 140px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }}

  h1 {{ margin: 0; font: 600 17px/1.2 inherit; letter-spacing: -0.02em; color: #f4f5f7; }}
  h2 {{ margin: 0; font: 600 11px/1 inherit; letter-spacing: 0.08em; color: #7a7e87; text-transform: uppercase; }}
  p  {{ margin: 4px 0 0; color: #8a8e97; font-size: 12.5px; }}

  /* Header */
  .hero {{
    display: grid;
    grid-template-columns: 1fr auto;
    align-items: center;
    gap: 16px;
    padding: 18px 20px;
    background: linear-gradient(180deg, #15161b 0%, #101116 100%);
    border-radius: 14px;
    box-shadow:
      0 0 0 1px rgba(255,255,255,0.05),
      0 1px 2px rgba(0,0,0,0.4),
      0 12px 32px rgba(0,0,0,0.35);
  }}
  .hero .info .sub {{ color: #9095a0; font-size: 12px; margin-top: 3px; display: flex; gap: 8px; align-items: center; }}
  .hero .info .sub b {{ color: #e4e5e9; font-weight: 600; }}
  .hero .ver {{ font: 11px ui-monospace, "SF Mono", Consolas, monospace; color: #595d65; margin-left: 6px; }}

  .status {{
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 3px 9px;
    border-radius: 999px;
    font: 600 10.5px/1 inherit;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }}
  .status.on  {{ background: rgba(239, 68, 68, 0.14); color: #fca5a5; box-shadow: inset 0 0 0 1px rgba(239,68,68,0.25); }}
  .status.off {{ background: rgba(255,255,255,0.05); color: #7a7e87; box-shadow: inset 0 0 0 1px rgba(255,255,255,0.08); }}
  .status .dot {{ width: 6px; height: 6px; border-radius: 50%; background: currentColor; }}

  /* Toggle switches */
  .switch {{ position: relative; width: 40px; height: 24px; flex-shrink: 0; cursor: pointer; display: block; }}
  .switch::before {{ content: ""; position: absolute; inset: -8px; }}
  .switch input {{ position: absolute; opacity: 0; width: 0; height: 0; }}
  .switch .track {{
    position: absolute; inset: 0;
    background: #25272d;
    border-radius: 999px;
    box-shadow: inset 0 0 0 1px rgba(255,255,255,0.06);
    transition: background 180ms cubic-bezier(0.2, 0, 0, 1);
  }}
  .switch .thumb {{
    position: absolute; top: 2px; left: 2px;
    width: 20px; height: 20px;
    background: #f4f5f7;
    border-radius: 50%;
    box-shadow: 0 1px 2px rgba(0,0,0,0.4), 0 0 0 0.5px rgba(0,0,0,0.3);
    transition: translate 180ms cubic-bezier(0.2, 0, 0, 1);
  }}
  .switch input:checked + .track {{ background: #ef4444; box-shadow: inset 0 0 0 1px rgba(255,255,255,0.1); }}
  .switch input:checked + .track .thumb {{ translate: 16px 0; }}

  .switch.sm {{ width: 30px; height: 18px; }}
  .switch.sm .thumb {{ width: 14px; height: 14px; }}
  .switch.sm input:checked + .track .thumb {{ translate: 12px 0; }}

  /* Banners */
  .banner {{
    padding: 11px 14px;
    border-radius: 10px;
    font-size: 12.5px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }}
  .banner.ok  {{ background: rgba(74,222,128,0.08); color: #8fd79e; box-shadow: inset 0 0 0 1px rgba(74,222,128,0.15); }}
  .banner.err {{ background: rgba(239,68,68,0.1); color: #f4a5a5; box-shadow: inset 0 0 0 1px rgba(239,68,68,0.2); }}
  .banner.info {{ background: rgba(99,179,237,0.08); color: #a5c9f5; box-shadow: inset 0 0 0 1px rgba(99,179,237,0.2); }}

  /* Casino grid */
  .casinos {{
    display: grid;
    grid-template-columns: 1fr;
    gap: 8px;
  }}
  @media (min-width: 680px) {{
    .casinos {{ grid-template-columns: 1fr 1fr; align-items: start; }}
    .casinos > details[open] {{ grid-column: span 2; }}
  }}

  details.card {{
    background: #121317;
    border-radius: 11px;
    overflow: hidden;
    box-shadow:
      0 0 0 1px rgba(255,255,255,0.04),
      0 1px 2px rgba(0,0,0,0.25);
    transition: box-shadow 180ms cubic-bezier(0.2, 0, 0, 1);
  }}
  details.card[open] {{
    box-shadow:
      0 0 0 1px rgba(255,255,255,0.08),
      0 8px 24px rgba(0,0,0,0.3);
  }}
  details.card > summary {{
    list-style: none;
    display: grid;
    grid-template-columns: 1fr auto 14px;
    align-items: center;
    gap: 10px;
    padding: 10px 14px;
    cursor: pointer;
    user-select: none;
    transition: background 120ms;
  }}
  details.card > summary::-webkit-details-marker {{ display: none; }}
  details.card > summary:hover {{ background: rgba(255,255,255,0.02); }}
  details.card .title {{ min-width: 0; display: flex; align-items: baseline; gap: 8px; }}
  details.card .cname {{ font-weight: 600; font-size: 13px; color: #e4e5e9; }}
  details.card .domain {{ font: 11px ui-monospace, "SF Mono", Consolas, monospace; color: #595d65; }}
  details.card .chev {{ color: #595d65; font-size: 10px; transition: transform 180ms; text-align: right; }}
  details.card[open] .chev {{ transform: rotate(90deg); }}

  /* State pill — tells you exactly what's blocked */
  .pill {{
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 3px 8px;
    border-radius: 999px;
    font: 600 10.5px/1 inherit;
    letter-spacing: 0.02em;
    white-space: nowrap;
  }}
  .pill .pdot {{ width: 5px; height: 5px; border-radius: 50%; background: currentColor; }}
  .pill.none {{ background: rgba(255,255,255,0.04); color: #6e7280; box-shadow: inset 0 0 0 1px rgba(255,255,255,0.05); }}
  .pill.some {{ background: rgba(245,158,11,0.1); color: #fbbf24; box-shadow: inset 0 0 0 1px rgba(245,158,11,0.2); }}
  .pill.all  {{ background: rgba(239,68,68,0.12); color: #fca5a5; box-shadow: inset 0 0 0 1px rgba(239,68,68,0.25); }}

  /* Expanded card body */
  .card-body {{
    border-top: 1px solid rgba(255,255,255,0.05);
    background: #0f1014;
  }}
  .blockall {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 12px 14px;
    border-bottom: 1px solid rgba(255,255,255,0.04);
  }}
  .blockall .lbl b {{ display: block; font-weight: 600; color: #e4e5e9; font-size: 12.5px; }}
  .blockall .lbl span {{ color: #7a7e87; font-size: 11.5px; display: block; margin-top: 1px; }}

  .games {{ display: flex; flex-direction: column; }}
  .game {{
    display: grid;
    grid-template-columns: 18px 1fr auto auto;
    align-items: center;
    gap: 10px;
    padding: 8px 14px;
    border-bottom: 1px solid rgba(255,255,255,0.03);
  }}
  .game:last-child {{ border-bottom: none; }}
  .game label {{ display: contents; cursor: pointer; }}
  .game input[type=checkbox] {{ width: 16px; height: 16px; accent-color: #ef4444; margin: 0; cursor: pointer; }}
  .game .name  {{ font-weight: 500; color: #e4e5e9; font-size: 12.5px; }}
  .game .slug  {{ font: 11px ui-monospace, "SF Mono", Consolas, monospace; color: #595d65; }}
  .game .rm {{
    appearance: none; border: 0; background: transparent;
    color: #4a4e57; cursor: pointer;
    font: 500 16px/1 inherit; padding: 4px 8px; border-radius: 6px;
    transition: color 120ms, background 120ms;
  }}
  .game .rm:hover {{ color: #f4a5a5; background: rgba(239,68,68,0.08); }}
  .game.muted {{ opacity: 0.35; }}
  .game.muted input[type=checkbox] {{ pointer-events: none; }}
  .empty {{ padding: 18px 14px; text-align: center; color: #595d65; font-size: 12px; }}

  /* Add-game form */
  .addbox {{
    background: #121317;
    border-radius: 11px;
    padding: 14px;
    box-shadow: 0 0 0 1px rgba(255,255,255,0.04), 0 1px 2px rgba(0,0,0,0.25);
    display: flex;
    flex-direction: column;
    gap: 10px;
  }}
  .addbox .row {{
    display: grid;
    grid-template-columns: 180px 1fr 1.3fr auto;
    gap: 8px;
  }}
  @media (max-width: 520px) {{
    .addbox .row {{ grid-template-columns: 1fr 1fr; }}
  }}

  .input, .select {{
    padding: 9px 12px;
    background: #0a0b0e;
    border: 0;
    border-radius: 8px;
    color: #ededef;
    font: 13px inherit;
    outline: none;
    min-width: 0;
    box-shadow: inset 0 0 0 1px rgba(255,255,255,0.06), inset 0 1px 2px rgba(0,0,0,0.4);
    transition: box-shadow 160ms;
  }}
  .input::placeholder {{ color: #4a4e57; }}
  .input:focus, .select:focus {{ box-shadow: inset 0 0 0 1px rgba(239,68,68,0.7), 0 0 0 3px rgba(239,68,68,0.14); }}
  .select {{
    appearance: none;
    cursor: pointer;
    padding-right: 30px;
    background-image: linear-gradient(45deg, transparent 50%, #8a8e97 50%), linear-gradient(135deg, #8a8e97 50%, transparent 50%);
    background-position: calc(100% - 14px) 50%, calc(100% - 10px) 50%;
    background-size: 4px 4px, 4px 4px;
    background-repeat: no-repeat;
    background-color: #0a0b0e;
  }}

  /* Buttons */
  button, .btn {{
    appearance: none; border: 0; cursor: pointer;
    padding: 9px 16px;
    background: #ef4444;
    color: #fff;
    border-radius: 8px;
    font: 600 12.5px/1 inherit;
    white-space: nowrap;
    box-shadow: 0 0 0 1px rgba(255,255,255,0.08), 0 1px 2px rgba(0,0,0,0.3), inset 0 1px 0 rgba(255,255,255,0.12);
    transition: scale 140ms, background 140ms, color 140ms, box-shadow 140ms;
  }}
  button:hover {{ background: #f05a5a; }}
  button:active {{ scale: 0.96; }}
  button:focus-visible {{ outline: 2px solid rgba(239,68,68,0.5); outline-offset: 2px; }}
  button.ghost {{
    background: transparent; color: #9095a0;
    box-shadow: inset 0 0 0 1px rgba(255,255,255,0.08);
  }}
  button.ghost:hover {{ color: #e4e5e9; background: rgba(255,255,255,0.03); box-shadow: inset 0 0 0 1px rgba(255,255,255,0.14); }}
  button.warn {{
    background: transparent; color: #fbbf24;
    box-shadow: inset 0 0 0 1px rgba(245,158,11,0.3);
  }}
  button.warn:hover {{ background: rgba(245,158,11,0.1); color: #fcd34d; }}

  /* Sticky action bar */
  .dock {{
    position: fixed;
    left: 0; right: 0; bottom: 0;
    padding: 12px 20px calc(12px + env(safe-area-inset-bottom));
    background: linear-gradient(180deg, rgba(10,10,12,0) 0%, rgba(10,10,12,0.92) 35%);
    backdrop-filter: blur(12px);
    -webkit-backdrop-filter: blur(12px);
    z-index: 10;
    pointer-events: none;
  }}
  .dock-inner {{
    max-width: 820px;
    margin: 0 auto;
    background: #15161b;
    border-radius: 14px;
    padding: 10px 12px;
    display: grid;
    grid-template-columns: 1fr auto auto auto;
    gap: 8px;
    align-items: center;
    box-shadow:
      0 0 0 1px rgba(255,255,255,0.07),
      0 12px 28px rgba(0,0,0,0.5);
    pointer-events: auto;
  }}
  .dock .hint {{ color: #6e7280; font-size: 11px; padding-left: 6px; line-height: 1.3; }}

  .code {{
    padding: 9px 12px;
    background: #0a0b0e;
    border: 0;
    border-radius: 8px;
    color: #ededef;
    font: 500 14px/1 ui-monospace, "SF Mono", Consolas, monospace;
    letter-spacing: 0.28em;
    text-align: center;
    outline: none;
    width: 110px;
    box-shadow: inset 0 0 0 1px rgba(255,255,255,0.06), inset 0 1px 2px rgba(0,0,0,0.4);
    transition: box-shadow 160ms;
  }}
  .code::placeholder {{ color: #4a4e57; letter-spacing: 0.2em; font-weight: 400; }}
  .code:focus {{ box-shadow: inset 0 0 0 1px rgba(239,68,68,0.7), 0 0 0 3px rgba(239,68,68,0.14); }}

  /* Setup page */
  .setup {{
    max-width: 420px;
    margin: 40px auto;
    padding: 24px;
    display: flex;
    flex-direction: column;
    gap: 16px;
    background: #121317;
    border-radius: 16px;
    box-shadow: 0 0 0 1px rgba(255,255,255,0.05), 0 16px 40px rgba(0,0,0,0.5);
  }}
  .qr {{ background: #fff; padding: 14px; border-radius: 12px; align-self: center; box-shadow: 0 0 0 1px rgba(0,0,0,0.1), 0 8px 24px rgba(0,0,0,0.3); }}
  .qr svg {{ display: block; width: 180px; height: 180px; }}
  code.secret {{
    display: inline-block;
    font: 11px ui-monospace, "SF Mono", Consolas, monospace;
    color: #b5b8bf;
    background: rgba(255,255,255,0.04);
    padding: 3px 7px;
    border-radius: 5px;
    word-break: break-all;
    user-select: all;
    box-shadow: inset 0 0 0 1px rgba(255,255,255,0.05);
  }}
  .setup-actions {{ display: grid; grid-template-columns: 1fr auto; gap: 8px; }}

  @media (prefers-reduced-motion: reduce) {{
    *, *::before, *::after {{ transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; }}
  }}
</style>
</head><body>
{body}
</body></html>"#,
        title = html_escape(title),
        body = body
    )
}

fn render_setup(stream: TcpStream, st: &ServerState, error: Option<&str>) {
    let mut pending = st.pending_secret.lock().unwrap();
    if pending.is_none() {
        *pending = Some(totp::generate_secret_b32());
    }
    let secret = pending.clone().unwrap();
    drop(pending);

    let uri = totp::otpauth_url(&secret, "BetWall", "BetWall");
    let svg_qr = QrCode::new(uri.as_bytes())
        .expect("qr")
        .render::<svg::Color>()
        .min_dimensions(180, 180)
        .dark_color(svg::Color("#0f1013"))
        .light_color(svg::Color("#ffffff"))
        .build();

    let err_html = error
        .map(|e| format!(r#"<div class="banner err">{}</div>"#, html_escape(e)))
        .unwrap_or_default();

    let body = format!(
        r#"<div class="setup">
  <div>
    <h1>Set up 2FA</h1>
    <p>Scan the QR with your authenticator app, then enter the current code to finish setup. No code needed for this step — it's just provisioning.</p>
  </div>
  <div class="qr">{svg}</div>
  <p>Or type the secret manually: <code class="secret">{secret}</code></p>
  {err}
  <form method="POST" action="/setup/verify" class="setup-actions">
    <input class="code" type="text" name="code" inputmode="numeric" pattern="[0-9]{{6}}" maxlength="6" placeholder="000000" autofocus autocomplete="one-time-code" required>
    <button type="submit">Confirm</button>
  </form>
</div>"#,
        svg = svg_qr,
        secret = html_escape(&secret),
        err = err_html
    );
    respond(stream, 200, "text/html", &page("BetWall — Setup", &body));
}

fn handle_setup_verify(stream: TcpStream, st: &ServerState, body: &HashMap<String, String>) {
    let code = body.get("code").map(|s| s.as_str()).unwrap_or("");
    let pending = st.pending_secret.lock().unwrap().clone();
    let Some(secret) = pending else {
        render_setup(stream, st, Some("Session expired, try again."));
        return;
    };
    if !totp::verify(&secret, code) {
        render_setup(stream, st, Some("Code didn't match. Try again."));
        return;
    }
    {
        let mut cfg = st.cfg.write().unwrap();
        cfg.totp_secret_b32 = Some(secret);
        cfg.setup_complete = true;
        config::save(&cfg);
    }
    *st.pending_secret.lock().unwrap() = None;
    redirect(stream, "/?msg=setup_ok");
}

fn verify_code(st: &ServerState, body: &HashMap<String, String>) -> bool {
    let Some(secret) = st.cfg.read().unwrap().totp_secret_b32.clone() else {
        return false;
    };
    let code = body.get("code").map(|s| s.as_str()).unwrap_or("");
    totp::verify(&secret, code)
}

fn apply_checkbox_state(cfg: &mut config::Config, body: &HashMap<String, String>) {
    cfg.enabled = body.contains_key("enabled");
    for c in cfg.casinos.iter_mut() {
        let key = format!("c_{}_all", c.id);
        c.blocked_all = body.contains_key(&key);
    }
    for g in cfg.games.iter_mut() {
        let key = format!("g_{}", g.id);
        g.blocked = body.contains_key(&key);
    }
}

fn save_weakens(old: &config::Config, new: &config::Config) -> bool {
    if old.enabled && !new.enabled {
        return true;
    }
    let old_block_all: HashMap<&str, bool> = old
        .casinos
        .iter()
        .map(|c| (c.id.as_str(), c.blocked_all))
        .collect();
    for c in &new.casinos {
        if old_block_all.get(c.id.as_str()).copied().unwrap_or(false) && !c.blocked_all {
            return true;
        }
    }
    let old_blocked: HashMap<&str, bool> = old
        .games
        .iter()
        .map(|g| (g.id.as_str(), g.blocked))
        .collect();
    for g in &new.games {
        if old_blocked.get(g.id.as_str()).copied().unwrap_or(false) && !g.blocked {
            return true;
        }
    }
    false
}

fn handle_save(stream: TcpStream, st: &ServerState, body: &HashMap<String, String>) {
    let weakens = {
        let old = st.cfg.read().unwrap().clone();
        let mut proposed = old.clone();
        apply_checkbox_state(&mut proposed, body);
        save_weakens(&old, &proposed)
    };
    if weakens && !verify_code(st, body) {
        redirect(stream, "/?msg=bad_code");
        return;
    }
    {
        let mut cfg = st.cfg.write().unwrap();
        apply_checkbox_state(&mut cfg, body);
        config::save(&cfg);
    }
    redirect(stream, "/?msg=ok");
}

fn handle_add(stream: TcpStream, st: &ServerState, body: &HashMap<String, String>) {
    let label = body.get("label").map(|s| s.trim()).unwrap_or("").to_string();
    let raw_pattern = body.get("url_pattern").map(|s| s.trim()).unwrap_or("");
    let casino_id = body
        .get("casino")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if label.is_empty() || raw_pattern.is_empty() {
        redirect(stream, "/?msg=bad_input");
        return;
    }
    {
        let mut cfg = st.cfg.write().unwrap();

        let final_pattern = if casino_id.is_empty() {
            config::normalize_pattern(raw_pattern)
        } else {
            let domain = cfg
                .casinos
                .iter()
                .find(|c| c.id == casino_id)
                .map(|c| c.domain.to_lowercase())
                .unwrap_or_default();
            let normalized = config::normalize_pattern(raw_pattern);
            let rest = normalized
                .strip_prefix(&domain)
                .unwrap_or(&normalized)
                .trim_start_matches('/');
            if domain.is_empty() {
                normalized
            } else if rest.is_empty() {
                domain
            } else {
                format!("{domain}/{rest}")
            }
        };

        if final_pattern.is_empty() {
            redirect(stream, "/?msg=bad_input");
            return;
        }

        let base = {
            let s = config::slugify(&label);
            if s.is_empty() {
                format!("game-{}", cfg.games.len() + 1)
            } else if casino_id.is_empty() {
                format!("custom-{s}")
            } else {
                format!("{casino_id}-{s}")
            }
        };
        let mut id = base.clone();
        let mut n = 2;
        while cfg.games.iter().any(|g| g.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        cfg.games.push(config::GameEntry {
            id,
            casino: casino_id,
            label,
            url_pattern: final_pattern,
            blocked: true,
        });
        config::save(&cfg);
    }
    redirect(stream, "/?msg=added");
}

fn handle_remove(stream: TcpStream, st: &ServerState, body: &HashMap<String, String>) {
    if !verify_code(st, body) {
        redirect(stream, "/?msg=bad_code");
        return;
    }
    let remove_id = body.get("remove_id").cloned().unwrap_or_default();
    if remove_id.is_empty() {
        redirect(stream, "/?msg=ok");
        return;
    }
    {
        let mut cfg = st.cfg.write().unwrap();
        cfg.games.retain(|g| g.id != remove_id);
        config::save(&cfg);
    }
    redirect(stream, "/?msg=removed");
}

fn handle_update(stream: TcpStream, st: &ServerState, body: &HashMap<String, String>) {
    if !verify_code(st, body) {
        redirect(stream, "/?msg=bad_code");
        return;
    }
    let has_update = st
        .update_status
        .read()
        .unwrap()
        .latest_version
        .is_some();
    if !has_update {
        redirect(stream, "/?msg=no_update");
        return;
    }
    updater::apply(st.update_status.clone(), st.stop.clone());
    let body_html = r#"<div class="setup"><div><h1>Updating</h1><p>Downloading the new version. BetWall will restart in a moment.</p></div></div>"#;
    respond(stream, 200, "text/html", &page("Updating", body_html));
}

fn handle_quit(stream: TcpStream, st: &ServerState, body: &HashMap<String, String>) {
    if !verify_code(st, body) {
        redirect(stream, "/?msg=bad_code");
        return;
    }
    let body_html = r#"<div class="setup"><div><h1>Shutting down</h1><p>Tray app is quitting. You can close this tab.</p></div></div>"#;
    respond(stream, 200, "text/html", &page("Quitting", body_html));
    st.stop.store(true, Ordering::Relaxed);
    thread::spawn(|| {
        thread::sleep(Duration::from_millis(200));
        std::process::exit(0);
    });
}

fn render_game_row(g: &config::GameEntry, muted: bool) -> String {
    let checked = if g.blocked { "checked" } else { "" };
    let row_class = if muted { "game muted" } else { "game" };
    let slug = g.url_pattern.rsplit('/').next().unwrap_or(&g.url_pattern);
    format!(
        r#"<div class="{row_class}"><label><input type="checkbox" name="g_{id}" {checked}><span class="name">{label}</span><span class="slug">{slug}</span></label><button type="submit" name="remove_id" value="{id}" formaction="/remove" class="rm" title="Remove {label}" aria-label="Remove {label}">×</button></div>"#,
        id = html_escape(&g.id),
        checked = checked,
        label = html_escape(&g.label),
        slug = html_escape(slug),
    )
}

fn render_panel(stream: TcpStream, st: &ServerState, msg: Option<&str>) {
    let cfg = st.cfg.read().unwrap();
    let banner = match msg {
        Some("setup_ok") => Some(("ok", "2FA active. Pausing and quitting require a code.")),
        Some("ok") => Some(("ok", "Saved.")),
        Some("added") => Some(("ok", "Game added.")),
        Some("removed") => Some(("ok", "Game removed.")),
        Some("bad_input") => Some(("err", "Name and URL pattern are required.")),
        Some("bad_code") => Some(("err", "Wrong 2FA code — that action needs one.")),
        Some("no_update") => Some(("info", "No update available.")),
        Some("update_err") => Some(("err", "Update failed. Check network and try again.")),
        _ => None,
    };
    let banner_html = banner
        .map(|(c, t)| format!(r#"<div class="banner {c}">{}</div>"#, html_escape(t)))
        .unwrap_or_default();

    let total_blocked = active_pattern_count(&cfg);
    let (status_class, status_text) = if cfg.enabled {
        ("on", "Blocking")
    } else {
        ("off", "Paused")
    };
    let state_sub = if cfg.enabled {
        format!("<b>{total_blocked}</b> pattern{s} active · all browsers monitored",
            s = if total_blocked == 1 { "" } else { "s" })
    } else {
        "Pausing lets everything through. Flip the switch to resume.".to_string()
    };
    let enabled_attr = if cfg.enabled { "checked" } else { "" };

    let update_banner = {
        let s = st.update_status.read().unwrap();
        if let Some(v) = &s.latest_version {
            format!(
                r#"<div class="banner info"><span>Update available — <b>v{new}</b> (you have v{cur}).</span><button type="submit" formaction="/update" class="warn">Install update</button></div>"#,
                new = html_escape(v),
                cur = html_escape(updater::current_version()),
            )
        } else {
            String::new()
        }
    };

    let mut sections = String::new();
    for c in &cfg.casinos {
        let casino_games: Vec<&config::GameEntry> =
            cfg.games.iter().filter(|g| g.casino == c.id).collect();
        let total = casino_games.len();
        let blocked_count = casino_games.iter().filter(|g| g.blocked).count();

        let (pill_class, pill_text) = if c.blocked_all {
            ("all", "All blocked".to_string())
        } else if blocked_count == 0 {
            ("none", if total == 0 { "no games".into() } else { "None".into() })
        } else if blocked_count == total && total > 0 {
            ("all", format!("{total} / {total}"))
        } else {
            ("some", format!("{blocked_count} / {total}"))
        };

        let checked_all = if c.blocked_all { "checked" } else { "" };
        let open_attr = if c.blocked_all || blocked_count > 0 { "open" } else { "" };

        let mut rows = String::new();
        for g in &casino_games {
            rows.push_str(&render_game_row(g, c.blocked_all));
        }
        if casino_games.is_empty() {
            rows.push_str(
                r#"<div class="empty">No games yet. Add one below.</div>"#,
            );
        }

        sections.push_str(&format!(
            r#"<details class="card" {open}>
  <summary>
    <span class="title"><span class="cname">{label}</span><span class="domain">{domain}</span></span>
    <span class="pill {pclass}"><span class="pdot"></span>{ptext}</span>
    <span class="chev">▸</span>
  </summary>
  <div class="card-body">
    <div class="blockall">
      <span class="lbl"><b>Block entire casino</b><span>All traffic on {domain}, regardless of game.</span></span>
      <label class="switch sm" title="Block entire casino">
        <input type="checkbox" name="c_{id}_all" {checked_all}>
        <span class="track"><span class="thumb"></span></span>
      </label>
    </div>
    <div class="games">{rows}</div>
  </div>
</details>"#,
            open = open_attr,
            id = html_escape(&c.id),
            label = html_escape(&c.label),
            domain = html_escape(&c.domain),
            pclass = pill_class,
            ptext = html_escape(&pill_text),
            checked_all = checked_all,
            rows = rows,
        ));
    }

    let custom_games: Vec<&config::GameEntry> =
        cfg.games.iter().filter(|g| g.casino.is_empty()).collect();
    let custom_section = if custom_games.is_empty() {
        String::new()
    } else {
        let blocked_c = custom_games.iter().filter(|g| g.blocked).count();
        let total_c = custom_games.len();
        let (pclass, ptext) = if blocked_c == 0 {
            ("none", "None".to_string())
        } else if blocked_c == total_c {
            ("all", format!("{total_c} / {total_c}"))
        } else {
            ("some", format!("{blocked_c} / {total_c}"))
        };
        let mut rows = String::new();
        for g in &custom_games {
            rows.push_str(&render_game_row(g, false));
        }
        format!(
            r#"<details class="card" open>
  <summary>
    <span class="title"><span class="cname">Custom</span><span class="domain">user-added</span></span>
    <span class="pill {pclass}"><span class="pdot"></span>{ptext}</span>
    <span class="chev">▸</span>
  </summary>
  <div class="card-body">
    <div class="games">{rows}</div>
  </div>
</details>"#,
            pclass = pclass,
            ptext = html_escape(&ptext),
            rows = rows,
        )
    };

    let mut casino_options = String::from(r#"<option value="">Custom (full URL)</option>"#);
    for c in &cfg.casinos {
        casino_options.push_str(&format!(
            r#"<option value="{id}">{label}</option>"#,
            id = html_escape(&c.id),
            label = html_escape(&c.label),
        ));
    }

    let body = format!(
        r#"<form method="POST" action="/save">
  <div class="wrap">
    <div class="hero">
      <div class="info">
        <h1>BetWall <span class="ver">v{ver}</span></h1>
        <div class="sub">
          <span class="status {sclass}"><span class="dot"></span>{stext}</span>
          <span>{sub}</span>
        </div>
      </div>
      <label class="switch" title="Master toggle (turning off requires 2FA)">
        <input type="checkbox" name="enabled" {enabled}>
        <span class="track"><span class="thumb"></span></span>
      </label>
    </div>

    {update_banner}
    {banner}

    <h2>Casinos</h2>
    <div class="casinos">{sections}{custom_section}</div>

    <h2>Add a game</h2>
    <div class="addbox">
      <div class="row">
        <select name="casino" class="select">{casino_options}</select>
        <input class="input" type="text" name="label" placeholder="Game name (e.g. Dice)" autocomplete="off">
        <input class="input" type="text" name="url_pattern" placeholder="path or full URL" autocomplete="off">
        <button type="submit" class="ghost" formaction="/add">Add</button>
      </div>
    </div>
  </div>

  <div class="dock">
    <div class="dock-inner">
      <span class="hint">2FA only needed to pause, unblock, quit, or install updates.</span>
      <input class="code" type="text" name="code" inputmode="numeric" pattern="[0-9]{{6}}" maxlength="6" placeholder="2FA" autocomplete="one-time-code">
      <button type="submit">Save</button>
      <button type="submit" class="ghost" formaction="/quit">Quit</button>
    </div>
  </div>
</form>"#,
        ver = html_escape(updater::current_version()),
        sclass = status_class,
        stext = status_text,
        sub = state_sub,
        enabled = enabled_attr,
        update_banner = update_banner,
        banner = banner_html,
        sections = sections,
        custom_section = custom_section,
        casino_options = casino_options,
    );
    respond(stream, 200, "text/html", &page("BetWall", &body));
}

fn active_pattern_count(cfg: &config::Config) -> usize {
    let blocked_all: std::collections::HashSet<&str> = cfg
        .casinos
        .iter()
        .filter(|c| c.blocked_all)
        .map(|c| c.id.as_str())
        .collect();
    let casino_all = blocked_all.len();
    let individual = cfg
        .games
        .iter()
        .filter(|g| g.blocked && !blocked_all.contains(g.casino.as_str()))
        .count();
    casino_all + individual
}
