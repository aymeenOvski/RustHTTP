# LocalServer

A small HTTP/1.1 server written in **Rust** from scratch. One process, one thread, non-blocking I/O with [`mio`](https://docs.rs/mio). No web frameworks, no async runtimes.

## Features

- `GET`, `POST`, `DELETE`
- Multiple servers and ports, plus virtual hosts (chosen by the `Host` header)
- Static files, default index file, optional directory listing
- Custom error pages (400, 403, 404, 405, 413, 500)
- Redirections and per-route allowed methods
- Chunked and unchunked request bodies, keep-alive, `100-continue`
- File uploads (multipart)
- Cookies and sessions
- CGI (Python), run without ever blocking the event loop
- Timeouts (slow clients, stalled writes, long CGI scripts)
- A panic in one request closes only that connection, not the server

## Quick start

```bash
cargo run --release                 # uses ./config.conf
cargo run --release -- my.conf      # or pass your own config
cargo test                          # run the unit tests
```

Requirements: Rust (edition 2024) and `python3` at `/usr/bin/python3` if you use CGI.

## Project layout

```
src/
├── main.rs           entry point: load config, start server
├── config.rs         config parsing + validation
├── server.rs         event loop, listeners, virtual hosts, timeouts
├── connection.rs     one client: buffers, state machine, timeouts
├── http/             request parser (incl. chunked) and response serializer
├── router.rs         picks the route, checks methods, dispatches
├── static_files.rs   files, index, directory listing, DELETE
├── upload.rs         multipart uploads
├── cgi.rs            CGI processes
├── error.rs          error responses
└── utils/            cookies and sessions
www/  cgi-bin/  error_pages/  config.conf
```

## Configuration

The config is a simple INI-like file with two kinds of sections:

- `[server.<name>]`: a server (host, ports, limits, error pages)
- `[route.<server name>.<path>]`: a route that belongs to that server

Lines starting with `#` are comments. Paths are relative to where you run the server.

### Example

```ini
# ==========================================
# SERVER 1: Main Web & Static Server
# ==========================================
[server.main]
host = 127.0.0.1
ports = 8080, 8090
server_names = localhost, main.local
is_default = true
client_max_body_size = 1048576

# Custom error pages mapped to static files
error_400 = error_pages/400.html # Bad Request
error_403 = error_pages/403.html
error_404 = error_pages/404.html
error_405 = error_pages/405.html
error_413 = error_pages/413.html
error_500 = error_pages/500.html

# Home route (Root directory)
[route.main./]
root = www
methods = GET
index = index.html
autoindex = false

# Uploads and file management route (Supports DELETE and uploads)
[route.main./files]
root = www/uploads
upload_dir = www/uploads
methods = GET, POST, DELETE
autoindex = true 

# Redirection route
[route.main./old-page]
redirect = /

# CGI script execution route (.py extension)
[route.main./cgi]
root = cgi-bin
methods = GET, POST
cgi_extension = .py


# ==========================================
# SERVER 2: Secondary API / Testing Server (Shares Port 8080 via Virtual Hosting)
# ==========================================
[server.api]
host = 127.0.0.1
ports = 8080
server_names = test.com, api.local
is_default = false
client_max_body_size = 5242880

# Custom error pages mapped to static files
error_400 = error_pages/400.html
error_403 = error_pages/403.html
error_404 = error_pages/404.html
error_405 = error_pages/405.html
error_413 = error_pages/413.html
error_500 = error_pages/500.html

# Home route (Root directory)
[route.api./]
root = www
methods = GET
index = index.html
autoindex = false

# Uploads and file management route (Supports DELETE and uploads)
[route.api./files]
root = www/uploads
upload_dir = www/uploads
methods = GET, POST, DELETE
autoindex = true 

# Redirection route
[route.api./old-page]
redirect = /

# CGI script execution route (.py extension)
[route.api./cgi]
root = cgi-bin
methods = GET, POST
cgi_extension = .py
```

Here `main` and `small` share port 8080. The server tells them apart by the `Host` header.

### Server options

| Key | Meaning | Default |
|---|---|---|
| `host` | Address to listen on (`127.0.0.1`, `0.0.0.0`, or a name) | - |
| `ports` | Comma-separated list of ports | - |
| `server_names` | Host names this server answers to | none |
| `is_default` | Used when no name matches the `Host` header | `false` |
| `client_max_body_size` | Max request body in bytes (→ `413` if exceeded) | `1000000` |
| `error_<code>` | Custom page for a status, e.g. `error_404 = error_pages/404.html` | built-in page |

### Route options

| Key | Meaning |
|---|---|
| `root` | Folder the route serves files from |
| `methods` | Allowed methods (`GET, POST, DELETE`). Others get `405` |
| `index` | Default file for a directory (falls back to `index.html`) |
| `autoindex` | `true` shows a directory listing when there is no index file, otherwise `403` |
| `redirect` | Answers `301` with this `Location` |
| `upload_dir` | Where `POST` multipart uploads are saved |
| `cgi_extension` | Files with this extension are run as CGI (e.g. `.py`, runs with `/usr/bin/python3`) |

The **longest matching route prefix** wins. `/docs` matches `/docs` and `/docs/a.txt`, but not `/docsx`.

### What is validated at startup

The server refuses to start (exit code 1, with a clear message) when:

- a port is invalid (`abc`, `0`, `70000`, ...)
- the same port **and** the same server name appear twice
- `client_max_body_size` is `0`, invalid, or above 1 GiB
- a custom error page file does not exist
- a route `root` folder does not exist
- no server is defined

Sharing a port is fine as long as the server names differ (virtual hosts). If one address cannot be bound at runtime (for example the port is taken), it is skipped with a message and the other servers keep running.

## Testing

### Unit tests

```bash
cargo test
```

They cover:

| Area | Examples |
|---|---|
| HTTP parsing | request line, headers, bad requests (400), unsupported methods (405), chunked bodies, size limits (413) |
| Config validation | duplicate ports, invalid ports and body sizes, missing roots and error pages, virtual hosts |
| Route matching | longest prefix, trailing slashes, redirects, CGI routes, 301/404/405 results |
| Responses | status line, `Content-Length`, no body on 204, header-injection protection |
| Connections | timeout rules (idle, slowloris, stalled write, draining) |
| CGI | environment, output parsing, timeouts, output limit, script outside root refused |
| Sessions | unique ids, lookup, expiry |

### Manual tests with curl

Start the server with the example config, then try:

```bash
# Static page and 404
curl -i http://localhost:8080/
curl -i http://localhost:8080/nope

# Session cookie (look for Set-Cookie on the first response)
curl -i http://localhost:8080/

# Virtual hosts: same IP and port, different names
curl --resolve small.local:8080:127.0.0.1 http://small.local:8080/

# Body limit: server "small" accepts at most 100 bytes → 413
curl -i -X POST -H "Host: small.local" --data "$(head -c 200 /dev/zero | tr '\0' a)" http://localhost:8080/

# Method not allowed → 405
curl -i -X DELETE http://localhost:8080/

# Redirect → 301
curl -i http://localhost:8080/old

# Upload a file, list it, download it, delete it
curl -i -F "file=@photo.png" http://localhost:8080/upload
curl http://localhost:8080/upload/
curl -i -X DELETE http://localhost:8080/upload/<saved-file-name>

# Chunked request
curl -i -H "Transfer-Encoding: chunked" --data "hello" http://localhost:8080/cgi-bin/hello.py

# CGI
curl -i http://localhost:8080/cgi-bin/hello.py

# Malformed request: the server must stay alive
printf 'GARBAGE\r\n\r\n' | nc localhost 8080
curl -i http://localhost:8080/
```

Uploaded files are saved as `<timestamp>_<original name>` so they never overwrite each other.

**Config error check:** copy the example config, make two servers use the same port with the same `server_names`, and start the server. It must stop with a `Configuration Error` message.

### Stress test

```bash
siege -b http://127.0.0.1:8080/
```

Target: availability of at least 99.5%. While it runs, watch memory with `top` and check for leaked sockets or processes afterwards:

```bash
ls /proc/$(pgrep localserver)/fd | wc -l
```

> Only run `siege` against servers you own or have permission to test.

## Design notes

- **One event loop.** A single `mio::Poll` watches listeners, clients and CGI pipes. Every read and write is driven by an event, and each connection is advanced until its socket returns `WouldBlock`.
- **State machine per client:** `Reading → Writing → (keep-alive) Reading`, with `WaitingCgi` and `Draining` for CGI and graceful close.
- **Never blocking on CGI.** Script input and output are non-blocking pipes registered in the same poll. A script running more than 10 seconds gets a `504`.
- **Safe by default.** Paths are checked against `..` and symlinks leaving the root, CGI starts with an empty environment, and header values are stripped of CR/LF.

## Limitations

- Only one CGI type is supported (`.py`, with `/usr/bin/python3`).
- No admin dashboard or metrics endpoint.
- Unix only (uses `mio::unix::pipe`).