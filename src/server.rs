use crate::config::{self, SharedConfig};
use crate::totp;
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
}

pub fn start(cfg: SharedConfig, stop: Arc<AtomicBool>) -> Arc<ServerState> {
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
    font: 13.5px/1.5 ui-sans-serif, -apple-system, "Segoe UI Variable", "Segoe UI", system-ui, sans-serif;
    -webkit-font-smoothing: antialiased;
    -moz-osx-font-smoothing: grayscale;
    font-variant-numeric: tabular-nums;
    background: #09090b;
    color: #d8dadf;
    padding: 36px 20px;
    display: grid;
    place-items: start center;
  }}

  .panel {{
    width: 100%;
    max-width: 460px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }}
  .panel > * {{
    animation: fadeUp 340ms cubic-bezier(0.2, 0, 0, 1) both;
  }}
  .panel > *:nth-child(1) {{ animation-delay: 0ms; }}
  .panel > *:nth-child(2) {{ animation-delay: 70ms; }}
  .panel > *:nth-child(3) {{ animation-delay: 140ms; }}
  .panel > *:nth-child(4) {{ animation-delay: 210ms; }}
  .panel > *:nth-child(5) {{ animation-delay: 280ms; }}

  @keyframes fadeUp {{
    from {{ opacity: 0; transform: translateY(4px); }}
    to   {{ opacity: 1; transform: translateY(0); }}
  }}

  h1 {{
    margin: 0;
    font-size: 15px;
    font-weight: 600;
    letter-spacing: -0.015em;
    color: #f2f3f5;
    text-wrap: balance;
  }}
  p {{
    margin: 4px 0 0;
    color: #8a8e97;
    font-size: 12.5px;
    text-wrap: pretty;
  }}

  .head {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }}
  .head .info {{ min-width: 0; }}
  .head .info .sub {{ font-size: 12px; color: #8a8e97; margin-top: 2px; }}
  .head .info .sub b {{ color: #d8dadf; font-weight: 500; }}

  .switch {{
    position: relative;
    width: 36px;
    height: 22px;
    flex-shrink: 0;
    cursor: pointer;
    display: block;
  }}
  .switch::before {{
    content: "";
    position: absolute;
    inset: -9px;
  }}
  .switch input {{ position: absolute; opacity: 0; width: 0; height: 0; }}
  .switch .track {{
    position: absolute;
    inset: 0;
    background: #2a2d34;
    border-radius: 999px;
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.05);
    transition-property: background-color, box-shadow;
    transition-duration: 180ms;
    transition-timing-function: cubic-bezier(0.2, 0, 0, 1);
  }}
  .switch .thumb {{
    position: absolute;
    top: 2px; left: 2px;
    width: 18px; height: 18px;
    background: #f4f5f7;
    border-radius: 50%;
    box-shadow:
      0 1px 2px rgba(0, 0, 0, 0.35),
      0 0 0 0.5px rgba(0, 0, 0, 0.25);
    transition-property: translate, background-color;
    transition-duration: 180ms;
    transition-timing-function: cubic-bezier(0.2, 0, 0, 1);
  }}
  .switch input:checked + .track {{
    background: #ef4444;
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.08);
  }}
  .switch input:checked + .track .thumb {{
    translate: 14px 0;
    background: #fff;
  }}

  .list {{
    background: #131418;
    border-radius: 12px;
    overflow: hidden;
    box-shadow:
      0 0 0 1px rgba(255, 255, 255, 0.04),
      0 1px 2px rgba(0, 0, 0, 0.3),
      0 8px 24px rgba(0, 0, 0, 0.2);
  }}
  .list .row {{
    display: flex;
    align-items: stretch;
    border-bottom: 1px solid rgba(255, 255, 255, 0.04);
  }}
  .list .row:last-child {{ border-bottom: none; }}
  .list .toggle {{
    flex: 1;
    min-width: 0;
    display: grid;
    grid-template-columns: 16px 1fr auto;
    gap: 12px;
    align-items: center;
    padding: 11px 14px;
    cursor: pointer;
    transition-property: background-color;
    transition-duration: 120ms;
    transition-timing-function: cubic-bezier(0.2, 0, 0, 1);
  }}
  .list .toggle:hover {{ background: rgba(255, 255, 255, 0.025); }}
  .list .remove {{
    appearance: none;
    border: 0;
    background: transparent;
    color: #5f636d;
    cursor: pointer;
    font: 500 18px/1 ui-sans-serif, system-ui, sans-serif;
    padding: 0 14px;
    border-radius: 0;
    box-shadow: none;
    transition-property: color, background-color;
    transition-duration: 140ms;
    transition-timing-function: cubic-bezier(0.2, 0, 0, 1);
  }}
  .list .remove:hover {{ color: #f4a5a5; background: rgba(239, 68, 68, 0.08); }}
  .list .empty {{
    padding: 22px 14px;
    text-align: center;
    color: #5f636d;
    font-size: 12.5px;
  }}
  .list input[type=checkbox] {{
    width: 16px; height: 16px;
    margin: 0;
    accent-color: #ef4444;
    cursor: pointer;
  }}
  .list .name {{
    color: #d8dadf;
    font-weight: 500;
    font-size: 13px;
    text-wrap: pretty;
  }}
  .list .slug {{
    font-family: ui-monospace, "SF Mono", Consolas, monospace;
    font-size: 11px;
    color: #5f636d;
  }}

  .actions {{
    display: grid;
    grid-template-columns: 1fr auto auto;
    gap: 8px;
  }}

  .casinos {{
    display: flex;
    flex-direction: column;
    gap: 6px;
  }}
  .casino {{
    background: #131418;
    border-radius: 10px;
    overflow: hidden;
    box-shadow:
      0 0 0 1px rgba(255, 255, 255, 0.04),
      0 1px 2px rgba(0, 0, 0, 0.25);
  }}
  .casino > summary {{
    list-style: none;
    padding: 10px 14px;
    display: grid;
    grid-template-columns: 14px 1fr auto auto auto;
    align-items: center;
    gap: 10px;
    cursor: pointer;
    user-select: none;
  }}
  .casino > summary::-webkit-details-marker {{ display: none; }}
  .casino > summary .chev {{
    color: #5f636d;
    font-size: 10px;
    transition: transform 180ms cubic-bezier(0.2, 0, 0, 1);
  }}
  .casino[open] > summary .chev {{ transform: rotate(90deg); }}
  .casino .cname {{ color: #d8dadf; font-weight: 600; font-size: 13px; }}
  .casino .domain {{
    font-family: ui-monospace, "SF Mono", Consolas, monospace;
    font-size: 11px;
    color: #5f636d;
    padding-left: 2px;
  }}
  .casino .count {{
    font-size: 11px;
    color: #8a8e97;
    padding: 2px 8px;
    background: rgba(255, 255, 255, 0.04);
    border-radius: 999px;
  }}
  .casino > summary:hover {{ background: rgba(255, 255, 255, 0.02); }}
  .casino .list {{ border-top: 1px solid rgba(255, 255, 255, 0.04); border-radius: 0; box-shadow: none; }}

  .allswitch {{
    position: relative;
    width: 30px;
    height: 18px;
    flex-shrink: 0;
    cursor: pointer;
    display: block;
  }}
  .allswitch input {{ position: absolute; opacity: 0; width: 0; height: 0; }}
  .allswitch .track {{
    position: absolute;
    inset: 0;
    background: #2a2d34;
    border-radius: 999px;
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.05);
    transition: background-color 180ms cubic-bezier(0.2, 0, 0, 1);
  }}
  .allswitch .thumb {{
    position: absolute;
    top: 2px; left: 2px;
    width: 14px; height: 14px;
    background: #f4f5f7;
    border-radius: 50%;
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.35);
    transition: translate 180ms cubic-bezier(0.2, 0, 0, 1);
  }}
  .allswitch input:checked + .track {{ background: #ef4444; }}
  .allswitch input:checked + .track .thumb {{ translate: 12px 0; background: #fff; }}

  .row.muted .toggle {{ opacity: 0.4; cursor: not-allowed; }}
  .row.muted input[type=checkbox] {{ cursor: not-allowed; }}

  .add {{
    display: grid;
    grid-template-columns: auto 1fr 1.2fr auto;
    gap: 8px;
  }}
  .select {{
    padding: 10px 12px;
    background: #0b0c0f;
    border: 0;
    border-radius: 8px;
    color: #ededef;
    font: 13px ui-sans-serif, -apple-system, "Segoe UI", system-ui, sans-serif;
    outline: none;
    cursor: pointer;
    appearance: none;
    padding-right: 28px;
    background-image: linear-gradient(45deg, transparent 50%, #8a8e97 50%), linear-gradient(135deg, #8a8e97 50%, transparent 50%);
    background-position: calc(100% - 14px) 50%, calc(100% - 10px) 50%;
    background-size: 4px 4px, 4px 4px;
    background-repeat: no-repeat;
    box-shadow:
      inset 0 0 0 1px rgba(255, 255, 255, 0.06),
      inset 0 1px 2px rgba(0, 0, 0, 0.4);
  }}
  .add input {{
    padding: 10px 12px;
    background: #0b0c0f;
    border: 0;
    border-radius: 8px;
    color: #ededef;
    font: 13px ui-sans-serif, -apple-system, "Segoe UI", system-ui, sans-serif;
    outline: none;
    min-width: 0;
    box-shadow:
      inset 0 0 0 1px rgba(255, 255, 255, 0.06),
      inset 0 1px 2px rgba(0, 0, 0, 0.4);
    transition-property: box-shadow;
    transition-duration: 160ms;
    transition-timing-function: cubic-bezier(0.2, 0, 0, 1);
  }}
  .add input::placeholder {{ color: #4a4e57; }}
  .add input:focus {{
    box-shadow:
      inset 0 0 0 1px rgba(239, 68, 68, 0.7),
      0 0 0 3px rgba(239, 68, 68, 0.14);
  }}

  .code {{
    padding: 10px 12px;
    background: #0b0c0f;
    border: 0;
    border-radius: 8px;
    color: #ededef;
    font: 500 14px/1 ui-monospace, "SF Mono", Consolas, monospace;
    letter-spacing: 0.32em;
    text-align: center;
    outline: none;
    box-shadow:
      inset 0 0 0 1px rgba(255, 255, 255, 0.06),
      inset 0 1px 2px rgba(0, 0, 0, 0.4);
    transition-property: box-shadow;
    transition-duration: 160ms;
    transition-timing-function: cubic-bezier(0.2, 0, 0, 1);
  }}
  .code::placeholder {{ color: #4a4e57; letter-spacing: 0.2em; font-weight: 400; }}
  .code:focus {{
    box-shadow:
      inset 0 0 0 1px rgba(239, 68, 68, 0.7),
      0 0 0 3px rgba(239, 68, 68, 0.14);
  }}

  button {{
    appearance: none;
    border: 0;
    padding: 10px 16px;
    background: #ef4444;
    color: #fff;
    border-radius: 8px;
    font: 600 12.5px/1 ui-sans-serif, -apple-system, "Segoe UI", system-ui, sans-serif;
    cursor: pointer;
    white-space: nowrap;
    box-shadow:
      0 0 0 1px rgba(255, 255, 255, 0.08),
      0 1px 2px rgba(0, 0, 0, 0.3),
      inset 0 1px 0 rgba(255, 255, 255, 0.12);
    transition-property: scale, background-color, box-shadow;
    transition-duration: 140ms;
    transition-timing-function: cubic-bezier(0.2, 0, 0, 1);
  }}
  button:hover {{ background: #f05a5a; }}
  button:active {{ scale: 0.96; }}
  button:focus-visible {{ outline: 2px solid rgba(239, 68, 68, 0.5); outline-offset: 2px; }}
  button.ghost {{
    background: transparent;
    color: #8a8e97;
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.08);
  }}
  button.ghost:hover {{
    color: #d8dadf;
    background: rgba(255, 255, 255, 0.03);
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.14);
  }}

  .banner {{
    padding: 10px 12px;
    border-radius: 8px;
    font-size: 12.5px;
    display: flex;
    align-items: center;
    gap: 8px;
    text-wrap: pretty;
  }}
  .banner.ok {{
    background: rgba(74, 222, 128, 0.09);
    color: #8fd79e;
    box-shadow: inset 0 0 0 1px rgba(74, 222, 128, 0.15);
  }}
  .banner.err {{
    background: rgba(239, 68, 68, 0.1);
    color: #f4a5a5;
    box-shadow: inset 0 0 0 1px rgba(239, 68, 68, 0.2);
  }}

  .qr {{
    background: #fff;
    padding: 14px;
    border-radius: 14px;
    display: grid;
    place-items: center;
    align-self: center;
    box-shadow:
      0 0 0 1px rgba(0, 0, 0, 0.1),
      0 8px 24px rgba(0, 0, 0, 0.3);
  }}
  .qr svg {{ display: block; width: 180px; height: 180px; }}

  code.secret {{
    display: inline-block;
    font: 11px ui-monospace, "SF Mono", Consolas, monospace;
    color: #b5b8bf;
    background: rgba(255, 255, 255, 0.04);
    padding: 2px 6px;
    border-radius: 5px;
    word-break: break-all;
    user-select: all;
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.05);
  }}
  .setup-hint {{ font-size: 12px; color: #8a8e97; }}

  @media (prefers-reduced-motion: reduce) {{
    *, *::before, *::after {{
      transition-duration: 0.01ms !important;
      animation-duration: 0.01ms !important;
    }}
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
        r#"<div class="panel">
  <div>
    <h1>Set up 2FA</h1>
    <p>Scan the QR with your authenticator app, then enter the current code to lock the controls.</p>
  </div>
  <div class="qr">{svg}</div>
  <p class="setup-hint">Or type the secret manually: <code class="secret">{secret}</code></p>
  {err}
  <form method="POST" action="/setup/verify" class="actions">
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

fn handle_save(stream: TcpStream, st: &ServerState, body: &HashMap<String, String>) {
    if !verify_code(st, body) {
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
    if !verify_code(st, body) {
        redirect(stream, "/?msg=bad_code");
        return;
    }
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
        apply_checkbox_state(&mut cfg, body);

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
        apply_checkbox_state(&mut cfg, body);
        cfg.games.retain(|g| g.id != remove_id);
        config::save(&cfg);
    }
    redirect(stream, "/?msg=removed");
}

fn handle_quit(stream: TcpStream, st: &ServerState, body: &HashMap<String, String>) {
    if !verify_code(st, body) {
        redirect(stream, "/?msg=bad_code");
        return;
    }
    let body_html = r#"<div class="panel"><div><h1>Shutting down</h1><p>Tray app is quitting. You can close this tab.</p></div></div>"#;
    respond(stream, 200, "text/html", &page("Quitting", body_html));
    st.stop.store(true, Ordering::Relaxed);
    thread::spawn(|| {
        thread::sleep(Duration::from_millis(200));
        std::process::exit(0);
    });
}

fn render_game_row(g: &config::GameEntry, disabled: bool) -> String {
    let checked = if g.blocked { "checked" } else { "" };
    let disabled_attr = if disabled { "disabled" } else { "" };
    let row_class = if disabled { "row muted" } else { "row" };
    let slug = g.url_pattern.rsplit('/').next().unwrap_or(&g.url_pattern);
    format!(
        r#"<div class="{row_class}"><label class="toggle"><input type="checkbox" name="g_{id}" {checked} {disabled}><span class="name">{label}</span><span class="slug">{slug}</span></label><button type="submit" name="remove_id" value="{id}" formaction="/remove" class="remove" title="Remove" aria-label="Remove {label}">×</button></div>"#,
        id = html_escape(&g.id),
        checked = checked,
        disabled = disabled_attr,
        label = html_escape(&g.label),
        slug = html_escape(slug),
    )
}

fn render_panel(stream: TcpStream, st: &ServerState, msg: Option<&str>) {
    let cfg = st.cfg.read().unwrap();
    let banner = match msg {
        Some("setup_ok") => Some(("ok", "2FA locked. Changes require a code.")),
        Some("ok") => Some(("ok", "Saved.")),
        Some("added") => Some(("ok", "Game added.")),
        Some("removed") => Some(("ok", "Game removed.")),
        Some("bad_input") => Some(("err", "Name and URL pattern are required.")),
        Some("bad_code") => Some(("err", "Wrong code. Try again.")),
        _ => None,
    };
    let banner_html = banner
        .map(|(c, t)| format!(r#"<div class="banner {c}">{}</div>"#, html_escape(t)))
        .unwrap_or_default();

    let total_blocked = active_pattern_count(&cfg);
    let state_sub = if cfg.enabled {
        format!("<b>{}</b> blocked · monitoring all browsers", total_blocked)
    } else {
        "Paused — no blocking".to_string()
    };
    let enabled_attr = if cfg.enabled { "checked" } else { "" };

    let mut sections = String::new();
    for c in &cfg.casinos {
        let casino_games: Vec<&config::GameEntry> =
            cfg.games.iter().filter(|g| g.casino == c.id).collect();
        let blocked_count = casino_games.iter().filter(|g| g.blocked).count();
        let count_label = if c.blocked_all {
            "all blocked".to_string()
        } else if blocked_count > 0 {
            format!("{blocked_count} blocked")
        } else if casino_games.is_empty() {
            "no games".to_string()
        } else {
            format!("{} games", casino_games.len())
        };
        let checked_all = if c.blocked_all { "checked" } else { "" };
        let open_attr = if c.blocked_all || blocked_count > 0 {
            "open"
        } else {
            ""
        };

        let mut rows = String::new();
        for g in &casino_games {
            rows.push_str(&render_game_row(g, c.blocked_all));
        }
        if casino_games.is_empty() {
            rows.push_str(
                r#"<div class="empty">No games yet. Add one with the form below.</div>"#,
            );
        }

        sections.push_str(&format!(
            r#"<details class="casino" {open}>
  <summary>
    <span class="chev">▸</span>
    <span class="cname">{label}</span>
    <span class="domain">{domain}</span>
    <span class="count">{count}</span>
    <label class="allswitch" title="Block all {label}" onclick="event.stopPropagation()">
      <input type="checkbox" name="c_{id}_all" {checked_all}>
      <span class="track"><span class="thumb"></span></span>
    </label>
  </summary>
  <div class="list">{rows}</div>
</details>"#,
            open = open_attr,
            id = html_escape(&c.id),
            label = html_escape(&c.label),
            domain = html_escape(&c.domain),
            count = html_escape(&count_label),
            checked_all = checked_all,
            rows = rows,
        ));
    }

    let custom_games: Vec<&config::GameEntry> =
        cfg.games.iter().filter(|g| g.casino.is_empty()).collect();
    let mut custom_rows = String::new();
    for g in &custom_games {
        custom_rows.push_str(&render_game_row(g, false));
    }
    let custom_section = if custom_games.is_empty() {
        String::new()
    } else {
        format!(
            r#"<details class="casino" open>
  <summary>
    <span class="chev">▸</span>
    <span class="cname">Custom</span>
    <span class="domain">user-added</span>
    <span class="count">{count} {word}</span>
  </summary>
  <div class="list">{rows}</div>
</details>"#,
            count = custom_games.len(),
            word = if custom_games.len() == 1 { "game" } else { "games" },
            rows = custom_rows,
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
        r#"<form method="POST" action="/save" class="panel">
  <div class="head">
    <div class="info">
      <h1>BetWall</h1>
      <div class="sub">{sub}</div>
    </div>
    <label class="switch" title="Master toggle">
      <input type="checkbox" name="enabled" {enabled}>
      <span class="track"><span class="thumb"></span></span>
    </label>
  </div>
  {banner}
  <div class="casinos">
    {sections}
    {custom_section}
  </div>
  <div class="add">
    <select name="casino" class="select">
      {casino_options}
    </select>
    <input type="text" name="label" placeholder="Game name (e.g. Dice)" autocomplete="off">
    <input type="text" name="url_pattern" placeholder="path or full URL" autocomplete="off">
    <button type="submit" class="ghost" formaction="/add">Add</button>
  </div>
  <div class="actions">
    <input class="code" type="text" name="code" inputmode="numeric" pattern="[0-9]{{6}}" maxlength="6" placeholder="2FA code" autocomplete="one-time-code" autofocus required>
    <button type="submit">Save</button>
    <button type="submit" class="ghost" formaction="/quit">Quit</button>
  </div>
</form>"#,
        sub = state_sub,
        enabled = enabled_attr,
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
