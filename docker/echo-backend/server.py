import http.server
import os

NAME = os.environ.get("BACKEND_NAME", "echo")


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"OK")
            return

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(f'{{"backend":"{NAME}","path":"{self.path}"}}'.encode())

    def log_message(self, fmt, *args):
        pass


if __name__ == "__main__":
    http.server.ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
