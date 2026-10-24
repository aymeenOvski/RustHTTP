#!/usr/bin/env python3
# Reports what the server gave this script, so we can see if the body arrived intact.
import os, sys

# CONTENT_LENGTH is set by the server: how many body bytes to read from stdin.
length = int(os.environ.get("CONTENT_LENGTH") or 0)
body = sys.stdin.buffer.read(length)

print("Content-Type: text/plain")
print()
print("method=" + os.environ.get("REQUEST_METHOD", ""))
print("query=" + os.environ.get("QUERY_STRING", ""))
print("content_length=" + str(length))
print("bytes_read=" + str(len(body)))
if len(body) < 100:
    print("body=" + body.decode(errors="replace"))