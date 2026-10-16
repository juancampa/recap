#!/bin/sh
# Serve index.html on http://127.0.0.1:$PORT and send each /v1/ request to
# the recap server. A browser does not let a page from file:// read the API.
#
# The recap server also sends this page at its own root URL, for example
# http://127.0.0.1:7717/. Use this script when you change index.html, because
# the server contains the copy of the page from its build.
#
# RECAP sets the recap server URL. The default is the prod server.
# PORT sets the port of this server. The default is 8000.
set -eu
RECAP="${RECAP:-http://127.0.0.1:7717}"
PORT="${PORT:-8000}"
cd "$(dirname "$0")"
echo "open http://127.0.0.1:$PORT/ (the API is at $RECAP)"
exec python3 - "$RECAP" "$PORT" <<'PY'
import http.server, sys, urllib.error, urllib.request

recap, port = sys.argv[1].rstrip("/"), int(sys.argv[2])

class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if not self.path.startswith("/v1/"):
            return super().do_GET()
        try:
            response = urllib.request.urlopen(recap + self.path)
        except urllib.error.HTTPError as e:
            response = e
        except OSError as e:
            return self.send_error(502, f"cannot connect to {recap}: {e}")
        self.send_response(response.status)
        for name in ("Content-Type", "Cache-Control"):
            if response.headers.get(name):
                self.send_header(name, response.headers[name])
        self.end_headers()
        # Send each block when it arrives. The event stream does not end.
        try:
            while block := response.read1(65536):
                self.wfile.write(block)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
