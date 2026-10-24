#!/usr/bin/env bash
URL="http://localhost:8080/files"
LIMIT=1048576 # make sure this matches the limit in the server config

# check NAME EXPECTED ACTUAL
check() {
  if [ "$3" = "$1" ] || { [ "$1" = "not413" ] && [ "$3" != "413" ]; }; then
    echo "PASS  $2 (got $3)"
  else
    echo "FAIL  $2 (got $3)"
  fi
}

make_body() { head -c "$1" /dev/zero | tr '\0' 'a' > /tmp/body.txt; }

make_body 10
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST --data-binary @/tmp/body.txt "$URL")
check not413 "small body (10 bytes)" "$code"

make_body $LIMIT
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST --data-binary @/tmp/body.txt "$URL")
check not413 "body exactly at the limit" "$code"

make_body $((LIMIT + 1))
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST --data-binary @/tmp/body.txt "$URL")
check 413 "body 1 byte over the limit" "$code"

make_body 52428800
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST --data-binary @/tmp/body.txt "$URL")
check 413 "huge body (50 MB)" "$code"

make_body $((LIMIT + 1))
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "Transfer-Encoding: chunked" --data-binary @/tmp/body.txt "$URL")
check 413 "chunked body over the limit" "$code"

code=$(curl -s -o /dev/null -w "%{http_code}" -X POST -d "" "$URL")
check not413 "empty body" "$code"

code=$(curl -s -o /dev/null -w "%{http_code}" http://localhost:8080/)
check 200 "server still alive after all that" "$code"