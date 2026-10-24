#!/usr/bin/env bash
# Tests the CGI with unchunked and chunked bodies.
# Start the server first, then run FROM THE PROJECT ROOT: bash tests/cgi.sh
# The CGI route is /cgi (root = cgi-bin). Output is printed so you can read it.

echo "1) GET hello.py -> expect: Hello from CGI"
# WHAT: run a simple script.  HOW: the server starts python3 on cgi-bin/hello.py and returns what it prints.
curl -s http://localhost:8080/cgi/hello.py

echo
echo "2) GET with a query string -> expect: query=name=audit"
# WHAT: check the script receives the URL query.  HOW: the server puts it in the QUERY_STRING variable.
curl -s "http://localhost:8080/cgi/echo.py?name=audit"

echo
echo "3) POST unchunked -> expect: method=POST, content_length=5, bytes_read=5, body=hello"
# WHAT: send a normal body.  HOW: --data makes curl send a Content-Length header.
curl -s -X POST --data "hello" http://localhost:8080/cgi/echo.py

echo
echo "4) POST chunked -> expect: exactly the same as test 3"
# WHAT: send the same body in chunks.  HOW: the header Transfer-Encoding: chunked makes curl send
#       the body in pieces with no Content-Length; the server joins them before giving them to the script.
curl -s -X POST -H "Transfer-Encoding: chunked" --data "hello" http://localhost:8080/cgi/echo.py

echo
echo "5) POST big unchunked (500000 bytes) -> expect: content_length=500000, bytes_read=500000"
# WHAT: create a big body.  HOW: head reads 500000 bytes from /dev/zero, tr turns the zeros into letters.
head -c 500000 /dev/zero | tr '\0' 'a' > /tmp/cgi_big.txt
# WHAT: send it.  HOW: a big body does not fit in a small pipe, so this tests that nothing gets stuck or cut.
curl -s -X POST --data-binary @/tmp/cgi_big.txt http://localhost:8080/cgi/echo.py

echo
echo "6) POST big chunked (500000 bytes) -> expect: same numbers as test 5"
curl -s -X POST -H "Transfer-Encoding: chunked" --data-binary @/tmp/cgi_big.txt http://localhost:8080/cgi/echo.py

echo
echo "7) Missing script -> expect: 404"
# WHAT: ask for a script that does not exist.  HOW: the server must answer an error, not crash.
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/cgi/nope.py

echo "8) Server still alive -> expect: 200"
# WHAT: prove the server survived.  HOW: a normal GET on the home page.
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/