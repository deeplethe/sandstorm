# The lightest sandbox a request can wake: a Python HTTP server and nothing
# else. What the ceiling runs fill nodes with, so that every one of them
# can be called afterwards and the count is of sandboxes that answer.
import http.server
import sys

# An argument is the port, for running many on one host network.
port = int(sys.argv[1]) if len(sys.argv) > 1 else 8000


class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("content-length", "3")
        self.end_headers()
        self.wfile.write(b"ok\n")

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("0.0.0.0", port), H).serve_forever()
