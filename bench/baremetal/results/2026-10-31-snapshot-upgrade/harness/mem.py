# Holds state in memory, so a restore can be told from a fresh start.
import http.server, os, sys
boot = os.urandom(4).hex()
count = 0
blob = bytearray(os.urandom(1 << 22))
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        global count
        count += 1
        body = f"{boot} {count} {sum(blob[::4096]) % 997}\n".encode()
        self.send_response(200); self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
http.server.ThreadingHTTPServer(("0.0.0.0", 8000), H).serve_forever()
