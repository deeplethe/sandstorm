# A sandbox that holds a heap and answers HTTP, the way an agent's
# interpreter holds its imports and state. Each request reads one value
# from every page of the heap, so a request needs the whole working set.
import http.server
import sys

import numpy as np

mib = int(sys.argv[1])
heap = np.ones(mib * 1024 * 1024 // 8) if mib else np.ones(1)


class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = f"{float(heap[::512].sum())}\n".encode()
        self.send_response(200)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[2])), H).serve_forever()
