#!/usr/bin/env bash
# Tests the client body size limit (client_max_body_size).
# Start the server first, then run: bash tests/body_limit.sh
# The limit of the "main" server is 1048576 bytes (1 MB).
#
# curl flags used everywhere below:
#   -s                 silent: no progress bar
#   -o /dev/null       throw the response page away
#   -w "%{http_code}\n"  print only the status code (200, 413...)
#   -X POST            send a POST request
#   --data-binary @f   send the content of file f as the request body

echo "1) small body (10 bytes) -> expect: NOT 413"
# WHAT: create a 10 byte body.  HOW: head -c 10 reads 10 bytes from /dev/zero (endless zero bytes).
head -c 10 /dev/zero > /tmp/body.txt
# WHAT: send it to the server.  HOW: curl POSTs the file; the server must not answer 413.
curl -s -o /dev/null -w "%{http_code}\n" -X POST --data-binary @/tmp/body.txt http://localhost:8080/files

echo "2) body exactly 1048576 bytes -> expect: NOT 413"
# WHAT: create a body exactly at the limit.  HOW: same trick, 1048576 bytes.
head -c 1048576 /dev/zero > /tmp/body.txt
# WHAT: the limit itself must be accepted.  HOW: curl sends Content-Length: 1048576, server checks size > limit.
curl -s -o /dev/null -w "%{http_code}\n" -X POST --data-binary @/tmp/body.txt http://localhost:8080/files

echo "3) body 1048577 bytes (1 over the limit) -> expect: 413"
# WHAT: create a body one byte too big.  HOW: 1048576 + 1 bytes.
head -c 1048577 /dev/zero > /tmp/body.txt
# WHAT: the server must refuse it.  HOW: it sees Content-Length > limit and answers 413 Payload Too Large.
curl -s -o /dev/null -w "%{http_code}\n" -X POST --data-binary @/tmp/body.txt http://localhost:8080/files

echo "4) body 50 MB -> expect: 413 (refused from the header, never fully read)"
# WHAT: create a huge body.  HOW: 52428800 bytes = 50 MB.
head -c 52428800 /dev/zero > /tmp/body.txt
# WHAT: the server must refuse without reading all of it.  HOW: the decision comes from the Content-Length header alone.
curl -s -o /dev/null -w "%{http_code}\n" -X POST --data-binary @/tmp/body.txt http://localhost:8080/files

echo "5) chunked body over the limit -> expect: 413"
# WHAT: create a body over the limit.  HOW: 1048577 bytes.
head -c 1048577 /dev/zero > /tmp/body.txt
# WHAT: send it in chunks, with no Content-Length.  HOW: the header Transfer-Encoding: chunked makes curl
#       send the body piece by piece, so the server must count the bytes itself while decoding.
curl -s -o /dev/null -w "%{http_code}\n" -X POST -H "Transfer-Encoding: chunked" --data-binary @/tmp/body.txt http://localhost:8080/files

echo "6) empty body -> expect: NOT 413"
# WHAT: a POST with no body is valid.  HOW: -d "" sends an empty body (Content-Length: 0).
curl -s -o /dev/null -w "%{http_code}\n" -X POST -d "" http://localhost:8080/files

echo "7) server still alive -> expect: 200"
# WHAT: prove the server survived all the refused requests.  HOW: a normal GET on the home page.
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/