#!/usr/bin/env bash
# Tests GET, POST (upload), DELETE and wrong requests.
# Start the server first, then run it FROM THE PROJECT ROOT: bash tests/methods.sh
# Each curl prints the status code. The expected one is written in the echo above it.
#
# curl flags used everywhere below:
#   -s                 silent: no progress bar
#   -o /dev/null       throw the response page away
#   -w "%{http_code}\n"  print only the status code
#   -X METHOD          choose the HTTP method (POST, DELETE, PUT)

# ---------- GET ----------
echo "GET /  -> expect: 200"
# WHAT: ask for the home page.  HOW: plain GET (curl's default method).
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/

echo "GET /files/ (directory listing)  -> expect: 200"
# WHAT: ask for a folder.  HOW: /files has autoindex = true, so the server builds an HTML list.
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/files/

echo "GET missing file  -> expect: 404"
# WHAT: ask for a file that does not exist.  HOW: the server looks on disk, finds nothing, answers 404.
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/files/does_not_exist.txt

echo "GET /old-page  -> expect: 301 (redirect)"
# WHAT: ask for a redirected URL.  HOW: the route has "redirect = /", so the server answers 301 + Location header.
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/old-page

# ---------- POST (upload) ----------
# WHAT: create a small text file to upload.  HOW: echo writes text into a file.
echo "hello audit" > /tmp/test_upload.txt
# WHAT: create a 300 KB file of random bytes.  HOW: head reads /dev/urandom; random data shows any corruption.
head -c 300000 /dev/urandom > /tmp/test_upload.bin

echo "POST upload text file  -> expect: 201"
# WHAT: upload the text file.  HOW: -F sends a multipart/form-data form, like a browser upload form.
curl -s -o /dev/null -w "%{http_code}\n" -F "file=@/tmp/test_upload.txt" http://localhost:8080/files

echo "POST upload binary file  -> expect: 201"
# WHAT: upload the binary file.  HOW: same, with the random file.
curl -s -o /dev/null -w "%{http_code}\n" -F "file=@/tmp/test_upload.bin" http://localhost:8080/files

# ---------- Download and compare (corruption check) ----------
# WHAT: find the saved names.  HOW: the server saves files as <timestamp>_<name> in www/uploads,
#       so ls lists the folder and grep keeps the line containing our file name.
TXT=$(ls www/uploads | grep "test_upload.txt" | head -n1)
BIN=$(ls www/uploads | grep "test_upload.bin" | head -n1)

# WHAT: download both files back.  HOW: GET with -o <file> saves the response body into a file.
curl -s -o /tmp/back.txt http://localhost:8080/files/$TXT
curl -s -o /tmp/back.bin http://localhost:8080/files/$BIN

echo "Text file identical to the original?  -> expect: IDENTICAL"
# WHAT: compare the original with the downloaded copy.
# HOW: cmp -s compares byte by byte and stays silent; its exit code is 0 if identical.
#      "&&" runs the echo IDENTICAL when cmp succeeded, "||" runs DIFFERENT when it failed.
cmp -s /tmp/test_upload.txt /tmp/back.txt && echo "IDENTICAL" || echo "DIFFERENT (file corrupted!)"

echo "Binary file identical to the original?  -> expect: IDENTICAL"
cmp -s /tmp/test_upload.bin /tmp/back.bin && echo "IDENTICAL" || echo "DIFFERENT (file corrupted!)"

# ---------- DELETE ----------
echo "DELETE uploaded file  -> expect: 204"
# WHAT: delete the uploaded file.  HOW: -X DELETE; the server removes the file and answers 204 No Content.
curl -s -o /dev/null -w "%{http_code}\n" -X DELETE http://localhost:8080/files/$TXT

echo "GET deleted file  -> expect: 404"
# WHAT: check it is really gone.  HOW: a GET on the same URL must not find it anymore.
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/files/$TXT

echo "DELETE it again  -> expect: 404"
# WHAT: delete a file that no longer exists.  HOW: the server finds nothing to remove, answers 404.
curl -s -o /dev/null -w "%{http_code}\n" -X DELETE http://localhost:8080/files/$TXT

echo "DELETE on /  -> expect: 405"
# WHAT: DELETE on a route that does not allow it.  HOW: route / has "methods = GET", so the server answers 405.
curl -s -o /dev/null -w "%{http_code}\n" -X DELETE http://localhost:8080/

# WHAT: cleanup, remove the binary test file.  HOW: DELETE again, output ignored (no -w).
curl -s -o /dev/null -X DELETE http://localhost:8080/files/$BIN

# ---------- WRONG requests ----------
echo "PUT (unsupported method)  -> expect: 405"
# WHAT: use a method the server does not handle.  HOW: -X PUT; the server refuses it with 405.
curl -s -o /dev/null -w "%{http_code}\n" -X PUT http://localhost:8080/

echo "Path traversal ../..  -> expect: 403"
# WHAT: try to escape the web folder.  HOW: --path-as-is stops curl from cleaning the ".."; the server must refuse.
curl -s -o /dev/null -w "%{http_code}\n" --path-as-is http://localhost:8080/files/../../Cargo.toml

echo "Garbage, not HTTP  -> expect: HTTP/1.1 400 Bad Request"
# WHAT: send text that is not HTTP at all.  HOW: printf writes it, nc (netcat) sends it over a raw TCP
#       connection, timeout 2 stops waiting, head -n1 shows only the first line of the answer.
printf 'GARBAGE\r\n\r\n' | timeout 2 nc localhost 8080 | head -n1

echo "Server still alive  -> expect: 200"
# WHAT: prove the server survived the wrong requests.  HOW: a normal GET on the home page.
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/